//! Update paths: their segments, the positional operators `$`, `$[]` and `$[<id>]` with
//! `arrayFilters`, and the walk that reaches every place an operator changes.

use std::cmp::Ordering;

use teseladb_bson::{Document, RawArray, RawValue, Value};

use super::tree::{MDoc, MVal};
use crate::error::{MqlError, Result, bad_value, codes, failed_to_parse, type_mismatch};
use crate::matcher::{Filter, MatchContext, array_index};
use crate::parse::FilterUse;
use crate::value::OwnedDoc;

/// A segment of an update path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Seg {
    /// A field name, or an array position when it is made of digits.
    Name(String),
    /// `$`.
    Positional,
    /// `$[]`.
    All,
    /// `$[<id>]`.
    Filtered(String),
}

/// A parsed update path.
#[derive(Clone, Debug)]
pub(crate) struct UpdatePath {
    /// As written.
    pub text: String,
    pub segs: Vec<Seg>,
}

impl UpdatePath {
    /// Reads `text`; `$`-prefixed names other than the positional operators are refused.
    pub(crate) fn parse(text: &str) -> Result<UpdatePath> {
        if text.is_empty() {
            return Err(MqlError::new(
                codes::EMPTY_FIELD_NAME,
                "An empty update path is not valid.",
            ));
        }
        let mut segs = Vec::new();
        for s in text.split('.') {
            if s.is_empty() {
                return Err(MqlError::new(
                    codes::EMPTY_FIELD_NAME,
                    format!(
                        "The update path '{text}' contains an empty field name, which is not allowed."
                    ),
                ));
            }
            segs.push(match s {
                "$" => Seg::Positional,
                "$[]" => Seg::All,
                s if s.starts_with("$[") && s.ends_with(']') => {
                    Seg::Filtered(s[2..s.len() - 1].to_owned())
                }
                s => Seg::Name(s.to_owned()),
            });
        }
        Ok(UpdatePath { text: text.to_owned(), segs })
    }

    /// Whether the path has a positional operator.
    pub(crate) fn dynamic(&self) -> bool {
        self.segs.iter().any(|s| !matches!(s, Seg::Name(_)))
    }

    /// The identifiers of `$[<id>]` in the path.
    pub(crate) fn identifiers(&self) -> impl Iterator<Item = &str> {
        self.segs.iter().filter_map(|s| match s {
            Seg::Filtered(id) => Some(id.as_str()),
            _ => None,
        })
    }
}

/// RefDb's order of update paths: segment by segment, names by their bytes and numeric names
/// by their value.
pub(crate) fn path_order(a: &str, b: &str) -> Ordering {
    let mut sa = a.split('.');
    let mut sb = b.split('.');
    loop {
        match (sa.next(), sb.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) => {
                let o = match (array_index(x), array_index(y)) {
                    (Some(i), Some(j)) => i.cmp(&j),
                    _ => x.as_bytes().cmp(y.as_bytes()),
                };
                if o != Ordering::Equal {
                    return o;
                }
            }
        }
    }
}

/// Whether `prefix` is `path` or one of its ancestors.
pub(crate) fn is_prefix(prefix: &str, path: &str) -> bool {
    path == prefix || path.strip_prefix(prefix).is_some_and(|rest| rest.starts_with('.'))
}

/// The array filters of an update, by identifier.
#[derive(Debug, Default)]
pub(crate) struct ArrayFilters {
    pub filters: Vec<(String, Filter)>,
}

impl ArrayFilters {
    /// Reads `arrayFilters`.
    pub(crate) fn parse(list: Option<RawArray<'_>>) -> Result<ArrayFilters> {
        let mut filters: Vec<(String, Filter)> = Vec::new();
        for item in list.iter().flat_map(|a| a.iter()) {
            let RawValue::Document(d) = item else {
                return Err(type_mismatch("Each array filter must be an object"));
            };
            let mut id: Option<&str> = None;
            for (k, _) in d.iter() {
                let top = k.split('.').next().unwrap_or(k);
                match id {
                    None => id = Some(top),
                    Some(prev) if prev != top => {
                        return Err(failed_to_parse(format!(
                            "Error parsing array filter :: caused by :: Expected a single top-level field name, found '{prev}' and '{top}'"
                        )));
                    }
                    Some(_) => {}
                }
            }
            let Some(id) = id else {
                return Err(failed_to_parse(
                    "Cannot use an expression without a top-level field name in arrayFilters",
                ));
            };
            let valid = id.starts_with(|c: char| c.is_ascii_lowercase())
                && id.chars().all(|c| c.is_ascii_alphanumeric());
            if !valid {
                return Err(bad_value(format!(
                    "Error parsing array filter :: caused by :: The top-level field name must be an alphanumeric string beginning with a lowercase letter, found '{id}'"
                )));
            }
            if filters.iter().any(|(i, _)| i == id) {
                return Err(failed_to_parse(format!(
                    "Found multiple array filters with the same top-level field name {id}"
                )));
            }
            filters.push((id.to_owned(), Filter::parse(d, FilterUse::Nested)?));
        }
        Ok(ArrayFilters { filters })
    }

