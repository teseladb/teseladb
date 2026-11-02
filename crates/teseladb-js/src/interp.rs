//! The interpreter: it walks the syntax tree with environments of bindings, counts steps,
//! call depth and heap bytes against the invocation's limits, and checks the deadline.

use std::cell::RefCell;
use std::rc::{Rc, Weak};

use crate::ast::*;
use crate::number;
use crate::value::*;

/// Why evaluation stops early.
#[derive(Debug)]
pub enum Ctrl {
    Throw(Val),
    Return(Val),
    Break(Option<Name>),
    Continue(Option<Name>),
    /// A limit that ends the invocation and cannot be caught.
    Fatal(Fatal),
    /// A generator's `yield`: unwinds its body to `next`'s caller (see `generators.rs`).
    Suspend,
}

/// The limits that end an invocation.
#[derive(Clone, Debug, PartialEq)]
pub enum Fatal {
    OutOfMemory,
    Deadline,
    Steps,
    /// A value without a BSON form, with RefDb's message.
    Convert(String),
    /// What the engine of RefDb 9.0 cannot run and ends the call with a trap of its
    /// WebAssembly build: a promise job (`then`, `await`) or a `FinalizationRegistry`.
    Trap,
    /// A function the engine of RefDb 9.0 declares and cannot run (`version()`): its
    /// WebAssembly build reaches an `unreachable` instruction.
    Unreachable,
}

pub type R<T = Val> = Result<T, Ctrl>;

/// A binding.
#[derive(Clone, Debug)]
pub struct Binding {
    pub value: Option<Val>,
    pub mutable: bool,
    /// A `let`, `const`, `class` or a parameter of a list with expressions: a `var` of a direct
    /// `eval` may not redeclare it.
    pub lexical: bool,
}

/// A scope: its bindings, its parent, and for a function scope its `this`, home object and
/// function.
#[derive(Debug, Default)]
pub struct EnvRec {
    pub vars: Vec<(Name, Binding)>,
    /// The object of a `with` statement, whose properties are the names of this scope.
    pub with: Option<ObjRef>,
    /// Whether this scope holds the `var`s of a function or of the program.
    pub var_scope: bool,
    pub parent: Option<EnvRef>,
    pub this: Option<Val>,
    pub home: Option<ObjRef>,
    pub func: Option<ObjRef>,
    pub new_target: Option<Val>,
}

pub type EnvRef = Rc<RefCell<EnvRec>>;

/// The parameters the elements of a mapped `arguments` object stand for, `None` once deleted.
#[derive(Debug)]
pub struct Mapped {
    pub env: EnvRef,
    pub names: RefCell<Vec<Option<Name>>>,
}

impl Mapped {
    /// The parameter element `key` stands for.
    fn name(&self, key: &str) -> Option<Name> {
        array_index(key).and_then(|i| self.names.borrow().get(i).cloned().flatten())
    }
}

/// The prototypes and constructors the interpreter makes values with.
#[derive(Clone, Debug)]
pub struct Realm {
    pub global: ObjRef,
    pub object: ObjRef,
    pub function: ObjRef,
    pub array: ObjRef,
    pub string: ObjRef,
    pub number: ObjRef,
    pub boolean: ObjRef,
    pub symbol: ObjRef,
    pub bigint: ObjRef,
    pub error: ObjRef,
    pub date: ObjRef,
    pub regexp: ObjRef,
    pub map: ObjRef,
    pub set: ObjRef,
    pub iter: ObjRef,
    pub errors: Vec<(Name, ObjRef)>,
    pub bson: Vec<(Name, ObjRef)>,
    /// The prototypes of the other objects (`WeakMap`, `Proxy`, typed arrays…) by name.
    pub protos: Vec<(&'static str, ObjRef)>,
}

impl Realm {
    /// The prototype installed under `name`, or `Object.prototype`.
    pub fn proto(&self, name: &str) -> ObjRef {
        self.protos
            .iter()
            .find(|(n, _)| *n == name)
            .map_or_else(|| self.object.clone(), |(_, o)| o.clone())
    }
}

/// An iteration in progress.
pub enum IterRec {
    /// The values, read ahead, and the next one's index.
    Items(Vec<Val>, usize),
    /// An iterator object and its `next`.
    Proto { it: Val, next: Val },
}

/// The invocation's limits.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Statements, calls and loop turns a call may run.
    pub steps: u64,
    /// Bytes the call's live objects and strings may hold.
    pub heap: usize,
    /// Nested calls.
    pub depth: usize,
}

/// One interpreter, for one invocation.
pub struct Interp<'c> {
    pub realm: Realm,
    pub budget: Rc<Budget>,
    pub limits: Limits,
    pub steps: u64,
    pub depth: usize,
    /// The address of the stack when the first call began, to stop before the native stack
    /// runs out (a call through `for…in` and accessors takes far more of it than a plain one).
    pub stack_base: usize,
    pub envs: Vec<Weak<RefCell<EnvRec>>>,
    /// The name of the native function last called, for the messages that name it.
    pub native: Name,
    /// The last error about an operand (`can't convert ({}) to number`): the error, the operand
    /// and the text written for it, which the operator that sees its expression may replace.
    pub operand_failure: Option<(ObjRef, Val, String)>,
    pub global_env: EnvRef,
    pub now_ms: f64,
    pub rng: u64,
    pub deadline: Option<&'c dyn Fn() -> bool>,
    pub symbol_ids: u64,
    /// The symbols made without a description (`Symbol()`): their `description` is undefined.
    pub undescribed: Vec<u64>,
    /// The registry of `Symbol.for`.
    pub registry: Vec<(Name, Val)>,
    /// The strings arrays of the tagged templates evaluated so far, by site.
    pub templates: Vec<(usize, Val)>,
    /// Whether a `with` statement has run: names are then looked up in its objects too.
    pub with_used: bool,
    pub output: Vec<String>,
    /// The value of the last expression statement (a `$where` string's result).
    pub last: Option<Val>,
    /// Where the expression or `throw` evaluated last starts: the place of an error.
    pub pos: Pos,
    /// Whether the running code is strict: assignments to read-only properties throw.
    pub strict: bool,
    /// The name of the running function, for the stack of an error.
    pub fname: Name,
    /// The callers of the running function: their names and where each made its call.
    pub frames: Vec<(Name, Pos)>,
    /// Where the last `throw` left the evaluation: the place of a value that is not an error.
    pub thrown_at: Pos,
    /// What the engine of RefDb adds to the columns of the first line: the `(` it puts before a
    /// function, or the code it puts before a `$where` expression.
    pub col_offset: u32,
}

/// The key the parser gives a class's `static { … }` block.
pub const STATIC_BLOCK: &str = "@@static-block";

/// The file SpiderMonkey names in RefDb's errors.
pub const ERROR_FILE: &str = "wasm:function";

/// How often the deadline is read, in steps.
const DEADLINE_EVERY: u64 = 1024;

/// The bytes an object costs before its properties.
pub const OBJ_BYTES: usize = 96;
/// The bytes of a property or an element.
pub const PROP_BYTES: usize = 48;

impl Drop for Interp<'_> {
    fn drop(&mut self) {
        // Closures and their scopes refer to each other: clearing every scope breaks the cycles.
        for w in self.envs.drain(..) {
            if let Some(e) = w.upgrade() {
                let mut e = e.borrow_mut();
                e.vars.clear();
                e.parent = None;
                e.this = None;
                e.home = None;
                e.func = None;
            }
        }
        // The global object is the call's own; the objects it holds are shared ([`crate::shared`]).
        self.realm.global.borrow_mut().props.clear();
    }
}

/// Whether `new f()` may run: a class or plain function, a native constructor (one with a
/// `prototype`, or `Proxy`), a bound constructor or a callable proxy.
/// How much native stack the calls of one program may take: well inside the 2 MiB of a spawned
/// thread, so deep recursion ends as `too much recursion` instead of overflowing.
const STACK_BUDGET: usize = 1 << 20;

/// The address of a local of the caller's frame: where the stack is now.
#[inline(never)]
fn stack_position() -> usize {
    let marker = 0u8;
    std::hint::black_box(&marker as *const u8 as usize)
}

pub fn is_constructor(f: &Val) -> bool {
    let Val::Obj(o) = f else { return false };
    let b = o.borrow();
    match &b.kind {
        Kind::Func(Func::Closure { def, .. }) => {
            !(def.arrow || def.generator || def.is_async || def.method)
        }
        // `UUID`, `MD5` and `HexData` of the RefDb shell have no `prototype` and still construct.
        Kind::Func(Func::Native(_, name)) => {
            b.get_own("prototype").is_some()
                || matches!(&**name, "Proxy" | "UUID" | "MD5" | "HexData")
        }
        Kind::Func(Func::Bound(t, ..)) => is_constructor(&Val::Obj(t.clone())),
        Kind::Proxy(_, callable) => *callable,
        _ => false,
    }
}

/// The name of a key for messages: `o.f`.
pub(crate) fn callee_text(e: &Expr) -> String {
    match e {
        Expr::Ident(n, _) => n.to_string(),
        Expr::Member { obj, prop, .. } => match &**prop {
            MemberProp::Name(n) => format!("{}.{n}", callee_text(obj)),
            // SpiderMonkey folds a constant key: `o['k' + 1]` is `o.k1`, `o['a b']` stays.
            MemberProp::Computed(k) => match fold_key(k) {
                Some(key) if is_identifier(&key) => format!("{}.{key}", callee_text(obj)),
                Some(key) => format!("{}['{key}']", callee_text(obj)),
                None => match k {
                    Expr::Num(n) => format!("{}[{}]", callee_text(obj), number::to_string(*n)),
                    Expr::Ident(n, _) => format!("{}[{n}]", callee_text(obj)),
                    Expr::At(_, inner) if matches!(**inner, Expr::Ident(..)) => {
                        format!("{}[{}]", callee_text(obj), callee_text(inner))
                    }
                    _ => format!("{}[...]", callee_text(obj)),
                },
            },
        },
        Expr::This => "this".into(),
        Expr::Call { callee, args, .. } if args.is_empty() => format!("{}()", callee_text(callee)),
        Expr::Call { callee, .. } => format!("{}(...)", callee_text(callee)),
        Expr::Str(s) => format!("\"{s}\""),
        Expr::Num(n) => number::to_string(*n),
        Expr::OptChain(e) | Expr::At(_, e) => callee_text(e),
        _ => "(intermediate value)".into(),
    }
}

/// The string a constant key folds to: a string, or `+` of strings and numbers.
fn fold_key(e: &Expr) -> Option<String> {
    match e {
        Expr::Str(s) => Some(s.to_string()),
        Expr::At(_, x) => fold_key(x),
        Expr::Binary("+", a, b) => {
            let side = |x: &Expr| match x {
                Expr::Num(n) => Some(number::to_string(*n)),
                Expr::At(_, y) if matches!(**y, Expr::Num(_)) => match **y {
                    Expr::Num(n) => Some(number::to_string(n)),
                    _ => None,
                },
                other => fold_key(other),
            };
            Some(side(a)? + &side(b)?)
        }
        _ => None,
    }
}

fn is_identifier(s: &str) -> bool {
    let mut c = s.chars();
    c.next().is_some_and(|f| f == '_' || f == '$' || f.is_alphabetic())
        && c.all(|x| x == '_' || x == '$' || x.is_alphanumeric())
}

impl<'c> Interp<'c> {
    /// Whether the calls in progress have used up the stack budget (the stack grows downwards).
    fn stack_exhausted(&mut self) -> bool {
        let here = stack_position();
        if self.stack_base == 0 || self.depth == 0 {
            self.stack_base = here;
        }
        self.stack_base.saturating_sub(here) > STACK_BUDGET
    }

    // Limits.

    pub fn step(&mut self) -> R<()> {
        self.steps += 1;
        if self.steps > self.limits.steps {
            return Err(Ctrl::Fatal(Fatal::Steps));
        }
        if self.steps.is_multiple_of(DEADLINE_EVERY)
            && let Some(d) = self.deadline
            && d()
        {
            return Err(Ctrl::Fatal(Fatal::Deadline));
        }
        Ok(())
    }

    pub fn charge(&self, bytes: usize) -> R<()> {
        if self.budget.charge(bytes) { Ok(()) } else { Err(Ctrl::Fatal(Fatal::OutOfMemory)) }
    }

    /// A string made by joining others (`+`, a template, `concat`). SpiderMonkey joins them
    /// without copying and fails when the length passes its limit; here the copy fails earlier,
    /// at the heap limit, with the same catchable error (measured: `s = s + s` in a loop gives
    /// «allocation size overflow» on a `refrouter` 9.0.4).
    pub fn joined_string(&self, s: String) -> R {
        if s.len() > self.max_string() {
            return Err(self.error("InternalError", "allocation size overflow"));
        }
        self.string(s)
    }

    /// The longest string a call makes: an eighth of its heap, so that the error of a string
    /// that keeps growing comes before the heap runs out, as SpiderMonkey's limit of 2^30 - 2
    /// characters comes before its heap of 1100 MB does.
    pub fn max_string(&self) -> usize {
        self.budget.limit / 8
    }

    /// A new string, charged to the heap.
    pub fn string(&self, s: String) -> R {
        self.charge(s.len())?;
        Ok(Val::Str(crate::strings::pair_surrogates(s).into()))
    }

    // Objects.

    pub fn alloc(&self, proto: Option<ObjRef>, kind: Kind) -> R<ObjRef> {
        let size = OBJ_BYTES
            + match &kind {
                Kind::Array(v) | Kind::Set(v) => v.len() * PROP_BYTES,
                Kind::Map(v) => v.len() * PROP_BYTES * 2,
                _ => 0,
            };
        self.charge(size)?;
        Ok(ObjRef(Rc::new(ObjCell {
            obj: RefCell::new(Obj { proto, props: Vec::new(), kind, extensible: true, locks: 0 }),
            charged: std::cell::Cell::new(size),
            budget: self.budget.clone(),
            shared: std::cell::Cell::new(false),
        })))
    }

    /// Charges `n` more properties or elements to `o`.
    pub fn grow(&self, o: &ObjRef, n: usize) -> R<()> {
        let bytes = n * PROP_BYTES;
        self.charge(bytes)?;
        o.0.charged.set(o.0.charged.get() + bytes);
        Ok(())
    }

