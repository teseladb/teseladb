//! How `usersInfo` shows the documents of `admin.system.users` (013 FR-030), as the reference
//! server 9.0.4 shows them, whether a user command or a direct write stored them (measured on the
//! reference router, 31-10-2026): the stored document with `mechanisms` set as `$addFields` sets a
//! field (in the place of a stored `mechanisms`, else at the end), without `credentials`,
//! `authenticationRestrictions` and, unless asked, `customData`, sorted by `user` and `db`. With
//! `showPrivileges` or `showAuthenticationRestrictions` a stored `mechanisms` stays, the computed
//! one takes the place of `credentials`, and what the roles give follows.

use teseladb_bson::{Document, RawDoc, Value};

use super::sort::SortSpec;
use crate::error::{CommandError, EXPECTED_OBJECT};

const CREDENTIALS: &str = "credentials";
const MECHANISMS: &str = "mechanisms";
const RESTRICTIONS: &str = "authenticationRestrictions";
const CUSTOM_DATA: &str = "customData";

/// What `usersInfo` was asked to show.
#[derive(Clone, Copy, Debug, Default)]
pub struct Shown {
    /// `showCredentials`.
    pub credentials: bool,
    /// `showCustomData`.
    pub custom: bool,
    /// `showAuthenticationRestrictions`.
    pub restrictions: bool,
}

/// The stored documents in `usersInfo`'s order: by `user`, then `db`, as a sort on those fields
/// orders any values.
pub fn sorted(docs: Vec<Vec<u8>>) -> Result<Vec<Document>, CommandError> {
    let spec = SortSpec::parse(&Document::new().with("user", 1).with("db", 1))?;
    let mut keyed = Vec::new();
    for bytes in docs {
        let raw = RawDoc::from_bytes(&bytes)?;
        keyed.push((spec.keys(raw, None), Document::from(raw)));
    }
    keyed.sort_by(|a, b| spec.compare_keys(&a.0, &b.0, None));
    Ok(keyed.into_iter().map(|(_, d)| d).collect())
}

/// `mechanisms`: the names in `credentials`, `null` without it.
fn mechanisms(doc: &Document) -> Result<Value, CommandError> {
    match doc.get(CREDENTIALS) {
        None => Ok(Value::Null),
        Some(Value::Document(c)) => Ok(Value::Array(c.iter().map(|(k, _)| k.into()).collect())),
        Some(_) => Err(CommandError::new(
            EXPECTED_OBJECT,
            "invalid parameter: expected an object (credentials)",
        )),
    }
}

/// A user as plain `usersInfo` shows it.
pub fn plain(doc: &Document, shown: Shown) -> Result<Document, CommandError> {
    let mechs = mechanisms(doc)?;
    let mut out = Document::new();
    let mut placed = false;
    for (k, v) in doc.iter() {
        match k {
            MECHANISMS => {
                out.push(k, mechs.clone());
                placed = true;
            }
            CREDENTIALS if !shown.credentials => {}
            RESTRICTIONS => {}
            CUSTOM_DATA if !shown.custom => {}
            _ => out.push(k, v.clone()),
        }
    }
    if !placed {
        out.push(MECHANISMS, mechs);
    }
    Ok(out)
}

/// A user as `usersInfo` shows it with what its roles give (`inherited`, already as documents).
pub fn with_inherited(
    doc: &Document,
    shown: Shown,
    inherited: [(&str, Value); 3],
) -> Result<Document, CommandError> {
    let stored = doc.get(MECHANISMS).is_some();
    let mechs = mechanisms(doc)?;
    let mut out = Document::new();
    let mut placed = stored;
    for (k, v) in doc.iter() {
        match k {
            CREDENTIALS => {
                if !stored {
                    out.push(MECHANISMS, mechs.clone());
                    placed = true;
                }
                if shown.credentials {
                    out.push(k, v.clone());
                }
            }
            RESTRICTIONS if !shown.restrictions => {}
            CUSTOM_DATA if !shown.custom => {}
            _ => out.push(k, v.clone()),
        }
    }
    if !placed {
        out.push(MECHANISMS, mechs);
    }
    for (k, v) in inherited {
        out.push(k, v);
    }
    Ok(out)
}