    fn get(&self, id: &str) -> Option<&Filter> {
        self.filters.iter().find(|(i, _)| i == id).map(|(_, f)| f)
    }

    /// Whether `elem` matches the filter of `id`.
    fn accepts(&self, id: &str, elem: &MVal, ctx: &MatchContext<'_>) -> Result<bool> {
        // Every identifier has its filter: `Update::parse` checked it.
        let wrapper = OwnedDoc::new(&Document::new().with(id, elem.to_value()))?;
        self.get(id).map_or(Ok(false), |filter| filter.matches(wrapper.get(), ctx))
    }
}

/// Where an operator acts: a field of a document or a position of an array.
pub(crate) struct Slot<'a> {
    pub parent: &'a mut MVal,
    pub key: Key,
    /// Name of the parent's field, for messages.
    pub parent_name: &'a str,
}

/// The key of a [`Slot`].
#[derive(Clone, Debug)]
pub(crate) enum Key {
    Name(String),
    Index(usize),
}

impl Slot<'_> {
    pub(crate) fn get(&self) -> Option<&MVal> {
        match (&*self.parent, &self.key) {
            (MVal::Doc(d), Key::Name(n)) => d.get(n),
            (MVal::Arr(a), Key::Index(i)) => a.get(*i),
            (MVal::Arr(a), Key::Name(n)) => array_index(n).and_then(|i| a.get(i)),
            _ => None,
        }
    }

    /// Sets the value, padding an array with nulls up to the position.
    pub(crate) fn set(&mut self, v: MVal) -> Result<()> {
        let name = self.key_text();
        self.parent.unpack();
        match (&mut *self.parent, &self.key) {
            (MVal::Doc(d), _) => {
                d.set(&name, v);
                Ok(())
            }
            (MVal::Arr(a), key) => {
                let i = match key {
                    Key::Index(i) => Some(*i),
                    Key::Name(n) => array_index(n),
                };
                let Some(i) = i else {
                    return Err(not_viable(&name, self.parent_name, &MVal::Arr(a.clone())));
                };
                if i >= a.len() {
                    a.resize(i + 1, MVal::Scalar(Value::Null));
                }
                a[i] = v;
                Ok(())
            }
            (scalar, _) => Err(not_viable(&name, self.parent_name, scalar)),
        }
    }

    /// Removes a field, or sets an array position to null.
    pub(crate) fn unset(&mut self) {
        let name = self.key_text();
        self.parent.unpack();
        match &mut *self.parent {
            MVal::Doc(d) => {
                d.remove(&name);
            }
            MVal::Arr(a) => {
                if let Some(slot) = array_index(&name).and_then(|i| a.get_mut(i)) {
                    *slot = MVal::Scalar(Value::Null);
                }
            }
            MVal::Scalar(_) | MVal::Packed(_) => {}
        }
    }

    pub(crate) fn key_text(&self) -> String {
        match &self.key {
            Key::Name(n) => n.clone(),
            Key::Index(i) => i.to_string(),
        }
    }
}

/// `Cannot create field 'name' in element {parent: value}`.
pub(crate) fn not_viable(name: &str, parent_name: &str, parent: &MVal) -> MqlError {
    let shown = MDoc { fields: vec![(parent_name.to_owned(), parent.clone())] };
    let text = match crate::value::OwnedDoc::new(&shown.to_document()) {
        Ok(d) => crate::fmt::shell_doc(d.get()),
        Err(e) => e.message,
    };
    MqlError::new(codes::PATH_NOT_VIABLE, format!("Cannot create field '{name}' in element {text}"))
}