    pub fn object(&self) -> R<ObjRef> {
        self.alloc(Some(self.realm.object.clone()), Kind::Ordinary)
    }

    pub fn array(&self, items: Vec<Val>) -> R<Val> {
        Ok(Val::Obj(self.alloc(Some(self.realm.array.clone()), Kind::Array(items))?))
    }

    pub fn native(&self, name: &str, f: NativeFn, arity: usize) -> R<ObjRef> {
        let o = self
            .alloc(Some(self.realm.function.clone()), Kind::Func(Func::Native(f, name.into())))?;
        {
            let mut b = o.borrow_mut();
            b.props.push((
                "length".into(),
                Prop { writable: false, ..Prop::hidden(Val::Num(arity as f64)) },
            ));
            b.props.push(("name".into(), Prop { writable: false, ..Prop::hidden(Val::str(name)) }));
        }
        Ok(o)
    }

    /// An error of constructor `name` (`TypeError`, …) with `message`.
    pub fn error(&self, name: &str, message: &str) -> Ctrl {
        match self.make_error(name, message) {
            Ok(v) => Ctrl::Throw(v),
            Err(c) => c,
        }
    }

    pub fn make_error(&self, name: &str, message: &str) -> R {
        let proto = self
            .realm
            .errors
            .iter()
            .find(|(n, _)| &**n == name)
            .map_or_else(|| self.realm.error.clone(), |(_, o)| o.clone());
        let proto = proto.borrow().get_own("prototype").and_then(|p| p.value.as_obj().cloned());
        let o = self.alloc(proto, Kind::Error)?;
        self.grow(&o, 1)?;
        o.borrow_mut().props.push(("message".into(), Prop::hidden(Val::str(message))));
        self.place_error(&o)?;
        Ok(Val::Obj(o))
    }

    /// The column RefDb reports for `p`.
    pub fn shown(&self, p: Pos) -> (u32, u32) {
        (p.line, if p.line == 1 { p.col + self.col_offset } else { p.col })
    }

    /// The stack SpiderMonkey writes for an error made here: `name@file:line:column` for this
    /// function and each caller, at most 128.
    pub fn stack_text(&self) -> String {
        let mut out = String::new();
        let here = std::iter::once((&self.fname, self.pos));
        let callers = self.frames.iter().rev().map(|(n, p)| (n, *p));
        for (name, p) in here.chain(callers).take(128) {
            if p.line == 0 {
                // The call the runtime makes has no place in the code.
                continue;
            }
            let (l, c) = self.shown(p);
            out.push_str(&format!("{name}@{ERROR_FILE}:{l}:{c}\n"));
        }
        out
    }

    /// Gives an error object where it is made: `fileName`, `lineNumber`, `columnNumber` and
    /// `stack`, as SpiderMonkey does.
    pub fn place_error(&self, o: &ObjRef) -> R<()> {
        let (l, c) = self.shown(self.pos);
        let stack = self.stack_text();
        self.grow(o, 4)?;
        let mut b = o.borrow_mut();
        for (k, v) in [
            ("fileName", Val::str(ERROR_FILE)),
            ("lineNumber", Val::Num(f64::from(l))),
            ("columnNumber", Val::Num(f64::from(c))),
            // Read through the accessor `Error.prototype.stack`, as in SpiderMonkey.
            ("@@stack", Val::Str(stack.into())),
        ] {
            b.props.retain(|(x, _)| &**x != k);
            b.props.push((k.into(), Prop::hidden(v)));
        }
        Ok(())
    }

    pub fn type_error<T>(&self, message: &str) -> R<T> {
        Err(self.error("TypeError", message))
    }

    /// A `TypeError` about the object `v` with its source as its text; an operator that sees the
    /// operand's expression writes the expression instead ([`Interp::name_operands`]).
    pub fn operand_error(&mut self, v: &Val, message: impl FnOnce(&str) -> String) -> Ctrl {
        let text = crate::decompile::value_source(self, v);
        let e = self.error("TypeError", &message(&text));
        if let Ctrl::Throw(Val::Obj(o)) = &e {
            self.operand_failure = Some((o.clone(), v.clone(), text));
        }
        e
    }

    /// `r`, with the text of the operand in an operand error replaced by its expression, as
    /// SpiderMonkey's decompiler writes it (`can't convert o to number`).
    pub fn name_operands<T>(&mut self, r: R<T>, operands: &[(&Expr, &Val)]) -> R<T> {
        let Err(Ctrl::Throw(Val::Obj(err))) = &r else { return r };
        let Some((o, v, text)) = self.operand_failure.take() else { return r };
        if !o.ptr_eq(err) {
            return r;
        }
        let Some((e, _)) = operands.iter().find(|(_, x)| strict_eq(x, &v)) else { return r };
        let name = crate::decompile::operand_text(self, e, &v);
        let mut b = err.borrow_mut();
        if let Some((_, p)) = b.props.iter_mut().find(|(k, _)| &**k == "message")
            && let Val::Str(m) = &p.value
        {
            p.value = Val::Str(m.replacen(&text, &name, 1).into());
        }
        drop(b);
        r
    }

    // Conversions.

    pub fn primitive(&mut self, v: &Val, hint: &str) -> R {
        let Val::Obj(o) = v else { return Ok(v.clone()) };
        let exotic = self.get(v, "@@Symbol.toPrimitive:0")?;
        let to = if hint == "string" { "string" } else { "number" };
        if !exotic.is_nullish() && !exotic.is_callable() {
            return Err(self.operand_error(v, |t| {
                format!(
                    "can't convert {t} to {to}: its [Symbol.toPrimitive] property is not a function"
                )
            }));
        }
        if exotic.is_callable() {
            let r = self.call(&exotic, v, &[Val::str(hint)])?;
            if matches!(r, Val::Obj(_)) {
                return Err(self.operand_error(v, |t| {
                    format!("can't convert {t} to {to}: its [Symbol.toPrimitive] method returned an object")
                }));
            }
            return Ok(r);
        }
        let kind = o.borrow().kind.clone();
        match kind {
            Kind::Num(n) => return Ok(Val::Num(n)),
            Kind::Str(s) => return Ok(Val::Str(s)),
            Kind::Bool(b) => return Ok(Val::Bool(b)),
            Kind::BigInt(b) => return Ok(Val::BigInt(b)),
            _ => {}
        }
        let order: [&str; 2] =
            if hint == "string" || (hint == "default" && matches!(kind, Kind::Date(_))) {
                ["toString", "valueOf"]
            } else {
                ["valueOf", "toString"]
            };
        for m in order {
            let f = self.get(v, m)?;
            if f.is_callable() {
                let r = self.call(&f, v, &[])?;
                if !matches!(r, Val::Obj(_)) {
                    return Ok(r);
                }
            }
        }
        let to = match hint {
            "number" => "number",
            "string" => "string",
            _ => "primitive type",
        };
        Err(self.operand_error(v, |t| format!("can't convert {t} to {to}")))
    }

    pub fn number_of(&mut self, v: &Val) -> R<f64> {
        Ok(match v {
            Val::Undef => f64::NAN,
            Val::Null => 0.0,
            Val::Bool(b) => f64::from(u8::from(*b)),
            Val::Num(n) => *n,
            Val::Str(s) => number::parse(s),
            Val::Sym(_) => return self.type_error("can't convert symbol to number"),
            Val::BigInt(_) => return self.type_error("can't convert BigInt to number"),
            Val::Obj(_) => {
                let p = self.primitive(v, "number")?;
                self.number_of(&p)?
            }
        })
    }

    pub fn str_of(&mut self, v: &Val) -> R<Name> {
        Ok(match v {
            Val::Undef => "undefined".into(),
            Val::Null => "null".into(),
            Val::Bool(b) => if *b { "true" } else { "false" }.into(),
            Val::Num(n) => number::to_string(*n).into(),
            Val::Str(s) => s.clone(),
            Val::BigInt(b) => b.to_string().into(),
            Val::Sym(_) => return self.type_error("can't convert symbol to string"),
            Val::Obj(_) => {
                let p = self.primitive(v, "string")?;
                self.str_of(&p)?
            }
        })
    }

    /// The property key of `v`.
    pub fn key_of(&mut self, v: &Val) -> R<Name> {
        match v {
            Val::Sym(s) => Ok(format!("@@{}:{}", s.0, s.1).into()),
            Val::Num(n) if *n >= 0.0 && n.fract() == 0.0 && *n < 1e15 => {
                Ok(format!("{}", *n as u64).into())
            }
            _ => self.str_of(v),
        }
    }

    pub fn object_of(&mut self, v: &Val) -> R<ObjRef> {
        match v {
            Val::Obj(o) => Ok(o.clone()),
            Val::Undef | Val::Null => {
                self.type_error(&format!("can't convert {} to object", self.show(v)))
            }
            Val::Bool(b) => self.alloc(Some(self.realm.boolean.clone()), Kind::Bool(*b)),
            Val::Num(n) => self.alloc(Some(self.realm.number.clone()), Kind::Num(*n)),
            Val::Str(s) => self.alloc(Some(self.realm.string.clone()), Kind::Str(s.clone())),
            Val::Sym(_) => self.alloc(Some(self.realm.symbol.clone()), Kind::Symbol(v.clone())),
            Val::BigInt(b) => self.alloc(Some(self.realm.bigint.clone()), Kind::BigInt(b.clone())),
        }
    }

    /// A short form of a value for messages.
    pub fn show(&self, v: &Val) -> String {
        match v {
            Val::Undef => "undefined".into(),
            Val::Null => "null".into(),
            Val::Str(s) => format!("\"{s}\""),
            Val::Num(n) => number::to_string(*n),
            Val::BigInt(b) => format!("{b}n"),
            Val::Bool(b) => b.to_string(),
            Val::Sym(s) => format!("Symbol(\"{}\")", s.0),
            Val::Obj(o) => {
                if o.borrow().is_callable() {
                    "function".into()
                } else {
                    "object".into()
                }
            }
        }
    }

    // Properties.

    fn proto_of_primitive(&self, v: &Val) -> Option<ObjRef> {
        match v {
            Val::Str(_) => Some(self.realm.string.clone()),
            Val::Num(_) => Some(self.realm.number.clone()),
            Val::Bool(_) => Some(self.realm.boolean.clone()),
            Val::Sym(_) => Some(self.realm.symbol.clone()),
            Val::BigInt(_) => Some(self.realm.bigint.clone()),
            _ => None,
        }
    }

    /// The property `key` of `v`, through its prototypes and getters.
    pub fn get(&mut self, v: &Val, key: &str) -> R {
        let start = match v {
            Val::Obj(o) => o.clone(),
            Val::Undef | Val::Null => {
                return self.type_error(&format!("{} has no properties", self.show(v)));
            }
            Val::Str(s) => {
                if key == "length" {
                    return Ok(Val::Num(crate::strings::utf16_len(s) as f64));
                }
                if let Some(i) = array_index(key) {
                    return Ok(utf16_at(s, i).map_or(Val::Undef, |c| {
                        Val::Str(crate::strings::from_units(&[c]).into())
                    }));
                }
                self.proto_of_primitive(v).unwrap_or_else(|| self.realm.object.clone())
            }
            _ => self.proto_of_primitive(v).unwrap_or_else(|| self.realm.object.clone()),
        };
        let mut cur = Some(start);
        while let Some(o) = cur {
            if crate::proxy::is_proxy(&o) {
                return crate::proxy::get(self, &o, key, v);
            }
            let found = {
                let b = o.borrow();
                match &b.kind {
                    Kind::Array(items) => {
                        if key == "length" {
                            return Ok(Val::Num(items.len() as f64));
                        }
                        if let Some(i) = array_index(key)
                            && let Some(x) = items.get(i)
                            && (b.locks & ELEMENTS_OWN == 0 || b.get_own(key).is_none())
                        {
                            return Ok(x.clone());
                        }
                    }
                    Kind::Str(s) if key == "length" => {
                        return Ok(Val::Num(crate::strings::utf16_len(s) as f64));
                    }
                    Kind::Str(s) if let Some(c) = array_index(key).and_then(|i| utf16_at(s, i)) => {
                        return Ok(Val::Str(crate::strings::from_units(&[c]).into()));
                    }
                    Kind::Arguments(Some(m)) => {
                        if let Some(n) = m.name(key) {
                            let v = m
                                .env
                                .borrow()
                                .vars
                                .iter()
                                .rev()
                                .find(|(k, _)| *k == n)
                                .and_then(|(_, b)| b.value.clone());
                            if let Some(v) = v {
                                return Ok(v);
                            }
                        }
                    }
                    Kind::Typed(t) => {
                        if let Some(i) = crate::typed::numeric_key(key) {
                            return Ok(match i {
                                Some(i) if i < t.length() => crate::typed::read(t, i),
                                _ => Val::Undef,
                            });
                        }
                    }
                    _ => {}
                }
                b.get_own(key).cloned()
            };
            if let Some(p) = found {
                if let Some(g) = p.getter {
                    return self.call(&Val::Obj(g), v, &[]);
                }
                if p.setter.is_some() {
                    return Ok(Val::Undef);
                }
                return Ok(p.value);
            }
            cur = o.borrow().proto.clone();
        }
        Ok(Val::Undef)
    }

