//! The names SpiderMonkey guesses for anonymous functions, which its stacks show
//! (`frontend/NameFunctions.cpp`): the place the function is written to (`o.m`, `x.k`), after
//! the name of the function around it and a `/` (`v</o.m`), and `<` when the function is only
//! somewhere inside that value (`v<` for a callback in `const v = f(() => …)`). Measured on a
//! `refrouter` 9.0.4 on 02-11.

use std::rc::Rc;

use crate::ast::{Expr, FuncDef, Name};

/// One function read by the parser.
#[derive(Default)]
struct Entry {
    def: Option<Rc<FuncDef>>,
    /// The name of a method or accessor (`m`, `get g`), which its definition does not keep.
    method: Option<String>,
    parent: Option<usize>,
    /// The places it is written to, innermost first, and whether it is their whole value.
    places: Vec<(String, bool, bool)>,
}

/// The functions of one parse and where they are written.
#[derive(Default)]
pub struct Namer {
    entries: Vec<Entry>,
    open: Vec<usize>,
}

impl Namer {
    /// A function starts: the functions read until [`Namer::leave`] are inside it.
    pub fn enter(&mut self) -> usize {
        let parent = self.open.last().copied();
        self.entries.push(Entry { parent, ..Entry::default() });
        let i = self.entries.len() - 1;
        self.open.push(i);
        i
    }

    /// The function `i` ends, with its definition when it was read.
    pub fn leave(&mut self, i: usize, def: Option<&Rc<FuncDef>>) {
        self.open.pop();
        self.entries[i].def = def.cloned();
    }

    /// `def` is a method or accessor called `name`.
    pub fn method(&mut self, def: &Rc<FuncDef>, name: String) {
        let found = self
            .entries
            .iter_mut()
            .rev()
            .find(|e| e.def.as_ref().is_some_and(|d| Rc::ptr_eq(d, def)));
        if let Some(e) = found {
            e.method = Some(name);
        }
    }

    pub fn mark(&self) -> usize {
        self.entries.len()
    }

    /// The functions read since `mark` at this level are written to `place`, whose value is
    /// `value`; `binding` when the place names a function written there whole (ECMA-262's
    /// inferred name).
    pub fn place(&mut self, mark: usize, place: String, binding: bool, value: &Expr) {
        let level = self.open.last().copied();
        for e in self.entries.iter_mut().skip(mark) {
            if e.parent == level
                && let Some(def) = &e.def
            {
                let direct = whole_function(value).is_some_and(|w| Rc::ptr_eq(def, w));
                let whole = is_whole(value, def);
                e.places.push((place.clone(), whole, binding && direct));
            }
        }
    }

    /// Gives each function its guessed name.
    pub fn finish(self) {
        let mut names: Vec<Option<Option<String>>> = vec![None; self.entries.len()];
        for i in 0..self.entries.len() {
            display(&self.entries, &mut names, i);
        }
        for (e, n) in self.entries.iter().zip(names) {
            if let (Some(def), Some(Some(n))) = (&e.def, n)
                && def.name.is_none()
            {
                let _ = def.guessed.set(Name::from(n.as_str()));
            }
        }
    }
}

/// Whether `def` is the value of `e`, also as a branch of `?:` (measured: `v = c ? () => 1 : 0`
/// names the function `v`).
fn is_whole(e: &Expr, def: &Rc<FuncDef>) -> bool {
    match e {
        Expr::At(_, inner) => is_whole(inner, def),
        Expr::Func(d) => Rc::ptr_eq(d, def),
        Expr::Cond(_, a, b) => is_whole(a, def) || is_whole(b, def),
        _ => false,
    }
}

fn whole_function(e: &Expr) -> Option<&Rc<FuncDef>> {
    match e {
        Expr::At(_, inner) => whole_function(inner),
        Expr::Func(d) => Some(d),
        _ => None,
    }
}

/// The name the stack shows for entry `i`.
fn display(entries: &[Entry], memo: &mut Vec<Option<Option<String>>>, i: usize) -> Option<String> {
    if let Some(n) = &memo[i] {
        return n.clone();
    }
    let e = &entries[i];
    let explicit = e
        .def
        .as_ref()
        .and_then(|d| d.name.as_ref())
        .map(|n| n.to_string())
        .or_else(|| e.method.clone());
    let inferred = match e.places.first() {
        Some((p, true, true)) => Some(p.clone()),
        _ => None,
    };
    let out = explicit.or(inferred).or_else(|| {
        let prefix = e.parent.and_then(|p| display(entries, memo, p)).filter(|p| !p.is_empty());
        let parts: Vec<&str> = e.places.iter().rev().map(|(p, ..)| p.as_str()).collect();
        let whole = e.places.first().is_some_and(|(_, w, _)| *w);
        let guess = if whole { parts.join(".") } else { format!("{}<", parts.join(".")) };
        match prefix {
            Some(p) => Some(format!("{p}/{guess}")),
            None if !parts.is_empty() => Some(guess),
            None => None,
        }
    });
    memo[i] = Some(out.clone());
    out
}
