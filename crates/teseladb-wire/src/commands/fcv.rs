//! The cluster's feature compatibility version (007 FR-027): `setFeatureCompatibilityVersion`
//! keeps it in Group 0, as an internal cluster parameter, with the checks and transitions of
//! RefDb 9.0.4 (measured), and refuses a version above the one some node runs, so that a cluster
//! upgraded node by node turns new features on once every node has them.

use std::collections::BTreeMap;

use teseladb_bson::{Document, RawDoc, Value};

use super::admin::admin_only;
use super::{Backend, Ctx};
use crate::error::{
    CANNOT_DOWNGRADE, CommandError, FCV_BAD_TRANSITION, FCV_NOT_CONFIRMED, IDL_UNKNOWN_FIELD,
    ILLEGAL_OPERATION, INVALID_FCV,
};

/// `setFeatureCompatibilityVersion`.
pub const SET_FCV: &str = "setFeatureCompatibilityVersion";
/// The cluster parameter that holds the version (a `refrouter` 9.0.4 has no such `getParameter`).
pub const FCV: &str = "featureCompatibilityVersion";
/// The versions this binary runs, latest first: the latest, the last continuous and the last
/// long-term release (measured on 9.0.4).
pub const VERSIONS: [&str; 3] = ["9.0", "8.3", "8.0"];
const LATEST: &str = VERSIONS[0];
const LAST_LTS: &str = VERSIONS[2];
const VERSION: &str = "version";
const CONFIRM: &str = "confirm";
/// What a router passes along to the config server, which takes it (measured).
const FROM_CONFIG_SERVER: &str = "fromConfigServer";
/// The types `confirm` takes, in a `refrouter` 9.0.4's order.
const CONFIRM_TYPES: [&str; 5] = ["int", "decimal", "bool", "double", "long"];

/// The version among the cluster parameters `all`: the latest until it is set.
pub fn version_of(all: &BTreeMap<String, Vec<u8>>) -> String {
    all.get(FCV)
        .and_then(|b| RawDoc::from_bytes(b).ok())
        .map(Document::from)
        .and_then(|d| match d.get(VERSION) {
            Some(Value::String(v)) => Some(v.clone()),
            _ => None,
        })
        .unwrap_or_else(|| LATEST.to_owned())
}

/// Where a version sits in [`VERSIONS`], latest first.
fn rank(v: &str) -> Option<usize> {
    VERSIONS.iter().position(|x| *x == v)
}