    /// Sets property `key` of `target` to `value`, through setters of its prototypes.
    pub fn set(&mut self, target: &Val, key: &str, value: Val) -> R<()> {
        let Val::Obj(o) = target else {
            if target.is_nullish() {
                return self.type_error(&format!("{} has no properties", self.show(target)));
            }
            return Ok(());
        };
        if crate::proxy::is_proxy(o) {
            return crate::proxy::set(self, o, key, value, target);
        }
        let typed = match &o.borrow().kind {
            Kind::Typed(t) => crate::typed::numeric_key(key).map(|i| (t.clone(), i)),
            _ => None,
        };
        if let Some((t, i)) = typed {
            let x = crate::typed::convert(self, t.ty, &value)?;
            if let Some(i) = i
                && i < t.length()
            {
                crate::typed::write(&t, i, &x);
            }
            return Ok(());
        }
        {
            let mut b = o.borrow_mut();
            let (locks, extensible) = (b.locks, b.extensible);
            let own = locks & ELEMENTS_OWN != 0
                && array_index(key).is_some()
                && b.props.iter().any(|(n, _)| &**n == key);
            if let Kind::Array(items) = &mut b.kind {
                if key == "length" && locks & LENGTH_FIXED != 0 {
                    drop(b);
                    return self.read_only(key);
                }
                if key == "length" {
                    drop(b);
                    let n = self.number_of(&value)?;
                    if n < 0.0 || n.fract() != 0.0 || n > f64::from(u32::MAX) {
                        return Err(self.error("RangeError", "invalid array length"));
                    }
                    let n = n as usize;
                    let grow = n.saturating_sub(self.array_len(o));
                    self.grow(o, grow)?;
                    if let Kind::Array(items) = &mut o.borrow_mut().kind {
                        items.resize(n, Val::Undef);
                    }
                    return Ok(());
                }
                if let Some(i) = array_index(key).filter(|_| !own) {
                    if i < items.len() {
                        if locks & ELEMENTS_FROZEN != 0 {
                            drop(b);
                            return self.read_only(key);
                        }
                        items[i] = value;
                        return Ok(());
                    }
                    if locks & LENGTH_FIXED != 0 || !extensible {
                        let fixed = locks & LENGTH_FIXED != 0;
                        drop(b);
                        if !self.strict {
                            return Ok(());
                        }
                        return Err(crate::arrays::cannot_grow(self, fixed, i));
                    }
                    let grow = i + 1 - items.len();
                    drop(b);
                    self.grow(o, grow)?;
                    if let Kind::Array(items) = &mut o.borrow_mut().kind {
                        items.resize(i + 1, Val::Undef);
                        items[i] = value;
                    }
                    return Ok(());
                }
            }
        }
        // A setter or a read-only property along the chain.
        let mut cur = Some(o.clone());
        while let Some(c) = cur {
            let found = c.borrow().get_own(key).cloned();
            if let Some(p) = found {
                if let Some(s) = p.setter {
                    self.call(&Val::Obj(s), target, &[value])?;
                    return Ok(());
                }
                if p.getter.is_some() {
                    let shown = if array_index(key).is_some() {
                        key.to_string()
                    } else {
                        format!("\"{key}\"")
                    };
                    return self.type_error(&format!("setting getter-only property {shown}"));
                }
                if !p.writable {
                    return self.read_only(key);
                }
                if c.ptr_eq(o) {
                    let mut b = o.borrow_mut();
                    if let Kind::Arguments(Some(m)) = &b.kind
                        && let Some(n) = m.name(key)
                        && let Some((_, binding)) =
                            m.env.borrow_mut().vars.iter_mut().rev().find(|(k, _)| *k == n)
                    {
                        binding.value = Some(value.clone());
                    }
                    if let (Some(i), Kind::Array(items)) = (array_index(key), &mut b.kind)
                        && let Some(slot) = items.get_mut(i)
                    {
                        *slot = value.clone();
                    }
                    if let Some(p) = b.get_own_mut(key) {
                        p.value = value;
                    }
                    return Ok(());
                }
                break;
            }
            cur = c.borrow().proto.clone();
        }
        if !o.borrow().extensible {
            if self.strict {
                return self.type_error(&format!(
                    "can't define property \"{key}\": Object is not extensible"
                ));
            }
            return Ok(());
        }
        self.grow(o, 1)?;
        o.borrow_mut().props.push((key.into(), Prop::data(value)));
        Ok(())
    }

    /// An assignment to a read-only property: ignored, or a `TypeError` in strict code.
    fn read_only(&self, key: &str) -> R<()> {
        if self.strict {
            return self.type_error(&format!("{} is read-only", crate::props::shown_key(key)));
        }
        Ok(())
    }

    pub fn define(&self, o: &ObjRef, key: &str, prop: Prop) -> R<()> {
        let mut b = o.borrow_mut();
        if let Some(p) = b.get_own_mut(key) {
            *p = prop;
            return Ok(());
        }
        drop(b);
        self.grow(o, 1)?;
        o.borrow_mut().props.push((key.into(), prop));
        Ok(())
    }

    pub fn array_len(&self, o: &ObjRef) -> usize {
        match &o.borrow().kind {
            Kind::Array(v) => v.len(),
            _ => 0,
        }
    }

    /// Whether `key` is an own property of `o`.
    pub fn has_own(&self, o: &ObjRef, key: &str) -> bool {
        let b = o.borrow();
        match &b.kind {
            Kind::Array(items)
                if key == "length" || array_index(key).is_some_and(|i| i < items.len()) =>
            {
                true
            }
            Kind::Str(s)
                if key == "length"
                    || array_index(key).is_some_and(|i| i < crate::strings::utf16_len(s)) =>
            {
                true
            }
            Kind::Typed(t) if crate::typed::numeric_key(key).is_some() => {
                matches!(crate::typed::numeric_key(key), Some(Some(i)) if i < t.length())
            }
            _ => b.get_own(key).is_some(),
        }
    }

    pub fn has_property(&self, o: &ObjRef, key: &str) -> bool {
        let mut cur = Some(o.clone());
        while let Some(c) = cur {
            if self.has_own(&c, key) {
                return true;
            }
            cur = c.borrow().proto.clone();
        }
        false
    }

    /// The own string keys of `o` (only the enumerable ones when asked), through a proxy's
    /// traps.
    pub fn keys(&mut self, o: &ObjRef, only_enumerable: bool) -> R<Vec<Name>> {
        if crate::proxy::is_proxy(o) {
            return crate::proxy::keys(self, o, only_enumerable);
        }
        Ok(self.own_keys(o, only_enumerable))
    }

    /// Whether `o` or its prototypes have `key`, through a proxy's `has`.
    pub fn has(&mut self, o: &ObjRef, key: &str) -> R<bool> {
        let mut cur = Some(o.clone());
        while let Some(c) = cur {
            if crate::proxy::is_proxy(&c) {
                return crate::proxy::has(self, &c, key);
            }
            if self.has_own(&c, key) {
                return Ok(true);
            }
            cur = c.borrow().proto.clone();
        }
        Ok(false)
    }

    /// Whether `key` is an own property of `o`, through a proxy's traps.
    pub fn has_own_property(&mut self, o: &ObjRef, key: &str) -> R<bool> {
        if crate::proxy::is_proxy(o) {
            return crate::proxy::has_own(self, o, key);
        }
        Ok(self.has_own(o, key))
    }

    /// `delete o[key]`.
    pub fn delete_key(&mut self, o: &ObjRef, key: &str) -> R<bool> {
        if crate::proxy::is_proxy(o) {
            return crate::proxy::delete(self, o, key);
        }
        if crate::props::own_prop(o, key).is_some_and(|p| !p.configurable) {
            if self.strict {
                let k = crate::props::shown_key(key);
                return self
                    .type_error(&format!("property {k} is non-configurable and can't be deleted"));
            }
            return Ok(false);
        }
        let mut b = o.borrow_mut();
        if let Kind::Arguments(Some(m)) = &b.kind
            && let Some(i) = array_index(key)
            && let Some(slot) = m.names.borrow_mut().get_mut(i)
        {
            *slot = None;
        }
        if let Kind::Array(items) = &mut b.kind
            && let Some(i) = array_index(key)
        {
            // Holes are not kept: the element becomes undefined.
            if i < items.len() {
                items[i] = Val::Undef;
            }
            b.remove(key);
            return Ok(true);
        }
        b.remove(key);
        Ok(true)
    }

    /// The enumerable own string keys of `o`, indices first.
    pub fn own_keys(&self, o: &ObjRef, only_enumerable: bool) -> Vec<Name> {
        let b = o.borrow();
        let mut out: Vec<Name> = match &b.kind {
            Kind::Array(items) => (0..items.len()).map(|i| i.to_string().into()).collect(),
            Kind::Str(s) => {
                (0..crate::strings::utf16_len(s)).map(|i| i.to_string().into()).collect()
            }
            Kind::Typed(t) => (0..t.length()).map(|i| i.to_string().into()).collect(),
            _ => Vec::new(),
        };
        let mut ints: Vec<(usize, Name)> = Vec::new();
        let mut rest = Vec::new();
        let elements = matches!(b.kind, Kind::Array(_));
        for (k, p) in &b.props {
            // An array element with attributes of its own is listed with the others.
            if elements && array_index(k).is_some() {
                if only_enumerable && !p.enumerable {
                    out.retain(|x| x != k);
                }
                continue;
            }
            if (only_enumerable && !p.enumerable) || k.starts_with("@@") {
                continue;
            }
            match array_index(k) {
                Some(i) => ints.push((i, k.clone())),
                None => rest.push(k.clone()),
            }
        }
        ints.sort_by_key(|(i, _)| *i);
        out.extend(ints.into_iter().map(|(_, k)| k));
        out.extend(rest);
        out
    }

    // Calls.

    pub fn call(&mut self, f: &Val, this: &Val, args: &[Val]) -> R {
        let Val::Obj(fo) = f else {
            return self.type_error(&format!("{} is not a function", self.show(f)));
        };
        if crate::proxy::is_proxy(fo) && fo.borrow().is_callable() {
            self.step()?;
            return crate::proxy::apply(self, fo, this, args);
        }
        let func = match &fo.borrow().kind {
            Kind::Func(func) => func.clone(),
            _ => return self.type_error(&format!("{} is not a function", self.show(f))),
        };
        self.step()?;
        if self.depth >= self.limits.depth || self.stack_exhausted() {
            return Err(self.error("InternalError", "too much recursion"));
        }
        self.depth += 1;
        let r = match func {
            Func::Native(n, name) => {
                self.native = name;
                n(self, this, args)
            }
            Func::Bound(target, bound_this, bound_args) => {
                let mut all = bound_args.clone();
                all.extend_from_slice(args);
                self.call(&Val::Obj(target), &bound_this, &all)
            }
            Func::Closure { def, env, this: arrow_this, home, derived, class, .. } => {
                if class.is_some() || derived || def.is_ctor_of_derived {
                    self.depth -= 1;
                    return self.type_error("class constructors must be invoked with 'new'");
                }
                let this = arrow_this.unwrap_or_else(|| this.clone());
                self.run_closure(
                    fo,
                    &def,
                    &env,
                    if def.arrow { None } else { Some(this) },
                    home,
                    args,
                    None,
                )
            }
        };
        self.depth -= 1;
        r
    }

    pub fn new_env(&mut self, parent: Option<EnvRef>) -> EnvRef {
        let e = Rc::new(RefCell::new(EnvRec { parent, ..EnvRec::default() }));
        self.envs.push(Rc::downgrade(&e));
        if self.envs.len() > 4096 {
            self.envs.retain(|w| w.strong_count() > 0);
        }
        e
    }

    #[allow(clippy::too_many_arguments)]
    pub fn run_closure(
        &mut self,
        fo: &ObjRef,
        def: &Rc<FuncDef>,
        env: &EnvRef,
        this: Option<Val>,
        home: Option<ObjRef>,
        args: &[Val],
        new_target: Option<Val>,
    ) -> R {
        self.charge(0)?;
        let name = match fo.borrow().get_own("name").map(|p| p.value.clone()) {
            Some(Val::Str(n)) if !n.is_empty() => n,
            _ => def.guessed.get().cloned().unwrap_or_else(|| "".into()),
        };
        let caller = std::mem::replace(&mut self.fname, name);
        self.frames.push((caller, self.pos));
        let r = self.run_closure_in(fo, def, env, this, home, args, new_target);
        if let Some((caller, pos)) = self.frames.pop() {
            self.fname = caller;
            self.pos = pos;
        }
        r
    }

    #[allow(clippy::too_many_arguments)]
    fn run_closure_in(
        &mut self,
        fo: &ObjRef,
        def: &Rc<FuncDef>,
        env: &EnvRef,
        this: Option<Val>,
        home: Option<ObjRef>,
        args: &[Val],
        new_target: Option<Val>,
    ) -> R {
        // Sloppy code sees `this` as an object: the global one for undefined and null.
        let this = match this {
            Some(Val::Undef | Val::Null) if !def.strict && !def.arrow => {
                Some(Val::Obj(self.realm.global.clone()))
            }
            Some(v) if !def.strict && !def.arrow && !matches!(v, Val::Obj(_)) => {
                Some(Val::Obj(self.object_of(&v)?))
            }
            other => other,
        };
        let scope = self.new_env(Some(env.clone()));
        {
            let mut s = scope.borrow_mut();
            if !def.arrow {
                s.this = this;
                s.home = home;
                s.func = Some(fo.clone());
                s.new_target = Some(new_target.unwrap_or(Val::Undef));
            }
        }
        if let (false, Some(name)) = (def.arrow, &def.name) {
            declare(&scope, name, Val::Obj(fo.clone()), true);
        }
        if !def.arrow {
            let plain = def.rest.is_none()
                && def.params.iter().all(|(p, d)| d.is_none() && matches!(p, Pattern::Name(_)));
            let mapped = (plain && !def.strict).then(|| {
                let names = def
                    .params
                    .iter()
                    .take(args.len())
                    .map(|(p, _)| match p {
                        Pattern::Name(n) => Some(n.clone()),
                        _ => None,
                    })
                    .collect();
                Rc::new(Mapped { env: scope.clone(), names: RefCell::new(names) })
            });
            let a = self.alloc(Some(self.realm.object.clone()), Kind::Arguments(mapped))?;
            self.grow(&a, args.len() + 1)?;
            {
                let mut b = a.borrow_mut();
                for (i, x) in args.iter().enumerate() {
                    b.props.push((i.to_string().into(), Prop::data(x.clone())));
                }
                b.props.push(("length".into(), Prop::hidden(Val::Num(args.len() as f64))));
            }
            declare(&scope, "arguments", Val::Obj(a), true);
        }
        // A list with defaults or patterns: each parameter is uninitialized until its turn, and
        // the body's `var`s get a scope of their own (ECMA-262 §10.2.11).
        let expressions = def.rest.as_ref().is_some_and(|r| !matches!(r, Pattern::Name(_)))
            || def.params.iter().any(|(p, d)| d.is_some() || !matches!(p, Pattern::Name(_)));
        if expressions {
            let mut names = Vec::new();
            for (p, _) in &def.params {
                pattern_names(p, &mut names);
            }
            if let Some(r) = &def.rest {
                pattern_names(r, &mut names);
            }
            let mut sc = scope.borrow_mut();
            for n in names {
                sc.vars.push((n, Binding { value: None, mutable: true, lexical: true }));
            }
        }
        scope.borrow_mut().var_scope = true;
        for (i, (p, d)) in def.params.iter().enumerate() {
            let mut v = args.get(i).cloned().unwrap_or(Val::Undef);
            if matches!(v, Val::Undef)
                && let Some(d) = d
            {
                v = self.eval(d, &scope)?;
                name_function(&v, p, d);
            }
            self.bind(p, v, &scope, Some(true))?;
        }
        if let Some(r) = &def.rest {
            let rest = args.get(def.params.len()..).unwrap_or(&[]).to_vec();
            let arr = self.array(rest)?;
            self.bind(r, arr, &scope, Some(true))?;
        }
        let scope = if expressions {
            let body = self.new_env(Some(scope.clone()));
            body.borrow_mut().var_scope = true;
            // A `var` named like a parameter starts with the parameter's value.
            let mut names = Vec::new();
            for st in &def.body {
                var_names(st, &mut names);
            }
            for n in names {
                let v = scope
                    .borrow()
                    .vars
                    .iter()
                    .rev()
                    .find(|(k, _)| *k == n)
                    .and_then(|(_, b)| b.value.clone());
                if let Some(v) = v {
                    declare(&body, &n, v, true);
                }
            }
            body
        } else {
            scope
        };
        self.hoist(&def.body, &scope, true)?;
        if def.generator {
            return crate::generators::make(self, fo, def, scope);
        }
        let outer = std::mem::replace(&mut self.strict, def.strict);
        let r = self.exec_block_in(&def.body, &scope);
        self.strict = outer;
        if def.is_async {
            return crate::promise::settle_async(self, r);
        }
        let v = match r {
            Ok(()) | Err(Ctrl::Break(_) | Ctrl::Continue(_)) => Val::Undef,
            Err(Ctrl::Return(v)) => v,
            Err(e) => return Err(e),
        };
        // A derived constructor gives the `this` its `super(...)` made, unless it returns an
        // object.
        if def.is_ctor_of_derived && !matches!(v, Val::Obj(_)) {
            return Ok(scope.borrow().this.clone().unwrap_or(Val::Undef));
        }
        Ok(v)
    }

