//! Collection validators (FR-022): a filter with query operators and `$jsonSchema`, its
//! `validationLevel` and `validationAction`, `bypassDocumentValidation`, and the `errInfo` of
//! `DocumentValidationFailure` (121) that explains which rules the document breaks.

use teseladb_bson::{Document, RawDoc, Value};

use crate::error::{MqlError, Result, bad_value, codes};
use crate::matcher::{Filter, MatchContext};
use crate::ops;
use crate::parse::FilterUse;
use crate::value::OwnedDoc;

/// `validationLevel`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ValidationLevel {
    /// No validation.
    Off,
    /// Every insert and update.
    #[default]
    Strict,
    /// Inserts and updates of documents that were valid.
    Moderate,
    /// Every insert and update, without bypass; the whole collection was valid when it was set
    /// (9.0).
    Constraint,
}

/// `validationAction`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ValidationAction {
    /// The write fails.
    #[default]
    Error,
    /// The write succeeds and the node logs a warning.
    Warn,
    /// The write fails and the node logs it.
    ErrorAndLog,
}

const LEVELS: [(ValidationLevel, &str); 4] = [
    (ValidationLevel::Off, "off"),
    (ValidationLevel::Strict, "strict"),
    (ValidationLevel::Moderate, "moderate"),
    (ValidationLevel::Constraint, "constraint"),
];

const ACTIONS: [(ValidationAction, &str); 3] = [
    (ValidationAction::Error, "error"),
    (ValidationAction::Warn, "warn"),
    (ValidationAction::ErrorAndLog, "errorAndLog"),
];

fn enumeration(value: &str, field: &str) -> MqlError {
    bad_value(format!("Enumeration value '{value}' for field '{field}' is not a valid value."))
}

impl ValidationLevel {
    /// The level named `name`.
    pub fn parse(name: &str) -> Result<ValidationLevel> {
        LEVELS
            .iter()
            .find(|(_, n)| *n == name)
            .map(|(l, _)| *l)
            .ok_or_else(|| enumeration(name, "validationLevel"))
    }

    /// Its name.
    pub fn name(self) -> &'static str {
        LEVELS.iter().find(|(l, _)| *l == self).map_or("strict", |(_, n)| n)
    }
}

impl ValidationAction {
    /// The action named `name`.
    pub fn parse(name: &str) -> Result<ValidationAction> {
        ACTIONS
            .iter()
            .find(|(_, n)| *n == name)
            .map(|(a, _)| *a)
            .ok_or_else(|| enumeration(name, "validationAction"))
    }

    /// Its name.
    pub fn name(self) -> &'static str {
        ACTIONS.iter().find(|(a, _)| *a == self).map_or("error", |(_, n)| n)
    }
}

/// What a validator decided about a write.
#[derive(Clone, Debug, PartialEq)]
pub enum Verdict {
    /// The document satisfies the validator.
    Valid,
    /// Not validated: level `off`, or `moderate` on a document that was already invalid.
    Skipped,
    /// Not validated: `bypassDocumentValidation`.
    Bypassed,
    /// Invalid with action `warn`: the write goes ahead and the node logs this `errInfo`.
    Warned(Document),
}

/// A collection's validator.
#[derive(Debug)]
pub struct Validator {
    spec: OwnedDoc,
    filter: Filter,
    level: ValidationLevel,
    action: ValidationAction,
}

/// Fields of `errInfo`, each declared once.
pub(crate) mod info {
    pub const FAILING_DOCUMENT_ID: &str = "failingDocumentId";
    pub const DETAILS: &str = "details";
    pub const OPERATOR_NAME: &str = "operatorName";
    pub const SPECIFIED_AS: &str = "specifiedAs";
    pub const REASON: &str = "reason";
    pub const CONSIDERED_VALUE: &str = "consideredValue";
    pub const CONSIDERED_TYPE: &str = "consideredType";
    pub const CLAUSES_NOT_SATISFIED: &str = "clausesNotSatisfied";
    pub const CLAUSES_SATISFIED: &str = "clausesSatisfied";
    pub const INDEX: &str = "index";
    pub const EXPRESSION_RESULT: &str = "expressionResult";
    pub const CONSIDERED_VALUES: &str = "consideredValues";
}

impl Validator {
    /// Reads a validator with its level and action.
    pub fn parse(
        spec: RawDoc<'_>,
        level: ValidationLevel,
        action: ValidationAction,
    ) -> Result<Validator> {
        if level == ValidationLevel::Constraint && action == ValidationAction::Warn {
            return Err(bad_value(
                "validationAction must be 'error' or 'errorAndLog' when validationLevel is 'constraint'",
            ));
        }
        let filter = Filter::parse(spec, FilterUse::Validator)?;
        Ok(Validator { spec: OwnedDoc::new(&Document::from(spec))?, filter, level, action })
    }

    /// The level.
    pub fn level(&self) -> ValidationLevel {
        self.level
    }

    /// The action.
    pub fn action(&self) -> ValidationAction {
        self.action
    }

    /// Checks the new version `doc` of a write; `old` is the version an update replaces.
    /// `DocumentValidationFailure` with `errInfo` when the action is `error` or `errorAndLog`
    /// (the caller logs the latter).
    pub fn check(
        &self,
        doc: RawDoc<'_>,
        old: Option<RawDoc<'_>>,
        bypass: bool,
        ctx: &MatchContext<'_>,
    ) -> Result<Verdict> {
        if self.level == ValidationLevel::Off {
            return Ok(Verdict::Skipped);
        }
        if bypass && self.level != ValidationLevel::Constraint {
            return Ok(Verdict::Bypassed);
        }
        if self.filter.matches(doc, ctx)? {
            return Ok(Verdict::Valid);
        }
        if let (ValidationLevel::Moderate, Some(old)) = (self.level, old)
            && !self.filter.matches(old, ctx)?
        {
            return Ok(Verdict::Skipped);
        }
        let mut err_info = Document::new();
        if let Some(id) = doc.get(ops::ID) {
            err_info.insert(info::FAILING_DOCUMENT_ID, Value::from(id));
        }
        err_info.insert(info::DETAILS, crate::explain::explain(self.spec.get(), doc, ctx, false)?);
        if self.action == ValidationAction::Warn {
            return Ok(Verdict::Warned(err_info));
        }
        let mut e = MqlError::new(codes::DOCUMENT_VALIDATION_FAILURE, "Document failed validation");
        e.info = Some(err_info);
        Err(e)
    }
}