/// `setFeatureCompatibilityVersion`.
pub fn set_fcv<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    admin_only(SET_FCV, ctx.db)?;
    let target = match ctx.first() {
        Value::String(s) => s.clone(),
        v => return Err(crate::types::wrong_type(&format!("{SET_FCV}.{SET_FCV}"), v, "string")),
    };
    let mut confirmed = false;
    for (k, v) in ctx.cmd.iter().skip(1) {
        match (k, v) {
            (CONFIRM, Value::Boolean(b)) => confirmed = *b,
            (
                CONFIRM,
                Value::Int32(_) | Value::Int64(_) | Value::Double(_) | Value::Decimal128(_),
            ) => confirmed = crate::types::number(v).is_some_and(|n| n != 0.0),
            (CONFIRM, v) => {
                let path = format!("{SET_FCV}.{CONFIRM}");
                return Err(crate::types::wrong_types(&path, v, &CONFIRM_TYPES));
            }
            (FROM_CONFIG_SERVER, _) => {}
            (k, _) if crate::generic::GENERIC_ARGUMENTS.contains(&k) => {}
            (k, _) => {
                let msg = format!("BSON field '{SET_FCV}.{k}' is an unknown field.");
                return Err(CommandError::new(IDL_UNKNOWN_FIELD, msg));
            }
        }
    }
    let Some(to) = rank(&target) else {
        let listed = VERSIONS.iter().map(|v| format!("'{v}'")).collect::<Vec<_>>().join(", ");
        let msg = format!(
            "Invalid feature compatibility version value '{target}'. Expected one of the following versions: '{listed}. See https://docs.refdb.com/master/release-notes/{LAST_LTS}-compatibility/#feature-compatibility."
        );
        return Err(CommandError::new(INVALID_FCV, msg));
    };
    let current = version_of(&ctx.backend.cluster_parameters()?);
    let from = rank(&current).unwrap_or(0);
    if !confirmed {
        let way = if to < from { "upgrade" } else { "downgrade" };
        let msg = format!(
            "To proceed with the FCV {way}, please re-run this command with 'confirm: true'."
        );
        return Err(CommandError::new(FCV_NOT_CONFIRMED, msg));
    }
    // From a version below the latest only to the latest or to itself (measured: 8.0 to 8.3 is
    // refused, 9.0 to 8.3 goes).
    if from != 0 && to != 0 && to != from {
        let msg = format!(
            "cannot set featureCompatibilityVersion to '{target}' while featureCompatibilityVersion is '{current}'"
        );
        return Err(CommandError::new(FCV_BAD_TRANSITION, msg));
    }
    // A node that runs an older binary cannot take the new features yet.
    if let Some(limit) = ctx.backend.fcv_limit()?
        && rank(&limit).is_some_and(|l| to < l)
    {
        let msg = format!(
            "cannot set featureCompatibilityVersion to '{target}': a node of the cluster runs up to '{limit}'"
        );
        return Err(CommandError::new(ILLEGAL_OPERATION, msg));
    }
    if to > from {
        check_downgrade(ctx.backend, from, to)?;
    }
    let stored = Document::new().with(VERSION, target.as_str()).to_bytes()?;
    ctx.backend.set_cluster_parameter(FCV, stored)?;
    Ok(Document::new().with("ok", 1.0))
}

/// The first version, as its rank in [`VERSIONS`], with `validationAction: "errorAndLog"` (8.1:
/// 9.0 and 8.3 have it, 8.0 does not).
const ERROR_AND_LOG_FROM: usize = 1;
/// The first version with the validation level `constraint` and
/// `prepareConstraintValidationLevel` (9.0).
const CONSTRAINT_FROM: usize = 0;

/// Refuses a downgrade from rank `from` to rank `to` while a collection uses an option the target
/// version lacks, naming the first one, as RefDb r9.0.4's `userCollectionsUassertsForDowngrade`
/// (`legacy_fcv_step.cpp`) does: for each collection the action, then the level, then the
/// preparation of the level.
fn check_downgrade<B: Backend + ?Sized>(
    backend: &mut B,
    from: usize,
    to: usize,
) -> Result<(), CommandError> {
    let lost = |first: usize| from <= first && to > first;
    let (error_and_log, constraint) = (lost(ERROR_AND_LOG_FROM), lost(CONSTRAINT_FROM));
    if !error_and_log && !constraint {
        return Ok(());
    }
    for (ns, c) in backend.collections()? {
        let options = Document::from(RawDoc::from_bytes(&c.options)?);
        if options.get("viewOn").is_some() {
            continue;
        }
        let is = |k: &str, v: &str| options.get(k) == Some(&Value::from(v));
        let found = if error_and_log && is("validationAction", "errorAndLog") {
            Some(("'errorAndLog' validation action", "'errorAndLog' enabled"))
        } else if constraint && is("validationLevel", "constraint") {
            Some(("'constraint' validation level", "'constraint' enabled"))
        } else if constraint
            && options.get("prepareConstraintValidationLevel") == Some(&Value::Boolean(true))
        {
            Some(("prepareConstraintValidationLevel set", "prepareConstraintValidationLevel set"))
        } else {
            None
        };
        if let Some((with, detected)) = found {
            let msg = format!(
                "Cannot downgrade the cluster when there are collections with {with}. Please unset the option or drop the collection(s) before downgrading. First detected collection with {detected}: {}.{} (UUID: {}).",
                ns.db,
                ns.coll,
                super::uuid_text(&c.uuid)
            );
            return Err(CommandError::new(CANNOT_DOWNGRADE, msg));
        }
    }
    Ok(())
}