    /// `new f(...args)`.
    pub fn construct(&mut self, f: &Val, args: &[Val], new_target: &Val) -> R {
        let Val::Obj(fo) = f else {
            return self.type_error(&format!("{} is not a constructor", self.show(f)));
        };
        if crate::proxy::is_proxy(fo) && fo.borrow().is_callable() {
            self.step()?;
            return crate::proxy::construct(self, fo, args, new_target);
        }
        let func = match &fo.borrow().kind {
            Kind::Func(func) => func.clone(),
            _ => return self.type_error(&format!("{} is not a constructor", self.show(f))),
        };
        self.step()?;
        if self.depth >= self.limits.depth || self.stack_exhausted() {
            return Err(self.error("InternalError", "too much recursion"));
        }
        self.depth += 1;
        let r = self.construct_inner(fo, func, args, new_target);
        self.depth -= 1;
        r
    }

    fn construct_inner(&mut self, fo: &ObjRef, func: Func, args: &[Val], new_target: &Val) -> R {
        match func {
            Func::Native(n, _) => {
                // A native constructor sees `new` through `this` being the new target marker.
                let marker = Val::Obj(self.alloc(None, Kind::Ordinary)?);
                if let Val::Obj(m) = &marker {
                    m.borrow_mut().props.push(("@@new".into(), Prop::hidden(new_target.clone())));
                }
                let v = n(self, &marker, args)?;
                // A subclass of a native constructor: the result takes the subclass's prototype.
                if let (Val::Obj(o), Val::Obj(nt)) = (&v, new_target)
                    && !nt.ptr_eq(fo)
                {
                    let p = self.get(new_target, "prototype")?;
                    if let Val::Obj(p) = p {
                        o.borrow_mut().proto = Some(p);
                    }
                }
                Ok(v)
            }
            Func::Bound(target, _, bound_args) => {
                let mut all = bound_args.clone();
                all.extend_from_slice(args);
                let t = Val::Obj(target);
                self.construct(&t, &all, &t)
            }
            Func::Closure { def, env, home, class, class_env, derived, .. } => {
                if def.arrow {
                    return self.type_error("arrow functions are not constructors");
                }
                let proto = match self.get(new_target, "prototype")? {
                    Val::Obj(p) => Some(p),
                    _ => Some(self.realm.object.clone()),
                };
                if derived {
                    // `this` comes from `super(...)`.
                    let v = self.run_closure(
                        fo,
                        &def,
                        &env,
                        None,
                        home,
                        args,
                        Some(new_target.clone()),
                    )?;
                    return match v {
                        Val::Obj(_) => Ok(v),
                        _ => self.type_error("derived class constructor did not call super()"),
                    };
                }
                let obj = Val::Obj(self.alloc(proto, Kind::Ordinary)?);
                if let (Some(c), Some(ce)) = (&class, &class_env) {
                    self.init_fields(c, ce, &obj)?;
                }
                let v = self.run_closure(
                    fo,
                    &def,
                    &env,
                    Some(obj.clone()),
                    home,
                    args,
                    Some(new_target.clone()),
                )?;
                Ok(if matches!(v, Val::Obj(_)) { v } else { obj })
            }
        }
    }

    /// The instance fields of class `c` on `obj`.
    fn init_fields(&mut self, c: &ClassDef, env: &EnvRef, obj: &Val) -> R<()> {
        for m in c.members.iter().filter(|m| !m.is_static && m.func.is_none()) {
            let scope = self.new_env(Some(env.clone()));
            scope.borrow_mut().this = Some(obj.clone());
            let key = self.prop_key(&m.key, &scope)?;
            let v = match &m.field {
                Some(e) => self.eval(e, &scope)?,
                None => Val::Undef,
            };
            if let Val::Obj(o) = obj {
                self.define(o, &key, Prop::data(v))?;
            }
        }
        Ok(())
    }

    // Environments.

    fn lookup(&self, env: &EnvRef, name: &str) -> Option<(EnvRef, usize)> {
        let mut cur = Some(env.clone());
        while let Some(e) = cur {
            let idx = e.borrow().vars.iter().rposition(|(k, _)| &**k == name);
            if let Some(i) = idx {
                return Some((e, i));
            }
            cur = e.borrow().parent.clone();
        }
        None
    }

    /// A scope whose names are the properties of `o` (`with`).
    pub fn with_scope(&mut self, o: &Val, env: &EnvRef) -> R<EnvRef> {
        let o = self.object_of(o)?;
        let scope = self.new_env(Some(env.clone()));
        scope.borrow_mut().with = Some(o);
        self.with_used = true;
        Ok(scope)
    }

    /// The object of the innermost `with` that has `name`, before any scope that declares it.
    fn with_object(&mut self, env: &EnvRef, name: &str) -> R<Option<ObjRef>> {
        let mut cur = Some(env.clone());
        while let Some(e) = cur {
            let with = e.borrow().with.clone();
            if let Some(o) = with {
                if self.has(&o, name)? {
                    let blocked = match self.get(&Val::Obj(o.clone()), "@@Symbol.unscopables:0")? {
                        u @ Val::Obj(_) => self.get(&u, name)?.truthy(),
                        _ => false,
                    };
                    if !blocked {
                        return Ok(Some(o));
                    }
                }
            } else if e.borrow().vars.iter().any(|(k, _)| &**k == name) {
                return Ok(None);
            }
            cur = e.borrow().parent.clone();
        }
        Ok(None)
    }

    pub fn read_var(&mut self, env: &EnvRef, name: &str) -> R {
        if self.with_used
            && let Some(o) = self.with_object(env, name)?
        {
            return self.get(&Val::Obj(o), name);
        }
        if let Some((e, i)) = self.lookup(env, name) {
            return match &e.borrow().vars[i].1.value {
                Some(v) => Ok(v.clone()),
                None => Err(self.error(
                    "ReferenceError",
                    &format!("can't access lexical declaration '{name}' before initialization"),
                )),
            };
        }
        let g = Val::Obj(self.realm.global.clone());
        if self.has_property(&self.realm.global.clone(), name) {
            return self.get(&g, name);
        }
        Err(self.error("ReferenceError", &format!("{name} is not defined")))
    }

    fn write_var(&mut self, env: &EnvRef, name: &str, v: Val) -> R<()> {
        if self.with_used
            && let Some(o) = self.with_object(env, name)?
        {
            return self.set(&Val::Obj(o), name, v);
        }
        if let Some((e, i)) = self.lookup(env, name) {
            let mut b = e.borrow_mut();
            let binding = &mut b.vars[i].1;
            if binding.value.is_none() {
                drop(b);
                return Err(self.error(
                    "ReferenceError",
                    &format!("can't access lexical declaration '{name}' before initialization"),
                ));
            }
            if !binding.mutable {
                drop(b);
                return self.type_error(&format!("invalid assignment to const '{name}'"));
            }
            binding.value = Some(v);
            return Ok(());
        }
        let g = Val::Obj(self.realm.global.clone());
        self.set(&g, name, v)
    }

    fn this_of(&self, env: &EnvRef) -> Option<(Val, EnvRef)> {
        let mut cur = Some(env.clone());
        while let Some(e) = cur {
            if let Some(t) = &e.borrow().this {
                return Some((t.clone(), e.clone()));
            }
            if e.borrow().func.is_some() {
                // A derived constructor before `super()`.
                return None;
            }
            cur = e.borrow().parent.clone();
        }
        None
    }

    fn function_env(&self, env: &EnvRef) -> Option<EnvRef> {
        let mut cur = Some(env.clone());
        while let Some(e) = cur {
            if e.borrow().func.is_some() {
                return Some(e);
            }
            cur = e.borrow().parent.clone();
        }
        None
    }

    /// Declares the `var`s and functions of `body` in `scope` (function scope), or the
    /// `let`, `const`, `class` and functions of a block.
    pub fn hoist(&mut self, body: &[Stmt], scope: &EnvRef, function_scope: bool) -> R<()> {
        if function_scope {
            let mut names = Vec::new();
            for s in body {
                var_names(s, &mut names);
            }
            for n in names {
                if !scope.borrow().vars.iter().any(|(k, _)| *k == n) {
                    declare(scope, &n, Val::Undef, true);
                }
            }
        }
        for s in body {
            match s {
                Stmt::Decl(k @ (DeclKind::Let | DeclKind::Const), d) => {
                    for (p, _) in d {
                        let mut names = Vec::new();
                        pattern_names(p, &mut names);
                        for n in names {
                            scope.borrow_mut().vars.push((
                                n,
                                Binding {
                                    value: None,
                                    mutable: *k == DeclKind::Let,
                                    lexical: true,
                                },
                            ));
                        }
                    }
                }
                Stmt::Class(n, _) => {
                    scope
                        .borrow_mut()
                        .vars
                        .push((n.clone(), Binding { value: None, mutable: true, lexical: true }));
                }
                Stmt::Func(n, def) => {
                    let f = self.closure(def, scope)?;
                    declare(scope, n, f, true);
                }
                _ => {}
            }
        }
        Ok(())
    }

    pub fn closure(&mut self, def: &Rc<FuncDef>, env: &EnvRef) -> R {
        let this =
            if def.arrow { Some(self.this_of(env).map_or(Val::Undef, |(t, _)| t)) } else { None };
        let home = if def.arrow {
            self.function_env(env).and_then(|e| e.borrow().home.clone())
        } else {
            None
        };
        let f = self.alloc(
            Some(self.realm.function.clone()),
            Kind::Func(Func::Closure {
                def: def.clone(),
                env: env.clone(),
                this,
                home,
                class: None,
                class_env: None,
                derived: false,
            }),
        )?;
        self.grow(&f, 3)?;
        let fixed = |v: Val| Prop { writable: false, ..Prop::hidden(v) };
        let prototype = if def.generator {
            let (fp, pp) = if def.is_async {
                ("AsyncGeneratorFunction", "AsyncGenerator")
            } else {
                ("GeneratorFunction", "Generator")
            };
            f.borrow_mut().proto = Some(self.realm.proto(fp));
            Some(self.alloc(Some(self.realm.proto(pp)), Kind::Ordinary)?)
        } else if def.is_async {
            f.borrow_mut().proto = Some(self.realm.proto("AsyncFunction"));
            None
        } else if !def.arrow {
            let proto = self.object()?;
            self.define(&proto, "constructor", Prop::hidden(Val::Obj(f.clone())))?;
            Some(proto)
        } else {
            None
        };
        let mut b = f.borrow_mut();
        if let Some(p) = prototype {
            b.props.push((
                "prototype".into(),
                Prop { configurable: false, ..Prop::hidden(Val::Obj(p)) },
            ));
        }
        let length = def.params.iter().take_while(|(_, d)| d.is_none()).count() as f64;
        b.props.push(("length".into(), fixed(Val::Num(length))));
        let name = def.name.clone().unwrap_or_else(|| "".into());
        b.props.push(("name".into(), fixed(Val::Str(name))));
        drop(b);
        Ok(Val::Obj(f))
    }