/// What a walk needs.
pub(crate) struct Walk<'a, 'c> {
    pub filters: &'a ArrayFilters,
    pub ctx: &'a MatchContext<'c>,
    /// The match position for `$`.
    pub position: Option<usize>,
    /// Creates missing documents on the way (`$set`); else a missing path ends the walk.
    pub create: bool,
    /// The whole path, for messages.
    pub text: &'a str,
}

impl Walk<'_, '_> {
    /// Calls `f` with every slot `segs` reaches from `cur`.
    pub(crate) fn run(
        &self,
        cur: &mut MVal,
        name: &str,
        segs: &[Seg],
        f: &mut dyn FnMut(Slot<'_>) -> Result<()>,
    ) -> Result<()> {
        let Some((seg, rest)) = segs.split_first() else { return Ok(()) };
        cur.unpack();
        let seg = match seg {
            Seg::Positional => match self.position {
                Some(p) => Seg::Name(p.to_string()),
                None => {
                    return Err(bad_value(
                        "The positional operator did not find the match needed from the query.",
                    ));
                }
            },
            s => s.clone(),
        };
        match seg {
            Seg::All | Seg::Filtered(_) => {
                let MVal::Arr(items) = cur else {
                    return Err(bad_value(format!(
                        "Cannot apply array updates to non-array element {name}: {}",
                        cur.owned().map(|o| crate::fmt::shell(o.get())).unwrap_or_default()
                    )));
                };
                let mut chosen = Vec::new();
                for (i, e) in items.iter().enumerate() {
                    let ok = match &seg {
                        Seg::Filtered(id) => self.filters.accepts(id, e, self.ctx)?,
                        _ => true,
                    };
                    if ok {
                        chosen.push(i);
                    }
                }
                if rest.is_empty() {
                    for i in chosen {
                        f(Slot { parent: cur, key: Key::Index(i), parent_name: name })?;
                    }
                    return Ok(());
                }
                for i in chosen {
                    let label = i.to_string();
                    self.run(&mut items[i], &label, rest, f)?;
                }
                Ok(())
            }
            Seg::Name(n) => {
                if rest.is_empty() {
                    return f(Slot { parent: cur, key: Key::Name(n), parent_name: name });
                }
                let needs_array = matches!(rest.first(), Some(Seg::All | Seg::Filtered(_)));
                let child = self.child(cur, &n, name, needs_array)?;
                match child {
                    Some(c) => self.run(c, &n, rest, f),
                    None => Ok(()),
                }
            }
            Seg::Positional => Ok(()),
        }
    }

    /// The child `n` of `cur`, created as an empty document when the walk creates.
    fn child<'m>(
        &self,
        cur: &'m mut MVal,
        n: &str,
        name: &str,
        needs_array: bool,
    ) -> Result<Option<&'m mut MVal>> {
        let missing = || {
            bad_value(format!(
                "The path '{}' must exist in the document in order to apply array updates.",
                self.prefix_to(n)
            ))
        };
        match cur {
            MVal::Doc(d) => {
                if d.get(n).is_none() {
                    if needs_array {
                        return Err(missing());
                    }
                    if !self.create {
                        return Ok(None);
                    }
                    d.set(n, MVal::Doc(MDoc::default()));
                }
                Ok(d.get_mut(n))
            }
            MVal::Arr(items) => {
                let Some(i) = array_index(n) else {
                    if self.create {
                        return Err(not_viable(n, name, &MVal::Arr(items.clone())));
                    }
                    return Ok(None);
                };
                if i >= items.len() {
                    if needs_array {
                        return Err(missing());
                    }
                    if !self.create {
                        return Ok(None);
                    }
                    items.resize(i, MVal::Scalar(Value::Null));
                    items.push(MVal::Doc(MDoc::default()));
                }
                Ok(items.get_mut(i))
            }
            MVal::Scalar(_) | MVal::Packed(_) => {
                if self.create {
                    Err(not_viable(n, name, cur))
                } else if needs_array {
                    Err(missing())
                } else {
                    Ok(None)
                }
            }
        }
    }

    /// The written path up to and including the first segment named `n`.
    fn prefix_to(&self, n: &str) -> String {
        let mut out = Vec::new();
        for s in self.text.split('.') {
            out.push(s);
            if s == n {
                break;
            }
        }
        out.join(".")
    }
}
