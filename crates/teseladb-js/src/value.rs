//! The values of the language and its objects. Objects are reference counted; the memory each
//! one holds is charged to the invocation's heap budget while it lives ([`Budget`]).

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use teseladb_bson::Decimal128;

use crate::ast::{ClassDef, FuncDef};
use crate::interp::{Ctrl, Interp};

pub use crate::ast::Name;

/// A value.
#[derive(Clone, Debug)]
pub enum Val {
    Undef,
    Null,
    Bool(bool),
    Num(f64),
    BigInt(Rc<crate::bigint::BigInt>),
    Str(Name),
    /// A symbol: its description and its identity.
    Sym(Rc<(Name, u64)>),
    Obj(ObjRef),
}

/// The bytes the live objects of an invocation hold, against its limit.
#[derive(Debug)]
pub struct Budget {
    pub used: Cell<usize>,
    pub limit: usize,
    /// Whether a call changed an object of the shared globals ([`ObjCell::shared`]): the globals
    /// are then built again before the next call.
    pub touched: Cell<bool>,
}

impl Budget {
    /// Charges `n` bytes; `false` when the limit is passed.
    pub fn charge(&self, n: usize) -> bool {
        let total = self.used.get().saturating_add(n);
        self.used.set(total);
        total <= self.limit
    }

    pub fn release(&self, n: usize) {
        self.used.set(self.used.get().saturating_sub(n));
    }
}

/// A reference to an object.
#[derive(Clone, Debug)]
pub struct ObjRef(pub Rc<ObjCell>);

#[derive(Debug)]
pub struct ObjCell {
    pub obj: RefCell<Obj>,
    pub charged: Cell<usize>,
    pub budget: Rc<Budget>,
    /// Whether the object belongs to the global objects a program's calls share.
    pub shared: Cell<bool>,
}

impl Drop for ObjCell {
    fn drop(&mut self) {
        self.budget.release(self.charged.get());
    }
}

impl ObjRef {
    pub fn ptr_eq(&self, other: &ObjRef) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }

    pub fn borrow(&self) -> std::cell::Ref<'_, Obj> {
        self.0.obj.borrow()
    }

    /// The object to change; changing a shared global object marks the globals as touched.
    pub fn borrow_mut(&self) -> std::cell::RefMut<'_, Obj> {
        if self.0.shared.get() {
            self.0.budget.touched.set(true);
        }
        self.0.obj.borrow_mut()
    }
}

/// A property.
#[derive(Clone, Debug)]
pub struct Prop {
    pub value: Val,
    pub getter: Option<ObjRef>,
    pub setter: Option<ObjRef>,
    pub enumerable: bool,
    pub writable: bool,
    pub configurable: bool,
}

impl Prop {
    pub fn data(value: Val) -> Prop {
        Prop {
            value,
            getter: None,
            setter: None,
            enumerable: true,
            writable: true,
            configurable: true,
        }
    }

    pub fn hidden(value: Val) -> Prop {
        Prop {
            value,
            getter: None,
            setter: None,
            enumerable: false,
            writable: true,
            configurable: true,
        }
    }

    pub fn is_accessor(&self) -> bool {
        self.getter.is_some() || self.setter.is_some()
    }
}

/// A native function: the interpreter, `this`, the arguments.
pub type NativeFn = fn(&mut Interp, &Val, &[Val]) -> Result<Val, Ctrl>;

/// A function object's behavior.
#[derive(Clone, Debug)]
pub enum Func {
    Closure {
        def: Rc<FuncDef>,
        env: crate::interp::EnvRef,
        /// `this` of an arrow function.
        this: Option<Val>,
        /// The object whose prototype `super` reads (methods and constructors).
        home: Option<ObjRef>,
        /// The class whose instance fields a constructor sets.
        class: Option<Rc<ClassDef>>,
        class_env: Option<crate::interp::EnvRef>,
        derived: bool,
    },
    Native(NativeFn, Name),
    Bound(ObjRef, Val, Vec<Val>),
}

/// A value of a BSON type RefDb shows to JavaScript with its own constructor.
#[derive(Clone, Debug, PartialEq)]
pub enum Bson {
    Long(i64),
    Int(i32),
    Decimal(Decimal128),
    ObjectId([u8; 12]),
    BinData(u8, Vec<u8>),
    Timestamp(u32, u32),
    MinKey,
    MaxKey,
    DbPointer(String, [u8; 12]),
    Symbol(String),
    Code(String),
}

/// What kind of object.
#[derive(Clone, Debug)]
pub enum Kind {
    Ordinary,
    Array(Vec<Val>),
    Func(Func),
    Error,
    Date(f64),
    Bool(bool),
    Num(f64),
    BigInt(Rc<crate::bigint::BigInt>),
    Str(Name),
    Map(Vec<(Val, Val)>),
    Set(Vec<Val>),
    RegExp(Name, Name),
    Bson(Bson),
    Arguments,
    /// A `WeakMap`: its entries by key identity.
    WeakMap(Vec<(ObjRef, Val)>),
    /// A `WeakSet`.
    WeakSet(Vec<ObjRef>),
    /// A `WeakRef` and its target.
    WeakRef(ObjRef),
    /// A lazy iterator helper (`map`, `filter`…).
    Helper(Rc<RefCell<crate::iterators::Helper>>),
    /// An `ArrayBuffer`.
    ArrayBuffer(crate::typed::BufRef),
    /// A typed array.
    Typed(crate::typed::View),
    /// A `DataView`.
    DataView(crate::typed::View),
    /// A proxy: its target and handler (`None` once revoked) and whether it is callable.
    Proxy(Option<(ObjRef, ObjRef)>, bool),
    /// A generator object.
    Generator(Rc<RefCell<crate::generators::GenState>>),
    /// A promise.
    Promise(Rc<RefCell<crate::promise::PromiseState>>),
    /// An iterator over a snapshot of values (`entries()`, `values()`, `keys()`).
    Iter(Rc<RefCell<(Vec<Val>, usize)>>),
    /// A `Symbol` object (`Object(Symbol())`): its symbol.
    Symbol(Val),
}