    /// Binds pattern `p` to `v`: declaring in `scope` (`mutable` given) or assigning.
    pub fn bind(
        &mut self,
        p: &Pattern,
        v: Val,
        scope: &EnvRef,
        declare_mutable: Option<bool>,
    ) -> R<()> {
        match p {
            Pattern::Name(n) => match declare_mutable {
                Some(m) => {
                    let mut s = scope.borrow_mut();
                    match s.vars.iter_mut().rev().find(|(k, _)| k == n) {
                        Some((_, b)) => {
                            b.value = Some(v);
                            b.mutable = m;
                        }
                        None => s.vars.push((
                            n.clone(),
                            Binding { value: Some(v), mutable: m, lexical: false },
                        )),
                    }
                    Ok(())
                }
                None => self.write_var(scope, n, v),
            },
            Pattern::Expr(e) => self.assign_to(e, v, scope),
            Pattern::Object(props, rest) => {
                if v.is_nullish() {
                    return self.type_error(&format!("{} has no properties", self.show(&v)));
                }
                let mut used = Vec::new();
                for (k, target, d) in props {
                    let key = self.prop_key(k, scope)?;
                    let mut x = self.get(&v, &key)?;
                    if matches!(x, Val::Undef)
                        && let Some(d) = d
                    {
                        x = self.eval(d, scope)?;
                        name_function(&x, target, d);
                    }
                    used.push(key);
                    self.bind(target, x, scope, declare_mutable)?;
                }
                if let Some(r) = rest {
                    let o = self.object()?;
                    if let Val::Obj(src) = &v {
                        for k in self.keys(src, true)? {
                            if !used.contains(&k) {
                                let x = self.get(&v, &k)?;
                                self.define(&o, &k, Prop::data(x))?;
                            }
                        }
                    }
                    self.bind(r, Val::Obj(o), scope, declare_mutable)?;
                }
                Ok(())
            }
            Pattern::Array(items, rest) => {
                // An iterator is read as far as the pattern needs and then closed.
                let mut rec = self.open_iter(&v)?;
                let mut all = Vec::new();
                let want = if rest.is_some() { usize::MAX } else { items.len() };
                while all.len() < want {
                    match self.iter_step(&mut rec)? {
                        Some(x) => all.push(x),
                        None => break,
                    }
                }
                if all.len() == want && rest.is_none() {
                    self.iter_close(&rec)?;
                }
                for (i, item) in items.iter().enumerate() {
                    let Some((target, d)) = item else { continue };
                    let mut x = all.get(i).cloned().unwrap_or(Val::Undef);
                    if matches!(x, Val::Undef)
                        && let Some(d) = d
                    {
                        x = self.eval(d, scope)?;
                        name_function(&x, target, d);
                    }
                    self.bind(target, x, scope, declare_mutable)?;
                }
                if let Some(r) = rest {
                    let tail = all.get(items.len()..).unwrap_or(&[]).to_vec();
                    let arr = self.array(tail)?;
                    self.bind(r, arr, scope, declare_mutable)?;
                }
                Ok(())
            }
        }
    }

    fn prop_key(&mut self, k: &PropName, scope: &EnvRef) -> R<Name> {
        match k {
            PropName::Fixed(n) => Ok(n.clone()),
            PropName::Computed(e) => {
                let v = self.eval(e, scope)?;
                self.key_of(&v)
            }
        }
    }

    /// The values `v` iterates over (arrays, strings, maps, sets, iterators and iterables).
    /// Starts iterating `v`: the values of the objects the interpreter knows, or the iterator
    /// its `Symbol.iterator` gives, read one value at a time.
    pub fn open_iter(&mut self, v: &Val) -> R<IterRec> {
        let fast = match v {
            Val::Str(_) => true,
            Val::Obj(o) => matches!(
                o.borrow().kind,
                Kind::Array(_)
                    | Kind::Typed(_)
                    | Kind::Set(_)
                    | Kind::Map(_)
                    | Kind::Iter(_)
                    | Kind::Arguments(_)
            ),
            _ => false,
        };
        if fast {
            return Ok(IterRec::Items(self.iterate(v)?, 0));
        }
        if v.is_nullish() {
            let m = format!("can't access property Symbol.iterator of {}", self.show(v));
            return self.type_error(&m);
        }
        let it_fn = self.get(v, "@@Symbol.iterator:0")?;
        if !it_fn.is_callable() {
            return Err(self.not_iterable(v));
        }
        let it = self.call(&it_fn, v, &[])?;
        if !matches!(it, Val::Obj(_)) {
            return self.type_error("result of the Symbol.iterator method is not an object");
        }
        let next = self.get(&it, "next")?;
        Ok(IterRec::Proto { it, next })
    }

    /// The next value of an iteration, `None` at its end.
    pub fn iter_step(&mut self, rec: &mut IterRec) -> R<Option<Val>> {
        self.step()?;
        match rec {
            IterRec::Items(items, i) => {
                let v = items.get(*i).cloned();
                *i += 1;
                Ok(v)
            }
            IterRec::Proto { it, next } => {
                let (it, next) = (it.clone(), next.clone());
                let r = self.call(&next, &it, &[])?;
                if !matches!(r, Val::Obj(_)) {
                    return self.type_error("iterator.next() returned a non-object value");
                }
                if self.get(&r, "done")?.truthy() {
                    *rec = IterRec::Items(Vec::new(), 0);
                    return Ok(None);
                }
                Ok(Some(self.get(&r, "value")?))
            }
        }
    }

    /// Ends an iteration early: calls the iterator's `return`.
    pub fn iter_close(&mut self, rec: &IterRec) -> R<()> {
        let IterRec::Proto { it, .. } = rec else { return Ok(()) };
        let f = self.get(it, "return")?;
        if f.is_callable() {
            let r = self.call(&f, it, &[])?;
            if !matches!(r, Val::Obj(_)) {
                return self.type_error("iterator.return() returned a non-object value");
            }
        }
        Ok(())
    }

    pub fn iterate(&mut self, v: &Val) -> R<Vec<Val>> {
        match v {
            Val::Str(s) => Ok(crate::text::code_points(s).map(|c| Val::Str(c.into())).collect()),
            Val::Obj(o) => {
                let kind = o.borrow().kind.clone();
                match kind {
                    Kind::Array(items) if o.borrow().locks & ELEMENTS_OWN == 0 => {
                        return Ok(items);
                    }
                    Kind::Array(items) => {
                        // An element with its own attributes is read through its getter.
                        let target = Val::Obj(o.clone());
                        let mut out = Vec::with_capacity(items.len());
                        for i in 0..items.len() {
                            out.push(self.get(&target, &i.to_string())?);
                        }
                        return Ok(out);
                    }
                    Kind::Typed(t) => return Ok(crate::typed::elements(&t)),
                    Kind::Set(items) => return Ok(items),
                    Kind::Map(entries) => {
                        let mut out = Vec::with_capacity(entries.len());
                        for (k, x) in entries {
                            out.push(self.array(vec![k, x])?);
                        }
                        return Ok(out);
                    }
                    Kind::Iter(state) => {
                        let (items, pos) = &mut *state.borrow_mut();
                        let rest = items.get(*pos..).unwrap_or(&[]).to_vec();
                        *pos = items.len();
                        return Ok(rest);
                    }
                    Kind::Arguments(_) => {
                        let len = self.get(v, "length")?;
                        let n = self.number_of(&len)? as usize;
                        let mut out = Vec::new();
                        for i in 0..n {
                            out.push(self.get(v, &i.to_string())?);
                        }
                        return Ok(out);
                    }
                    _ => {}
                }
                let it_fn = self.get(v, "@@Symbol.iterator:0")?;
                let it = if it_fn.is_callable() {
                    self.call(&it_fn, v, &[])?
                } else if self.get(v, "next")?.is_callable() {
                    v.clone()
                } else {
                    return Err(self.not_iterable(v));
                };
                let next = self.get(&it, "next")?;
                let mut out = Vec::new();
                loop {
                    self.step()?;
                    let r = self.call(&next, &it, &[])?;
                    if self.get(&r, "done")?.truthy() {
                        return Ok(out);
                    }
                    self.charge(PROP_BYTES)?;
                    out.push(self.get(&r, "value")?);
                }
            }
            _ => Err(self.not_iterable(v)),
        }
    }

    // Statements.

    pub fn exec_block_in(&mut self, body: &[Stmt], scope: &EnvRef) -> R<()> {
        for s in body {
            self.exec(s, scope)?;
        }
        Ok(())
    }

    fn block(&mut self, body: &[Stmt], env: &EnvRef) -> R<()> {
        let needs_scope = body.iter().any(|s| {
            matches!(
                s,
                Stmt::Decl(DeclKind::Let | DeclKind::Const, _) | Stmt::Class(..) | Stmt::Func(..)
            )
        });
        if !needs_scope {
            return self.exec_block_in(body, env);
        }
        let scope = self.new_env(Some(env.clone()));
        self.hoist(body, &scope, false)?;
        self.exec_block_in(body, &scope)
    }

