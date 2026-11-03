//! The global objects a program's calls share (016 SC-004, D-119). Building them is most of the
//! cost of a short call, so a program builds them once and each call gets a copy of the global
//! object over the same constructors and prototypes. RefDb's engine freezes those, so a call
//! cannot change them; any change that still reaches one (an internal slot, a property of an
//! object the freeze does not cover) marks the budget as touched and the next call builds them
//! again, so every call sees the globals of a fresh engine.

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;

use crate::ast::{self, Name};
use crate::interp::{EnvRec, EnvRef, Interp, Limits, R, Realm};
use crate::value::{Budget, Func, Kind, ObjRef, Prop, Val};

/// The global objects of a program, built for one heap limit.
pub(crate) struct Globals {
    /// The constructors and prototypes; its global object is the template of each call's.
    realm: Realm,
    budget: Rc<Budget>,
    /// The bytes the shared objects hold: where each call's count starts.
    baseline: usize,
    symbol_ids: u64,
    /// The properties of the global object.
    globals: Vec<(Name, Prop)>,
    /// The scopes of the shell's functions (`prelude.js`), kept alive for every call and emptied
    /// with the globals.
    scopes: Vec<EnvRef>,
    /// Every shared object, to break their reference cycles when the globals are dropped.
    objects: Vec<ObjRef>,
    /// The global object of the last call, kept when that call did not change it.
    spare: RefCell<Option<ObjRef>>,
}

/// An interpreter over `realm` with the call's state empty.
pub(crate) fn interp<'c>(realm: Realm, budget: Rc<Budget>, limits: Limits) -> Interp<'c> {
    let global_env = Rc::new(RefCell::new(EnvRec { var_scope: true, ..EnvRec::default() }));
    Interp {
        realm,
        budget,
        limits,
        steps: 0,
        depth: 0,
        units: 0,
        in_native: false,
        eval_cache: Vec::new(),
        stack_base: 0,
        last_match: None,
        envs: vec![Rc::downgrade(&global_env)],
        native: Name::from(""),
        operand_failure: None,
        conversion_called: false,
        global_env,
        now_ms: 0.0,
        rng: 0,
        deadline: None,
        symbol_ids: 0,
        undescribed: Vec::new(),
        registry: Vec::new(),
        templates: Vec::new(),
        with_used: false,
        output: Vec::new(),
        last: None,
        pos: ast::Pos::default(),
        strict: false,
        fname: "".into(),
        frames: Vec::new(),
        thrown_at: ast::Pos::default(),
        col_offset: 0,
    }
}

impl Globals {
    /// Builds the global objects for calls with `limits`; the error is that of the build (the
    /// heap limit too small for them).
    pub(crate) fn build(limits: Limits) -> Result<Globals, crate::JsError> {
        let budget = Rc::new(Budget {
            used: std::cell::Cell::new(0),
            limit: limits.heap,
            touched: std::cell::Cell::new(false),
        });
        let realm = crate::builtins::realm(&budget);
        let mut it = interp(realm, budget.clone(), limits);
        let built = crate::builtins::install(&mut it);
        // The scopes that the shell's functions closed over outlive this interpreter.
        let scopes: Vec<EnvRef> = it.envs.drain(1..).filter_map(|w| w.upgrade()).collect();
        let objects = reachable(&it.realm);
        if let Err(c) = built {
            let e = crate::error_of(&mut it, c);
            drop(it);
            teardown(&objects);
            clear_scopes(&scopes);
            return Err(e);
        }
        for o in &objects {
            o.0.shared.set(true);
        }
        let globals = it.realm.global.borrow().props.clone();
        let realm = it.realm.clone();
        let symbol_ids = it.symbol_ids;
        drop(it);
        budget.touched.set(false);
        Ok(Globals {
            realm,
            baseline: budget.used.get(),
            budget,
            symbol_ids,
            globals,
            scopes,
            objects,
            spare: RefCell::new(None),
        })
    }

    /// Whether these globals serve a call with `limits` and no call has changed them.
    pub(crate) fn fits(&self, limits: &Limits) -> bool {
        self.budget.limit == limits.heap && !self.budget.touched.get()
    }

    /// An interpreter for one call: the shared objects and a global object of its own.
    pub(crate) fn enter<'c>(&self, limits: Limits) -> Interp<'c> {
        self.budget.used.set(self.baseline);
        let mut it = interp(self.realm.clone(), self.budget.clone(), limits);
        it.symbol_ids = self.symbol_ids;
        it
    }

