//! The collation a pipeline compares strings with (010 FR-014): the command's, or the view's
//! it reads, shared by every scope and stage of the pipeline.

use std::fmt;
use std::rc::Rc;

use teseladb_bson::Collator;

/// A collation, or none (the binary order of strings).
#[derive(Clone, Default)]
pub struct Collation(Option<Rc<dyn Collator>>);

impl Collation {
    /// The collation that compares with `collator`.
    pub fn new(collator: Rc<dyn Collator>) -> Collation {
        Collation(Some(collator))
    }

    /// Its collator, `None` for the binary order.
    pub fn get(&self) -> Option<&dyn Collator> {
        self.0.as_deref()
    }
}

impl fmt::Debug for Collation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(if self.0.is_some() { "Collation(collator)" } else { "Collation(simple)" })
    }
}