    pub fn exec(&mut self, s: &Stmt, env: &EnvRef) -> R<()> {
        self.step()?;
        match s {
            Stmt::Expr(e) => {
                let v = self.eval(e, env)?;
                self.last = Some(v);
            }
            Stmt::Decl(kind, decls) => {
                for (p, init) in decls {
                    let v = match init {
                        Some(e) => {
                            let v = self.eval(e, env)?;
                            name_function(&v, p, e);
                            v
                        }
                        None if *kind == DeclKind::Var => continue,
                        None => Val::Undef,
                    };
                    match kind {
                        DeclKind::Var => self.bind(p, v, env, None)?,
                        DeclKind::Let => self.bind(p, v, env, Some(true))?,
                        DeclKind::Const => self.bind(p, v, env, Some(false))?,
                    }
                }
            }
            Stmt::Func(..) | Stmt::Empty => {}
            Stmt::Suspends(s) => self.exec(s, env)?,
            Stmt::Class(n, c) => {
                let v = self.class(c, env)?;
                self.bind(&Pattern::Name(n.clone()), v, env, Some(true))?;
            }
            Stmt::Return(e) => {
                let v = match e {
                    Some(e) => self.eval(e, env)?,
                    None => Val::Undef,
                };
                return Err(Ctrl::Return(v));
            }
            Stmt::If(c, a, b) => {
                if self.eval(c, env)?.truthy() {
                    self.exec(a, env)?;
                } else if let Some(b) = b {
                    self.exec(b, env)?;
                }
            }
            Stmt::Block(body) => self.block(body, env)?,
            Stmt::While(c, body) => {
                while self.eval(c, env)?.truthy() {
                    if loop_ctrl(self.exec(body, env), None)? {
                        break;
                    }
                }
            }
            Stmt::DoWhile(body, c) => loop {
                if loop_ctrl(self.exec(body, env), None)? || !self.eval(c, env)?.truthy() {
                    break;
                }
            },
            Stmt::For(init, test, update, body) => {
                self.exec_for(init.as_ref(), test.as_ref(), update.as_ref(), body, env, None)?
            }
            Stmt::ForIn { decl, target, of, obj, body } => {
                self.exec_for_in(*decl, target, *of, obj, body, env, None)?
            }
            Stmt::Break(l) => return Err(Ctrl::Break(l.clone())),
            Stmt::Continue(l) => return Err(Ctrl::Continue(l.clone())),
            Stmt::Throw(e, pos) => {
                // A value that is not an error is reported where the evaluation left the
                // position: the `throw`, or the last call or property inside it.
                self.pos = *pos;
                let v = self.eval(e, env)?;
                self.thrown_at = self.pos;
                return Err(Ctrl::Throw(v));
            }
            Stmt::Try(body, catch, finally) => {
                let mut r = self.block(body, env);
                if let (Err(Ctrl::Throw(x)), Some((param, handler))) = (&r, catch) {
                    let x = x.clone();
                    let scope = self.new_env(Some(env.clone()));
                    if let Some(p) = param {
                        self.bind(p, x, &scope, Some(true))?;
                    }
                    // The completion value of a statement list (`eval`): a catch starts from
                    // undefined, and a finally that ends normally keeps the value before it
                    // (measured on the engine of a `refrouter` 9.0.4).
                    self.last = Some(Val::Undef);
                    r = self.block(handler, &scope);
                }
                if let Some(f) = finally {
                    let before = self.last.replace(Val::Undef);
                    self.block(f, env)?;
                    self.last = before;
                }
                r?;
            }
            Stmt::Switch(d, cases) => {
                let v = self.eval(d, env)?;
                let scope = self.new_env(Some(env.clone()));
                let all: Vec<&Stmt> = cases.iter().flat_map(|(_, b)| b.iter()).collect();
                for s in &all {
                    if let Stmt::Decl(k @ (DeclKind::Let | DeclKind::Const), d) = s {
                        for (p, _) in d {
                            let mut names = Vec::new();
                            pattern_names(p, &mut names);
                            for n in names {
                                scope.borrow_mut().vars.push((
                                    n,
                                    Binding {
                                        value: None,
                                        mutable: *k == DeclKind::Let,
                                        lexical: true,
                                    },
                                ));
                            }
                        }
                    }
                }
                let mut start = None;
                for (i, (t, _)) in cases.iter().enumerate() {
                    if let Some(t) = t {
                        let x = self.eval(t, &scope)?;
                        if strict_eq(&v, &x) {
                            start = Some(i);
                            break;
                        }
                    }
                }
                let start = start.or_else(|| cases.iter().position(|(t, _)| t.is_none()));
                if let Some(start) = start {
                    for (_, body) in &cases[start..] {
                        match self.exec_block_in(body, &scope) {
                            Err(Ctrl::Break(None)) => return Ok(()),
                            r => r?,
                        }
                    }
                }
            }
            Stmt::With(o, body) => {
                let o = self.eval(o, env)?;
                let scope = self.with_scope(&o, env)?;
                self.exec(body, &scope)?;
            }
            Stmt::Labeled(label, body) => {
                let r = match &**body {
                    Stmt::For(i, t, u, b) => {
                        self.exec_for(i.as_ref(), t.as_ref(), u.as_ref(), b, env, Some(label))
                    }
                    Stmt::ForIn { decl, target, of, obj, body } => {
                        self.exec_for_in(*decl, target, *of, obj, body, env, Some(label))
                    }
                    Stmt::While(c, b) => {
                        let mut r = Ok(());
                        loop {
                            match self.eval(c, env) {
                                Ok(v) if v.truthy() => {}
                                Ok(_) => break,
                                Err(e) => {
                                    r = Err(e);
                                    break;
                                }
                            }
                            match loop_ctrl(self.exec(b, env), Some(label)) {
                                Ok(true) => break,
                                Ok(false) => {}
                                Err(e) => {
                                    r = Err(e);
                                    break;
                                }
                            }
                        }
                        r
                    }
                    other => self.exec(other, env),
                };
                match r {
                    Err(Ctrl::Break(Some(l))) if l == *label => {}
                    r => r?,
                }
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn exec_for(
        &mut self,
        init: Option<&ForInit>,
        test: Option<&Expr>,
        update: Option<&Expr>,
        body: &Stmt,
        env: &EnvRef,
        label: Option<&Name>,
    ) -> R<()> {
        let (mut scope, per_iteration) = self.for_init(init, env)?;
        loop {
            if let Some(t) = test
                && !self.eval(t, &scope)?.truthy()
            {
                break;
            }
            if loop_ctrl(self.exec(body, &scope), label)? {
                break;
            }
            if per_iteration {
                // Each iteration has its own copy of the loop's `let` bindings (closures see it).
                let copy = self.new_env(Some(env.clone()));
                copy.borrow_mut().vars = scope.borrow().vars.clone();
                scope = copy;
            }
            if let Some(u) = update {
                self.eval(u, &scope)?;
            }
        }
        Ok(())
    }

    /// A `for` loop's scope with its declarations run, and whether each iteration copies it.
    pub fn for_init(&mut self, init: Option<&ForInit>, env: &EnvRef) -> R<(EnvRef, bool)> {
        let scope = self.new_env(Some(env.clone()));
        let mut per_iteration = false;
        match init {
            Some(ForInit::Decl(kind, decls)) => {
                per_iteration = *kind != DeclKind::Var;
                self.exec(&Stmt::Decl(*kind, Vec::new()), &scope)?;
                for (p, e) in decls {
                    let v = match e {
                        Some(e) => self.eval(e, &scope)?,
                        None => Val::Undef,
                    };
                    match kind {
                        DeclKind::Var => self.bind(p, v, env, None)?,
                        k => self.bind(p, v, &scope, Some(*k == DeclKind::Let))?,
                    }
                }
            }
            Some(ForInit::Expr(e)) => {
                self.eval(e, &scope)?;
            }
            None => {}
        }
        Ok((scope, per_iteration))
    }

    #[allow(clippy::too_many_arguments)]
    fn exec_for_in(
        &mut self,
        decl: Option<DeclKind>,
        target: &Pattern,
        of: bool,
        obj: &Expr,
        body: &Stmt,
        env: &EnvRef,
        label: Option<&Name>,
    ) -> R<()> {
        let v = self.eval(obj, env)?;
        let mut rec =
            if of { self.open_iter(&v)? } else { IterRec::Items(self.for_in_keys(&v)?, 0) };
        while let Some(x) = self.iter_step(&mut rec)? {
            let scope = self.new_env(Some(env.clone()));
            let r = match decl {
                Some(DeclKind::Var) => self.bind(target, x, env, None),
                Some(k) => self.bind(target, x, &scope, Some(k == DeclKind::Let)),
                None => self.bind(target, x, &scope, None),
            }
            .and_then(|()| loop_ctrl(self.exec(body, &scope), label));
            match r {
                Ok(false) => {}
                Ok(true) => return self.iter_close(&rec),
                Err(e) => {
                    // A throw keeps its exception whatever `return` does.
                    let closed = self.iter_close(&rec);
                    if !matches!(e, Ctrl::Throw(_) | Ctrl::Fatal(_)) {
                        closed?;
                    }
                    return Err(e);
                }
            }
        }
        Ok(())
    }

    /// The keys `for…in` visits: the enumerable string keys of `v` and its prototypes.
    pub fn for_in_keys(&mut self, v: &Val) -> R<Vec<Val>> {
        Ok({
            match &v {
                Val::Undef | Val::Null => Vec::new(),
                _ => {
                    let o = self.object_of(v)?;
                    let mut keys: Vec<Name> = Vec::new();
                    let mut cur = Some(o);
                    while let Some(c) = cur {
                        for k in self.keys(&c, true)? {
                            if !keys.contains(&k) {
                                keys.push(k);
                            }
                        }
                        cur = c.borrow().proto.clone();
                    }
                    keys.into_iter().map(Val::Str).collect()
                }
            }
        })
    }

    // Classes.

    pub fn class(&mut self, c: &Rc<ClassDef>, env: &EnvRef) -> R {
        let scope = self.new_env(Some(env.clone()));
        let (parent_proto, parent_ctor) = match &c.parent {
            Some(p) => {
                let pv = self.eval(p, env)?;
                match &pv {
                    Val::Null => (None, None),
                    Val::Obj(_) if pv.is_callable() => match self.get(&pv, "prototype")? {
                        Val::Obj(pp) => (Some(pp), Some(pv)),
                        Val::Null => (None, Some(pv)),
                        _ => {
                            return self
                                .type_error("class heritage prototype is not an object or null");
                        }
                    },
                    _ => {
                        return self.type_error(&format!(
                            "class heritage {} is not an object or null",
                            self.show(&pv)
                        ));
                    }
                }
            }
            None => (Some(self.realm.object.clone()), None),
        };
        let proto = self.alloc(parent_proto, Kind::Ordinary)?;
        let derived = c.parent.is_some();
        let def = match &c.ctor {
            Some(d) => d.clone(),
            None => Rc::new(FuncDef {
                name: c.name.clone(),
                params: Vec::new(),
                rest: if derived { Some(Pattern::Name("args".into())) } else { None },
                body: if derived {
                    vec![Stmt::Expr(Expr::SuperCall(
                        vec![Expr::Spread(Box::new(Expr::Ident("args".into(), Pos::default())))],
                        c.pos,
                    ))]
                } else {
                    Vec::new()
                },
                arrow: false,
                generator: false,
                is_async: false,
                method: false,
                is_ctor_of_derived: derived,
                strict: true,
                source: format!("class {} {{ }}", c.name.as_deref().unwrap_or("")).into(),
                guessed: std::cell::OnceCell::new(),
            }),
        };
        let ctor_proto = match &parent_ctor {
            Some(Val::Obj(p)) => p.clone(),
            _ => self.realm.function.clone(),
        };
        let ctor = self.alloc(
            Some(ctor_proto),
            Kind::Func(Func::Closure {
                def,
                env: scope.clone(),
                this: None,
                home: Some(proto.clone()),
                class: Some(c.clone()),
                class_env: Some(scope.clone()),
                derived,
            }),
        )?;
        let name = c.name.clone().unwrap_or_else(|| "".into());
        let fixed = |v: Val| Prop { writable: false, ..Prop::hidden(v) };
        let prototype = Prop { configurable: false, ..fixed(Val::Obj(proto.clone())) };
        self.define(&ctor, "prototype", prototype)?;
        let length = match &c.ctor {
            Some(d) => d.params.iter().take_while(|(_, d)| d.is_none()).count(),
            None => 0,
        };
        self.define(&ctor, "length", fixed(Val::Num(length as f64)))?;
        self.define(&ctor, "name", fixed(Val::Str(name.clone())))?;
        self.define(&proto, "constructor", Prop::hidden(Val::Obj(ctor.clone())))?;
        if let Some(n) = &c.name {
            declare(&scope, n, Val::Obj(ctor.clone()), false);
        }
        for m in &c.members {
            let target = if m.is_static { ctor.clone() } else { proto.clone() };
            if m.is_static && matches!(&m.key, PropName::Fixed(k) if &**k == STATIC_BLOCK) {
                // `static { … }`: run once, with `this` the class.
                if let Some(f) = &m.func {
                    let fv = self.closure(f, &scope)?;
                    self.call(&fv, &Val::Obj(ctor.clone()), &[])?;
                }
                continue;
            }
            let key = self.prop_key(&m.key, &scope)?;
            if let Some(f) = &m.func {
                let fv = self.closure(f, &scope)?;
                if let Val::Obj(fo) = &fv
                    && let Kind::Func(Func::Closure { home, .. }) = &mut fo.borrow_mut().kind
                {
                    *home = Some(target.clone());
                }
                let fo = fv.as_obj().cloned();
                let mut p =
                    self.get_own_prop(&target, &key).unwrap_or_else(|| Prop::hidden(Val::Undef));
                p.enumerable = false;
                match m.kind {
                    PropKind::Value => p = Prop::hidden(fv),
                    PropKind::Get => p.getter = fo,
                    PropKind::Set => p.setter = fo,
                }
                self.define(&target, &key, p)?;
            } else if m.is_static {
                let s = self.new_env(Some(scope.clone()));
                s.borrow_mut().this = Some(Val::Obj(ctor.clone()));
                let v = match &m.field {
                    Some(e) => self.eval(e, &s)?,
                    None => Val::Undef,
                };
                self.define(&ctor, &key, Prop::data(v))?;
            }
        }
        Ok(Val::Obj(ctor))
    }

    fn get_own_prop(&self, o: &ObjRef, key: &str) -> Option<Prop> {
        o.borrow().get_own(key).cloned()
    }

    // Expressions.

    pub fn eval(&mut self, e: &Expr, env: &EnvRef) -> R {
        match e {
            Expr::Num(n) => Ok(Val::Num(*n)),
            Expr::BigInt(b) => Ok(Val::BigInt(b.clone())),
            // The parser leaves a generator's `yield` only where `generators.rs` runs it.
            Expr::Yield(..) => self.type_error("yield is only valid in generators"),
            Expr::Await(x) => {
                self.eval(x, env)?;
                Err(Ctrl::Fatal(Fatal::Trap))
            }
            Expr::Str(s) => Ok(Val::Str(s.clone())),
            Expr::Bool(b) => Ok(Val::Bool(*b)),
            Expr::Null => Ok(Val::Null),
            Expr::Undefined => Ok(Val::Undef),
            Expr::Template(strs, subs) => {
                let mut out = String::new();
                for (i, s) in strs.iter().enumerate() {
                    out.push_str(s);
                    if let Some(x) = subs.get(i) {
                        let v = self.eval(x, env)?;
                        let s = self.str_of(&v);
                        out.push_str(&self.name_operands(s, &[(x, &v)])?);
                    }
                }
                self.joined_string(out)
            }
            Expr::Regex(b, f) => crate::builtins::make_regexp(self, b.clone(), f.clone()),
            Expr::TemplateObject(parts) => self.template_object(parts),
            Expr::Ident(n, _) => {
                if &**n == "undefined" && self.lookup(env, "undefined").is_none() {
                    return Ok(Val::Undef);
                }
                self.read_var(env, n)
            }
            Expr::This => match self.this_of(env) {
                Some((t, _)) => Ok(t),
                None if self.function_env(env).is_some() => Err(self.error(
                    "ReferenceError",
                    "must call super constructor before using 'this' in derived class constructor",
                )),
                None => Ok(Val::Undef),
            },
            Expr::Array(items) => {
                let mut out = Vec::with_capacity(items.len());
                for it in items {
                    match it {
                        None => out.push(Val::Undef),
                        Some(Expr::Spread(x)) => {
                            let v = self.eval(x, env)?;
                            let all = self.iterate(&v);
                            let all = self.name_operands(all, &[(x, &v)])?;
                            self.charge(all.len() * PROP_BYTES)?;
                            out.extend(all);
                        }
                        Some(x) => out.push(self.eval(x, env)?),
                    }
                }
                self.array(out)
            }
            Expr::Object(props) => self.object_literal(props, env),
            Expr::Func(def) => self.closure(def, env),
            Expr::Class(c) => self.class(c, env),
            Expr::Spread(_) => self.type_error("unexpected spread"),
            Expr::Unary(op, x) => {
                let v = self.eval(x, env)?;
                let r = self.unary(op, &v);
                self.name_operands(r, &[(x, &v)])
            }
            Expr::Typeof(x) => {
                if let Expr::Ident(n, _) = &**x
                    && self.lookup(env, n).is_none()
                    && !self.has_property(&self.realm.global.clone(), n)
                {
                    return Ok(Val::str("undefined"));
                }
                let v = self.eval(x, env)?;
                Ok(Val::str(v.type_of()))
            }
            Expr::Delete(x) => match &**x {
                Expr::Member { obj, prop, .. } => {
                    let o = self.eval(obj, env)?;
                    let key = self.member_key(prop, env)?;
                    match &o {
                        Val::Obj(o) => Ok(Val::Bool(self.delete_key(o, &key)?)),
                        _ => Ok(Val::Bool(true)),
                    }
                }
                Expr::Ident(n, _) => self.delete_name(env, n),
                Expr::At(_, inner) if matches!(&**inner, Expr::Ident(..)) => {
                    let Expr::Ident(n, _) = &**inner else { return Ok(Val::Bool(true)) };
                    self.delete_name(env, n)
                }
                _ => Ok(Val::Bool(true)),
            },
            Expr::Update { op, prefix, target } => {
                let old = self.eval(target, env)?;
                let (old, new) = match self.numeric(&old)? {
                    Val::BigInt(b) => {
                        let one = crate::bigint::BigInt::from_i64(1);
                        let new = if *op == "++" { b.add(&one) } else { b.sub(&one) };
                        (Val::BigInt(b), Val::BigInt(Rc::new(new)))
                    }
                    v => {
                        let n = self.number_of(&v)?;
                        (Val::Num(n), Val::Num(if *op == "++" { n + 1.0 } else { n - 1.0 }))
                    }
                };
                self.assign_to(target, new.clone(), env)?;
                Ok(if *prefix { new } else { old })
            }
            Expr::Binary(op, a, b) => {
                let x = self.eval(a, env)?;
                let y = self.eval(b, env)?;
                let r = self.binary(op, &x, &y);
                // SpiderMonkey names only the right operand of `+` (measured: `o + 1` gives the
                // source of `o`, `"" + Object.create(null)` gives `Object.create(...)`).
                if *op == "+" {
                    return self.name_operands(r, &[(b, &y)]);
                }
                self.name_operands(r, &[(a, &x), (b, &y)])
            }
            Expr::Logical(op, a, b) => {
                let x = self.eval(a, env)?;
                let short = match *op {
                    "&&" => !x.truthy(),
                    "||" => x.truthy(),
                    _ => !x.is_nullish(),
                };
                if short { Ok(x) } else { self.eval(b, env) }
            }
            Expr::Cond(c, a, b) => {
                if self.eval(c, env)?.truthy() {
                    self.eval(a, env)
                } else {
                    self.eval(b, env)
                }
            }
            Expr::Assign { op, target, value } => self.assign(op, target, value, env),
            Expr::Seq(all) => {
                let mut last = Val::Undef;
                for x in all {
                    last = self.eval(x, env)?;
                }
                Ok(last)
            }
            Expr::OptChain(x) => match self.eval(x, env) {
                Err(Ctrl::Break(Some(l))) if &*l == "?." => Ok(Val::Undef),
                r => r,
            },
            Expr::Member { obj, prop, optional, .. } => {
                let o = self.eval(obj, env)?;
                if *optional && o.is_nullish() {
                    return Err(Ctrl::Break(Some("?.".into())));
                }
                let key = self.member_key(prop, env)?;
                if o.is_nullish() {
                    let msg = null_access(obj, &key, &o);
                    return self.type_error(&msg);
                }
                self.get(&o, &key)
            }
            Expr::SuperMember(k) => {
                let key = self.eval(k, env)?;
                let key = self.key_of(&key)?;
                let home = self.function_env(env).and_then(|e| e.borrow().home.clone());
                let this = self.this_of(env).map_or(Val::Undef, |(t, _)| t);
                let proto = home.and_then(|h| h.borrow().proto.clone());
                match proto {
                    Some(p) => self.get_with_receiver(&p, &key, &this),
                    None => Ok(Val::Undef),
                }
            }
            Expr::SuperCall(args, pos) => {
                self.pos = *pos;
                self.super_call(args, env)
            }
            Expr::Call { callee, args, optional, pos } => {
                self.call_expr(callee, args, *optional, *pos, env)
            }
            Expr::At(pos, e) => {
                self.pos = *pos;
                self.eval(e, env)
            }
            Expr::New { callee, args, pos } => {
                let f = self.eval(callee, env)?;
                let args = self.eval_args(args, env)?;
                self.pos = *pos;
                if !is_constructor(&f) {
                    let text = match &**callee {
                        Expr::Func(def) => def.source.to_string(),
                        other => callee_text(other),
                    };
                    return self.type_error(&format!("{text} is not a constructor"));
                }
                self.construct(&f, &args, &f)
            }
        }
    }

    fn get_with_receiver(&mut self, start: &ObjRef, key: &str, receiver: &Val) -> R {
        let mut cur = Some(start.clone());
        while let Some(o) = cur {
            let p = o.borrow().get_own(key).cloned();
            if let Some(p) = p {
                if let Some(g) = p.getter {
                    return self.call(&Val::Obj(g), receiver, &[]);
                }
                return Ok(p.value);
            }
            cur = o.borrow().proto.clone();
        }
        Ok(Val::Undef)
    }

    fn super_call(&mut self, args: &[Expr], env: &EnvRef) -> R {
        let fenv = self.function_env(env).ok_or_else(|| {
            self.error("SyntaxError", "super() is only valid in derived class constructors")
        })?;
        let (func, new_target) = {
            let b = fenv.borrow();
            (b.func.clone(), b.new_target.clone().unwrap_or(Val::Undef))
        };
        let parent = match func.and_then(|f| f.borrow().proto.clone()) {
            Some(p) => Val::Obj(p),
            None => return self.type_error("super is not a constructor"),
        };
        let args = self.eval_args(args, env)?;
        let this = self.construct(&parent, &args, &new_target)?;
        if fenv.borrow().this.is_some() {
            return Err(
                self.error("ReferenceError", "super() called twice in derived class constructor")
            );
        }
        fenv.borrow_mut().this = Some(this.clone());
        // The fields of the derived class.
        let class = match &fenv.borrow().func.as_ref().map(|f| f.borrow().kind.clone()) {
            Some(Kind::Func(Func::Closure { class: Some(c), class_env: Some(ce), .. })) => {
                Some((c.clone(), ce.clone()))
            }
            _ => None,
        };
        if let Some((c, ce)) = class {
            self.init_fields(&c, &ce, &this)?;
        }
        // A derived constructor returns its `this`.
        Ok(this)
    }

    fn eval_args(&mut self, args: &[Expr], env: &EnvRef) -> R<Vec<Val>> {
        let mut out = Vec::with_capacity(args.len());
        for a in args {
            match a {
                Expr::Spread(x) => {
                    let v = self.eval(x, env)?;
                    let all = self.iterate(&v);
                    out.extend(self.name_operands(all, &[(x, &v)])?);
                }
                x => out.push(self.eval(x, env)?),
            }
        }
        Ok(out)
    }

    fn member_key(&mut self, prop: &MemberProp, env: &EnvRef) -> R<Name> {
        match prop {
            MemberProp::Name(n) => Ok(n.clone()),
            MemberProp::Computed(e) => {
                let v = self.eval(e, env)?;
                self.key_of(&v)
            }
        }
    }

    fn call_expr(
        &mut self,
        callee: &Expr,
        args: &[Expr],
        optional: bool,
        pos: Pos,
        env: &EnvRef,
    ) -> R {
        let (f, this) = match callee {
            Expr::Member { obj, prop, optional: opt_member, .. } => {
                let o = self.eval(obj, env)?;
                if *opt_member && o.is_nullish() {
                    return Err(Ctrl::Break(Some("?.".into())));
                }
                let key = self.member_key(prop, env)?;
                if o.is_nullish() {
                    let msg = null_access(obj, &key, &o);
                    return self.type_error(&msg);
                }
                (self.get(&o, &key)?, o)
            }
            Expr::SuperMember(_) => {
                let f = self.eval(callee, env)?;
                (f, self.this_of(env).map_or(Val::Undef, |(t, _)| t))
            }
            _ => (self.eval(callee, env)?, Val::Undef),
        };
        if optional && f.is_nullish() {
            return Err(Ctrl::Break(Some("?.".into())));
        }
        let args = self.eval_args(args, env)?;
        if is_eval_name(callee) && is_builtin_eval(&f) {
            return self.direct_eval(args.first().cloned().unwrap_or(Val::Undef), env);
        }
        self.pos = pos;
        if !f.is_callable() {
            return self.type_error(&format!("{} is not a function", callee_text(callee)));
        }
        self.call(&f, &this, &args)
    }

    fn object_literal(&mut self, props: &[ObjProp], env: &EnvRef) -> R {
        let o = self.object()?;
        for p in props {
            match p {
                ObjProp::Value(k, e) => {
                    let key = self.prop_key(k, env)?;
                    let v = self.eval(e, env)?;
                    if matches!(k, PropName::Fixed(n) if &**n == "__proto__") {
                        match v {
                            Val::Obj(p) => o.borrow_mut().proto = Some(p),
                            Val::Null => o.borrow_mut().proto = None,
                            _ => {}
                        }
                        continue;
                    }
                    if let (Val::Obj(f), Expr::Func(_)) = (&v, e) {
                        if let Kind::Func(Func::Closure { home, .. }) = &mut f.borrow_mut().kind {
                            *home = Some(o.clone());
                        }
                        let unnamed = matches!(f.borrow().get_own("name").map(|p| &p.value), Some(Val::Str(s)) if s.is_empty());
                        if unnamed && let Some(p) = f.borrow_mut().get_own_mut("name") {
                            p.value = Val::Str(key.clone());
                        }
                    }
                    self.define(&o, &key, Prop::data(v))?;
                }
                ObjProp::Accessor(k, kind, def) => {
                    let key = self.prop_key(k, env)?;
                    let f = self.closure(def, env)?;
                    let mut p = self
                        .get_own_prop(&o, &key)
                        .filter(Prop::is_accessor)
                        .unwrap_or_else(|| Prop::data(Val::Undef));
                    if *kind == PropKind::Get {
                        p.getter = f.as_obj().cloned();
                    } else {
                        p.setter = f.as_obj().cloned();
                    }
                    self.define(&o, &key, p)?;
                }
                ObjProp::Spread(e) => {
                    let v = self.eval(e, env)?;
                    match &v {
                        Val::Obj(src) => {
                            // Symbol keys are copied too, after the string keys.
                            let mut keys = self.keys(src, true)?;
                            keys.extend(crate::props::enumerable_symbol_keys(src));
                            for k in keys {
                                let x = self.get(&v, &k)?;
                                self.define(&o, &k, Prop::data(x))?;
                            }
                        }
                        Val::Str(s) => {
                            for (i, c) in crate::strings::units(s).into_iter().enumerate() {
                                let unit = crate::strings::from_units(&[c]);
                                self.define(&o, &i.to_string(), Prop::data(Val::Str(unit.into())))?;
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        Ok(Val::Obj(o))
    }

    fn assign(&mut self, op: &str, target: &Pattern, value: &Expr, env: &EnvRef) -> R {
        if op == "=" {
            let v = self.eval(value, env)?;
            name_function(&v, target, value);
            self.bind(target, v.clone(), env, None)?;
            return Ok(v);
        }
        let Pattern::Expr(t) = target else { return self.type_error("invalid assignment") };
        let old = self.eval(t, env)?;
        let v = match op {
            "&&=" if !old.truthy() => return Ok(old),
            "||=" if old.truthy() => return Ok(old),
            "??=" if !old.is_nullish() => return Ok(old),
            "&&=" | "||=" | "??=" => self.eval(value, env)?,
            _ => {
                let y = self.eval(value, env)?;
                self.binary(&op[..op.len() - 1], &old, &y)?
            }
        };
        self.assign_to(t, v.clone(), env)?;
        Ok(v)
    }

    fn assign_to(&mut self, target: &Expr, v: Val, env: &EnvRef) -> R<()> {
        match target {
            Expr::Ident(n, _) => self.write_var(env, n, v),
            Expr::Member { obj, prop, .. } => {
                let o = self.eval(obj, env)?;
                let key = self.member_key(prop, env)?;
                if o.is_nullish() {
                    let msg = null_access(obj, &key, &o);
                    return self.type_error(&msg);
                }
                self.set(&o, &key, v)
            }
            _ => Err(self.error("SyntaxError", "invalid assignment left-hand side")),
        }
    }

    pub fn instance_of(&mut self, x: &Val, f: &Val) -> R<bool> {
        if let Val::Obj(fo) = f {
            let custom = self.get(f, "@@Symbol.hasInstance:0")?;
            let builtin = fo.borrow().is_callable()
                && custom.as_obj().is_some_and(|h| {
                    matches!(&h.borrow().kind, Kind::Func(Func::Native(_, n)) if &**n == "[Symbol.hasInstance]")
                });
            if !custom.is_nullish() && !builtin {
                return Ok(self.call(&custom, f, std::slice::from_ref(x))?.truthy());
            }
        }
        self.ordinary_instance_of(x, f)
    }

    /// OrdinaryHasInstance: whether `f.prototype` is on the chain of `x`.
    /// A direct `eval(src)`: the code runs in the caller's scope, its `var`s go to the caller's
    /// function (unless it is strict) and may not redeclare a `let` on the way (ECMA-262 §19.2.1).
    pub fn direct_eval(&mut self, src: Val, env: &EnvRef) -> R {
        let Val::Str(src) = src else { return Ok(src) };
        let body =
            crate::parser::parse_script(&src).map_err(|e| self.error("SyntaxError", &e.message))?;
        let strict = self.strict || crate::parser::has_use_strict(&body);
        let lex = self.new_env(Some(env.clone()));
        let var_env = if strict { lex.clone() } else { var_scope_of(env) };
        let mut names = Vec::new();
        for st in &body {
            var_names(st, &mut names);
            if let Stmt::Func(n, _) = st {
                names.push(n.clone());
            }
        }
        if !strict {
            for n in &names {
                let mut cur = Some(env.clone());
                while let Some(e) = cur {
                    let clash = e.borrow().vars.iter().any(|(k, b)| k == n && b.lexical);
                    if clash {
                        return Err(self.error("SyntaxError", &format!("redeclaration of let {n}")));
                    }
                    if Rc::ptr_eq(&e, &var_env) {
                        break;
                    }
                    cur = e.borrow().parent.clone();
                }
            }
        }
        for n in &names {
            if !var_env.borrow().vars.iter().any(|(k, _)| k == n) {
                declare(&var_env, n, Val::Undef, true);
            }
        }
        self.hoist(&body, &lex, false)?;
        for st in &body {
            if let Stmt::Func(n, _) = st {
                let f = self.read_var(&lex, n)?;
                declare(&var_env, n, f, true);
            }
        }
        let outer = std::mem::replace(&mut self.strict, strict);
        let saved = self.last.take();
        let mut r = Ok(());
        for st in &body {
            r = self.exec(st, &lex);
            if r.is_err() {
                break;
            }
        }
        self.strict = outer;
        let v = self.last.take().unwrap_or(Val::Undef);
        self.last = saved;
        r?;
        Ok(v)
    }

    /// `delete name`: a declared binding stays (`false`); a property of the global object goes.
    fn delete_name(&mut self, env: &EnvRef, name: &str) -> R {
        if self.lookup(env, name).is_some() {
            return Ok(Val::Bool(false));
        }
        let g = self.realm.global.clone();
        if self.has_own(&g, name) {
            return Ok(Val::Bool(self.delete_key(&g, name)?));
        }
        Ok(Val::Bool(true))
    }

    /// The strings array of a tagged template: one per site and call, frozen, with its frozen
    /// `raw` (ECMA-262 §13.2.8.4).
    fn template_object(&mut self, parts: &Rc<crate::lexer::TemplateParts>) -> R {
        let site = Rc::as_ptr(parts) as usize;
        if let Some((_, v)) = self.templates.iter().find(|(s, _)| *s == site) {
            return Ok(v.clone());
        }
        let cooked = parts.cooked.iter().map(|c| c.clone().map_or(Val::Undef, Val::Str)).collect();
        let strings = self.array(cooked)?;
        let raw = self.array(parts.raw.iter().cloned().map(Val::Str).collect())?;
        let (Val::Obj(s), Val::Obj(r)) = (&strings, &raw) else { return Ok(strings) };
        crate::props::lock(r, true);
        let fixed = Prop { writable: false, configurable: false, ..Prop::hidden(raw.clone()) };
        self.define(s, "raw", fixed)?;
        crate::props::lock(s, true);
        self.templates.push((site, strings.clone()));
        Ok(strings)
    }

    /// `v is not iterable`, with the source of an object.
    fn not_iterable(&mut self, v: &Val) -> Ctrl {
        if matches!(v, Val::Obj(_)) {
            return self.operand_error(v, |t| format!("{t} is not iterable"));
        }
        self.error("TypeError", &format!("{} is not iterable", self.show(v)))
    }

    pub fn ordinary_instance_of(&mut self, x: &Val, f: &Val) -> R<bool> {
        if matches!(f, Val::Obj(_)) && !f.is_callable() {
            return Err(self.operand_error(f, |t| format!("{t} is not a function")));
        }
        if !f.is_callable() {
            return self.type_error(&format!("invalid 'instanceof' operand {}", self.show(f)));
        }
        if let Val::Obj(fo) = f
            && let Kind::Func(Func::Bound(t, ..)) = &fo.borrow().kind
        {
            let t = Val::Obj(t.clone());
            return self.ordinary_instance_of(x, &t);
        }
        let Val::Obj(o) = x else { return Ok(false) };
        let Val::Obj(proto) = self.get(f, "prototype")? else { return Ok(false) };
        let mut cur = o.borrow().proto.clone();
        while let Some(c) = cur {
            if c.ptr_eq(&proto) {
                return Ok(true);
            }
            cur = c.borrow().proto.clone();
        }
        Ok(false)
    }

    pub fn loose_eq(&mut self, a: &Val, b: &Val) -> R<bool> {
        match (a, b) {
            (Val::Bool(x), y) if !matches!(y, Val::Bool(_)) => {
                return self.loose_eq(&Val::Num(f64::from(u8::from(*x))), b);
            }
            (x, Val::Bool(y)) if !matches!(x, Val::Bool(_)) => {
                return self.loose_eq(a, &Val::Num(f64::from(u8::from(*y))));
            }
            _ => {}
        }
        Ok(match (a, b) {
            (Val::Undef | Val::Null, Val::Undef | Val::Null) => true,
            (Val::Undef | Val::Null, _) | (_, Val::Undef | Val::Null) => false,
            (Val::Num(_), Val::Str(_))
            | (Val::Str(_), Val::Num(_))
            | (Val::Bool(_), _)
            | (_, Val::Bool(_)) => {
                if matches!((a, b), (Val::Bool(_), Val::Bool(_))) {
                    return Ok(strict_eq(a, b));
                }
                let (x, y) = (self.number_of(a)?, self.number_of(b)?);
                if matches!(a, Val::Obj(_)) || matches!(b, Val::Obj(_)) {
                    let pa = self.primitive(a, "default")?;
                    let pb = self.primitive(b, "default")?;
                    return self.loose_eq(&pa, &pb);
                }
                x == y
            }
            (Val::BigInt(_), Val::Num(_) | Val::Str(_) | Val::BigInt(_))
            | (Val::Num(_) | Val::Str(_), Val::BigInt(_)) => {
                crate::bigints::compare(self, a, b)? == Some(std::cmp::Ordering::Equal)
            }
            (Val::Obj(_), Val::Obj(_)) => strict_eq(a, b),
            (Val::Obj(_), _) => {
                let p = self.primitive(a, "default")?;
                self.loose_eq(&p, b)?
            }
            (_, Val::Obj(_)) => {
                let p = self.primitive(b, "default")?;
                self.loose_eq(a, &p)?
            }
            _ => strict_eq(a, b),
        })
    }

    /// `ToNumeric`: a number or a `BigInt`.
    pub fn numeric(&mut self, v: &Val) -> R {
        let p = self.primitive(v, "number")?;
        Ok(match p {
            Val::BigInt(_) => p,
            other => Val::Num(self.number_of(&other)?),
        })
    }

    fn number_binary(&mut self, op: &str, x: &Val, y: &Val) -> R {
        let num = |s: &mut Self, v: &Val| s.number_of(v);
        Ok(match op {
            "@+" => Val::Num(num(self, x)? + num(self, y)?),
            "-" => Val::Num(num(self, x)? - num(self, y)?),
            "*" => Val::Num(num(self, x)? * num(self, y)?),
            "/" => Val::Num(num(self, x)? / num(self, y)?),
            "%" => {
                let (a, b) = (num(self, x)?, num(self, y)?);
                Val::Num(if b.is_infinite() && a.is_finite() { a } else { a % b })
            }
            "**" => {
                let (a, b) = (num(self, x)?, num(self, y)?);
                Val::Num(if b.is_nan() || (a.abs() == 1.0 && b.is_infinite()) {
                    f64::NAN
                } else {
                    a.powf(b)
                })
            }
            "&" => Val::Num(f64::from(
                number::to_int32(num(self, x)?) & number::to_int32(num(self, y)?),
            )),
            "|" => Val::Num(f64::from(
                number::to_int32(num(self, x)?) | number::to_int32(num(self, y)?),
            )),
            "^" => Val::Num(f64::from(
                number::to_int32(num(self, x)?) ^ number::to_int32(num(self, y)?),
            )),
            "<<" => Val::Num(f64::from(
                number::to_int32(num(self, x)?).wrapping_shl(number::to_uint32(num(self, y)?) & 31),
            )),
            ">>" => Val::Num(f64::from(
                number::to_int32(num(self, x)?).wrapping_shr(number::to_uint32(num(self, y)?) & 31),
            )),
            ">>>" => Val::Num(f64::from(
                number::to_uint32(num(self, x)?)
                    .wrapping_shr(number::to_uint32(num(self, y)?) & 31),
            )),
            _ => return self.type_error(&format!("unknown operator {op}")),
        })
    }

    fn unary(&mut self, op: &str, v: &Val) -> R {
        Ok(match op {
            "!" => Val::Bool(!v.truthy()),
            "-" => match self.numeric(v)? {
                Val::BigInt(b) => Val::BigInt(Rc::new(b.neg())),
                n => Val::Num(-self.number_of(&n)?),
            },
            "+" => Val::Num(self.number_of(v)?),
            "~" => match self.numeric(v)? {
                Val::BigInt(b) => Val::BigInt(Rc::new(b.not())),
                n => Val::Num(f64::from(!number::to_int32(self.number_of(&n)?))),
            },
            _ => Val::Undef,
        })
    }

    pub fn binary(&mut self, op: &str, x: &Val, y: &Val) -> R {
        Ok(match op {
            "+" => {
                let a = self.primitive(x, "default")?;
                let b = self.primitive(y, "default")?;
                if matches!(a, Val::Str(_)) || matches!(b, Val::Str(_)) {
                    let (sa, sb) = (self.str_of(&a)?, self.str_of(&b)?);
                    let mut out = String::with_capacity(sa.len() + sb.len());
                    out.push_str(&sa);
                    out.push_str(&sb);
                    return self.joined_string(out);
                }
                return self.binary("@+", &a, &b);
            }
            "-" | "*" | "/" | "%" | "**" | "&" | "|" | "^" | "<<" | ">>" | ">>>" | "@+" => {
                let a = self.numeric(x)?;
                let b = self.numeric(y)?;
                match (&a, &b) {
                    (Val::BigInt(p), Val::BigInt(q)) => {
                        return crate::bigints::binary(self, op, p, q);
                    }
                    (Val::BigInt(_), _) | (_, Val::BigInt(_)) => {
                        return self.type_error("can't convert BigInt to number");
                    }
                    _ => self.number_binary(op, &a, &b)?,
                }
            }
            "==" => Val::Bool(self.loose_eq(x, y)?),
            "!=" => Val::Bool(!self.loose_eq(x, y)?),
            "===" => Val::Bool(strict_eq(x, y)),
            "!==" => Val::Bool(!strict_eq(x, y)),
            "<" | ">" | "<=" | ">=" => {
                let a = self.primitive(x, "number")?;
                let b = self.primitive(y, "number")?;
                let ord = if let (Val::Str(p), Val::Str(q)) = (&a, &b) {
                    Some(crate::strings::units(p).cmp(&crate::strings::units(q)))
                } else if matches!(a, Val::BigInt(_)) || matches!(b, Val::BigInt(_)) {
                    crate::bigints::compare(self, &a, &b)?
                } else {
                    let (p, q) = (self.number_of(&a)?, self.number_of(&b)?);
                    p.partial_cmp(&q)
                };
                use std::cmp::Ordering::*;
                Val::Bool(match (op, ord) {
                    (_, None) => false,
                    ("<", Some(o)) => o == Less,
                    (">", Some(o)) => o == Greater,
                    ("<=", Some(o)) => o != Greater,
                    (_, Some(o)) => o != Less,
                })
            }
            "instanceof" => Val::Bool(self.instance_of(x, y)?),
            "in" => {
                let Val::Obj(o) = y else {
                    if let Val::Str(_) = y {
                        let (k, s) = (self.key_of(x)?, self.show(y));
                        let msg = format!("cannot use 'in' operator to search for \"{k}\" in {s}");
                        return self.type_error(&msg);
                    }
                    let msg =
                        format!("right-hand side of 'in' should be an object, got {}", y.type_of());
                    return self.type_error(&msg);
                };
                let k = self.key_of(x)?;
                Val::Bool(self.has(o, &k)?)
            }
            _ => return self.type_error(&format!("unknown operator {op}")),
        })
    }
}

/// SpiderMonkey's message for reading `key` of `o` (null or undefined) that `obj` gave:
/// `can't access property "x", o is null` when the expression has a name, and
/// `can't access property "x" of null` for a literal (measured on a `refrouter` 9.0.4 on 02-11).
fn null_access(obj: &Expr, key: &str, o: &Val) -> String {
    let what = if matches!(o, Val::Undef) { "undefined" } else { "null" };
    let named = match obj {
        Expr::Ident(n, _) => &**n != "undefined",
        Expr::Member { .. } | Expr::Call { .. } | Expr::This => true,
        _ => false,
    };
    if named {
        format!("can't access property \"{key}\", {} is {what}", callee_text(obj))
    } else {
        format!("can't access property \"{key}\" of {what}")
    }
}

/// Whether a loop stops: `break` (of this loop) stops it, `continue` goes on, anything else
/// propagates.
fn loop_ctrl(r: R<()>, label: Option<&Name>) -> R<bool> {
    match r {
        Ok(()) => Ok(false),
        Err(Ctrl::Break(None)) => Ok(true),
        Err(Ctrl::Continue(None)) => Ok(false),
        Err(Ctrl::Continue(Some(l))) if Some(&l) == label => Ok(false),
        Err(Ctrl::Break(Some(l))) if Some(&l) == label => Ok(true),
        Err(e) => Err(e),
    }
}

/// Declares `name` in `env` with `v`.
pub fn declare(env: &EnvRef, name: &str, v: Val, mutable: bool) {
    let mut e = env.borrow_mut();
    match e.vars.iter_mut().find(|(k, _)| &**k == name) {
        Some((_, b)) => {
            b.value = Some(v);
            b.mutable = mutable;
        }
        None => e.vars.push((name.into(), Binding { value: Some(v), mutable, lexical: false })),
    }
}

/// Gives an anonymous function the name it is assigned to.
/// Whether a call's callee is the name `eval` (a direct eval when it is the built-in one).
fn is_eval_name(e: &Expr) -> bool {
    match e {
        Expr::Ident(n, _) => &**n == "eval",
        Expr::At(_, x) => is_eval_name(x),
        _ => false,
    }
}

fn is_builtin_eval(f: &Val) -> bool {
    matches!(f, Val::Obj(o) if matches!(&o.borrow().kind, Kind::Func(Func::Native(_, n)) if &**n == "eval"))
}

/// The scope that holds the `var`s for code running in `env`.
fn var_scope_of(env: &EnvRef) -> EnvRef {
    let mut cur = env.clone();
    loop {
        if cur.borrow().var_scope {
            return cur;
        }
        let parent = cur.borrow().parent.clone();
        match parent {
            Some(p) => cur = p,
            None => return cur,
        }
    }
}

/// Whether `e` is a function or class without a name, which takes the name of what it is bound
/// to (ECMA-262 §8.4.3 IsAnonymousFunctionDefinition, parentheses included).
fn anonymous_definition(e: &Expr) -> bool {
    match e {
        Expr::Func(d) => d.name.is_none() && !d.method,
        Expr::Class(c) => c.name.is_none(),
        Expr::At(_, x) => anonymous_definition(x),
        _ => false,
    }
}

/// Names the function `v` that `e` made after the name `p` binds, when `e` is anonymous.
fn name_function(v: &Val, p: &Pattern, e: &Expr) {
    if !anonymous_definition(e) {
        return;
    }
    let (Val::Obj(f), Pattern::Name(n)) = (v, p) else { return };
    let mut b = f.borrow_mut();
    if !b.is_callable() {
        return;
    }
    if let Some(prop) = b.get_own_mut("name")
        && matches!(&prop.value, Val::Str(s) if s.is_empty())
    {
        prop.value = Val::Str(n.clone());
    }
}

fn pattern_names(p: &Pattern, out: &mut Vec<Name>) {
    match p {
        Pattern::Name(n) => out.push(n.clone()),
        Pattern::Object(props, rest) => {
            for (_, t, _) in props {
                pattern_names(t, out);
            }
            if let Some(r) = rest {
                pattern_names(r, out);
            }
        }
        Pattern::Array(items, rest) => {
            for (t, _) in items.iter().flatten() {
                pattern_names(t, out);
            }
            if let Some(r) = rest {
                pattern_names(r, out);
            }
        }
        Pattern::Expr(_) => {}
    }
}

/// The names `var` declares in `s`, outside nested functions.
fn var_names(s: &Stmt, out: &mut Vec<Name>) {
    match s {
        Stmt::Decl(DeclKind::Var, d) => {
            for (p, _) in d {
                pattern_names(p, out);
            }
        }
        Stmt::If(_, a, b) => {
            var_names(a, out);
            if let Some(b) = b {
                var_names(b, out);
            }
        }
        Stmt::Block(body) => body.iter().for_each(|s| var_names(s, out)),
        Stmt::While(_, b) | Stmt::DoWhile(b, _) | Stmt::Labeled(_, b) | Stmt::With(_, b) => {
            var_names(b, out)
        }
        Stmt::For(init, _, _, b) => {
            if let Some(ForInit::Decl(DeclKind::Var, d)) = init {
                for (p, _) in d {
                    pattern_names(p, out);
                }
            }
            var_names(b, out);
        }
        Stmt::ForIn { decl, target, body, .. } => {
            if *decl == Some(DeclKind::Var) {
                pattern_names(target, out);
            }
            var_names(body, out);
        }
        Stmt::Try(a, c, f) => {
            a.iter().for_each(|s| var_names(s, out));
            if let Some((_, c)) = c {
                c.iter().for_each(|s| var_names(s, out));
            }
            if let Some(f) = f {
                f.iter().for_each(|s| var_names(s, out));
            }
        }
        Stmt::Switch(_, cases) => cases.iter().flat_map(|(_, b)| b).for_each(|s| var_names(s, out)),
        Stmt::Suspends(s) => var_names(s, out),
        _ => {}
    }
}

/// The UTF-16 unit `i` of `s`.
pub fn utf16_at(s: &str, i: usize) -> Option<u16> {
    crate::strings::units(s).get(i).copied()
}