/// An object: its prototype, its own properties in insertion order and its kind.
#[derive(Debug)]
pub struct Obj {
    pub proto: Option<ObjRef>,
    pub props: Vec<(Name, Prop)>,
    pub kind: Kind,
    pub extensible: bool,
    /// What `Object.seal` and `Object.freeze` did to the elements an array holds in its kind
    /// ([`LENGTH_FIXED`], [`ELEMENTS_SEALED`], [`ELEMENTS_FROZEN`]).
    pub locks: u8,
}

/// An array's `length` is read-only.
pub const LENGTH_FIXED: u8 = 1;
/// An array's elements cannot be deleted.
pub const ELEMENTS_SEALED: u8 = 2;
/// An array's elements are read-only.
pub const ELEMENTS_FROZEN: u8 = 4;

/// Whether `key` is the key of a symbol (`@@description:id`), not an internal slot (`@@new`).
pub fn is_symbol_key(key: &str) -> bool {
    key.strip_prefix("@@")
        .and_then(|k| k.rsplit_once(':'))
        .is_some_and(|(_, id)| !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()))
}

impl Obj {
    pub fn get_own(&self, key: &str) -> Option<&Prop> {
        self.props.iter().find(|(k, _)| &**k == key).map(|(_, p)| p)
    }

    pub fn get_own_mut(&mut self, key: &str) -> Option<&mut Prop> {
        self.props.iter_mut().find(|(k, _)| &**k == key).map(|(_, p)| p)
    }

    pub fn remove(&mut self, key: &str) {
        self.props.retain(|(k, _)| &**k != key);
    }

    pub fn is_callable(&self) -> bool {
        matches!(self.kind, Kind::Func(_) | Kind::Proxy(_, true))
    }
}

/// An array index from a key, as ECMAScript reads `"3"` (not `"03"`).
pub fn array_index(key: &str) -> Option<usize> {
    if key.is_empty() || (key.len() > 1 && key.starts_with('0')) || key.len() > 10 {
        return None;
    }
    let n: u64 = key.parse().ok()?;
    if n >= u64::from(u32::MAX) {
        return None;
    }
    usize::try_from(n).ok()
}

impl Val {
    pub fn str(s: &str) -> Val {
        Val::Str(s.into())
    }

    pub fn as_obj(&self) -> Option<&ObjRef> {
        match self {
            Val::Obj(o) => Some(o),
            _ => None,
        }
    }

    pub fn is_nullish(&self) -> bool {
        matches!(self, Val::Undef | Val::Null)
    }

    pub fn truthy(&self) -> bool {
        match self {
            Val::Undef | Val::Null => false,
            Val::Bool(b) => *b,
            Val::Num(n) => *n != 0.0 && !n.is_nan(),
            Val::BigInt(b) => !b.is_zero(),
            Val::Str(s) => !s.is_empty(),
            Val::Sym(_) | Val::Obj(_) => true,
        }
    }

    pub fn type_of(&self) -> &'static str {
        match self {
            Val::Undef => "undefined",
            Val::Null => "object",
            Val::Bool(_) => "boolean",
            Val::Num(_) => "number",
            Val::BigInt(_) => "bigint",
            Val::Str(_) => "string",
            Val::Sym(_) => "symbol",
            Val::Obj(o) => {
                if o.borrow().is_callable() {
                    "function"
                } else {
                    "object"
                }
            }
        }
    }

    pub fn is_callable(&self) -> bool {
        matches!(self, Val::Obj(o) if o.borrow().is_callable())
    }
}

/// `===`.
pub fn strict_eq(a: &Val, b: &Val) -> bool {
    match (a, b) {
        (Val::Undef, Val::Undef) | (Val::Null, Val::Null) => true,
        (Val::Bool(x), Val::Bool(y)) => x == y,
        (Val::Num(x), Val::Num(y)) => x == y,
        (Val::BigInt(x), Val::BigInt(y)) => x == y,
        (Val::Str(x), Val::Str(y)) => x == y,
        (Val::Sym(x), Val::Sym(y)) => x.1 == y.1 && x.0 == y.0,
        (Val::Obj(x), Val::Obj(y)) => x.ptr_eq(y),
        _ => false,
    }
}

/// SameValueZero (`includes`, `Map`, `Set`).
pub fn same_value_zero(a: &Val, b: &Val) -> bool {
    match (a, b) {
        (Val::Num(x), Val::Num(y)) if x.is_nan() && y.is_nan() => true,
        _ => strict_eq(a, b),
    }
}