    /// Gives `it` its own global object, a copy of the shared one's properties.
    pub(crate) fn fill_global(&self, it: &mut Interp) -> R<()> {
        if let Some(g) = self.spare.borrow_mut().take().filter(|g| g.0.pristine.get()) {
            // The bytes the previous call charged for it, charged to this one.
            it.charge(g.0.charged.get())?;
            it.realm.global = g;
            return Ok(());
        }
        let g = it.alloc(Some(self.realm.object.clone()), Kind::Ordinary)?;
        it.grow(&g, self.globals.len())?;
        let mut props = self.globals.clone();
        for (k, p) in &mut props {
            if &**k == "globalThis" {
                p.value = Val::Obj(g.clone());
            }
        }
        g.borrow_mut().props = props;
        g.0.pristine.set(true);
        it.realm.global = g;
        Ok(())
    }

    /// Keeps the global object of a call that ended without changing it, for the next call.
    /// `obj`, the document of `$where`, is the call's own: it goes, with the bytes it charged.
    pub(crate) fn keep_global(&self, g: ObjRef) {
        if !g.0.pristine.get() {
            return;
        }
        let extra = {
            let mut o = g.0.obj.borrow_mut();
            let before = o.props.len();
            o.props.truncate(self.globals.len());
            before - o.props.len()
        };
        g.0.charged.set(g.0.charged.get() - extra * crate::interp::PROP_BYTES);
        *self.spare.borrow_mut() = Some(g);
    }
}

impl Drop for Globals {
    fn drop(&mut self) {
        // The spare global object holds itself (`globalThis`).
        if let Some(g) = self.spare.borrow_mut().take() {
            g.borrow_mut().props.clear();
        }
        teardown(&self.objects);
        clear_scopes(&self.scopes);
    }
}

/// Empties `scopes`: they and the closures they hold refer to each other.
fn clear_scopes(scopes: &[EnvRef]) {
    for e in scopes {
        let mut e = e.borrow_mut();
        e.vars.clear();
        e.parent = None;
        e.this = None;
        e.home = None;
        e.func = None;
    }
}

/// Empties `objects`: constructors and prototypes refer to each other.
fn teardown(objects: &[ObjRef]) {
    for o in objects {
        let mut b = o.borrow_mut();
        b.props.clear();
        b.proto = None;
        b.kind = Kind::Ordinary;
    }
}

/// Every object reachable from the realm's roots.
fn reachable(realm: &Realm) -> Vec<ObjRef> {
    let mut roots: Vec<ObjRef> = vec![
        realm.global.clone(),
        realm.object.clone(),
        realm.function.clone(),
        realm.array.clone(),
        realm.string.clone(),
        realm.number.clone(),
        realm.boolean.clone(),
        realm.symbol.clone(),
        realm.bigint.clone(),
        realm.error.clone(),
        realm.date.clone(),
        realm.regexp.clone(),
        realm.map.clone(),
        realm.set.clone(),
        realm.iter.clone(),
    ];
    roots.extend(realm.errors.iter().chain(realm.bson.iter()).map(|(_, o)| o.clone()));
    roots.extend(realm.protos.iter().map(|(_, o)| o.clone()));
    let mut seen: HashSet<*const crate::value::ObjCell> = HashSet::new();
    let mut out = Vec::new();
    while let Some(o) = roots.pop() {
        if !seen.insert(Rc::as_ptr(&o.0)) {
            continue;
        }
        let mut next: Vec<Val> = Vec::new();
        {
            let b = o.borrow();
            for (_, p) in &b.props {
                next.push(p.value.clone());
                next.extend(p.getter.iter().chain(p.setter.iter()).cloned().map(Val::Obj));
            }
            next.extend(b.proto.iter().cloned().map(Val::Obj));
            match &b.kind {
                Kind::Array(v) | Kind::Set(v) => next.extend(v.iter().cloned()),
                Kind::Map(v) => next.extend(v.iter().flat_map(|(k, x)| [k.clone(), x.clone()])),
                Kind::Func(Func::Bound(f, this, args)) => {
                    next.push(Val::Obj(f.clone()));
                    next.push(this.clone());
                    next.extend(args.iter().cloned());
                }
                _ => {}
            }
        }
        roots.extend(next.into_iter().filter_map(|v| match v {
            Val::Obj(x) => Some(x),
            _ => None,
        }));
        out.push(o);
    }
    out
}
