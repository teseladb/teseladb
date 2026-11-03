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
    /// A missing element of an array (`[1, , 3]`, `delete a[1]`, a longer `length`). It lives only
    /// in the elements of an array: reading one gives `undefined` from the prototypes, and it is
    /// not an own property (ECMA-262 §10.4.2).
    Hole,
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
    /// Whether nothing has changed the object since it was marked: a call's global object that
    /// stays so serves the next call as it is.
    pub pristine: Cell<bool>,
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
        self.0.pristine.set(false);
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
    /// An object made by `DBRef`: its properties are the reference.
    DbRef,
    Symbol(String),
    Code(String),
}

/// What an array iterator gives at each step.
#[derive(Clone, Copy, Debug)]
pub enum ArrayIterKind {
    Keys,
    Values,
    Entries,
}

/// An array iterator: the object it reads (`None` once it is done), the next index and what it
/// gives. It reads `length` at each step, as SpiderMonkey does (measured on a `refrouter` 9.0.4 on
/// 02-11: elements pushed while iterating are visited, and once done it stays done).
#[derive(Debug)]
pub struct ArrayIter {
    pub source: Option<Val>,
    pub pos: usize,
    pub kind: ArrayIterKind,
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
    /// An `arguments` object; in a sloppy function with a plain parameter list its first
    /// elements are its parameters (ECMA-262 §10.4.4.7).
    Arguments(Option<Rc<crate::interp::Mapped>>),
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
    /// A proxy: its target and handler (`None` once revoked), whether it is callable and whether
    /// it is a constructor.
    Proxy(Option<(ObjRef, ObjRef)>, bool, bool),
    /// A generator object.
    Generator(Rc<RefCell<crate::generators::GenState>>),
    /// An async generator object: the interpreter never runs its body (`next` traps), so it
    /// only carries its class (`AsyncGenerator`, measured on a `refrouter` 9.0.4 on 02-11).
    AsyncGenerator,
    /// A promise.
    Promise(Rc<RefCell<crate::promise::PromiseState>>),
    /// An iterator over a snapshot of values (`entries()`, `values()`, `keys()`).
    /// An iterator over values read in advance, and its class (`Map Iterator`…).
    Iter(Rc<RefCell<(Vec<Val>, usize)>>, &'static str),
    /// An array iterator, which reads its array at each step.
    ArrayIter(Rc<RefCell<ArrayIter>>),
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
/// Some elements of an array have attributes of their own (an accessor, a read-only or
/// non-configurable element): those are kept in its properties under their index, over the value
/// the kind holds.
pub const ELEMENTS_OWN: u8 = 8;

/// The built-in properties of a function SpiderMonkey makes when they are first looked up
/// (measured on a `refrouter` 9.0.4 on 02-11: a property added before lists before them, and one
/// looked up moves ahead of those still unmade): `prototype` (of a plain function), `length` and
/// `name`.
pub const LAZY_PROTOTYPE: u8 = 16;
pub const LAZY_LENGTH: u8 = 32;
pub const LAZY_NAME: u8 = 64;
pub const LAZY: u8 = LAZY_PROTOTYPE | LAZY_LENGTH | LAZY_NAME;

fn lazy_bit(key: &str) -> u8 {
    match key {
        "prototype" => LAZY_PROTOTYPE,
        "length" => LAZY_LENGTH,
        "name" => LAZY_NAME,
        _ => 0,
    }
}

/// Makes the lazy built-in `key` of `o` when it has one not made yet (a lookup of it).
pub fn touch(o: &ObjRef, key: &str) {
    if o.borrow().locks & LAZY != 0 {
        o.borrow_mut().resolve(key);
    }
}

/// Makes every lazy built-in of `o`, in their order (an enumeration of its keys).
pub fn touch_all(o: &ObjRef) {
    for key in ["prototype", "length", "name"] {
        touch(o, key);
    }
}

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

    /// Adds the own property `key`: before the lazy built-ins of a function not made yet.
    pub fn insert(&mut self, key: Name, prop: Prop) {
        let at = self.first_lazy();
        self.props.insert(at, (key, prop));
    }

    fn first_lazy(&self) -> usize {
        if self.locks & LAZY == 0 {
            return self.props.len();
        }
        self.props
            .iter()
            .position(|(k, _)| self.locks & lazy_bit(k) != 0)
            .unwrap_or(self.props.len())
    }

    /// Makes the lazy built-in `key` of a function: it moves ahead of those not made yet.
    pub fn resolve(&mut self, key: &str) {
        let bit = lazy_bit(key);
        if self.locks & bit == 0 {
            return;
        }
        self.locks &= !bit;
        if let Some(i) = self.props.iter().position(|(k, _)| &**k == key) {
            let entry = self.props.remove(i);
            let at = self.first_lazy();
            self.props.insert(at, entry);
        }
    }

    pub fn is_callable(&self) -> bool {
        matches!(self.kind, Kind::Func(_) | Kind::Proxy(_, true, _))
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
            Val::Undef | Val::Hole | Val::Null => false,
            Val::Bool(b) => *b,
            Val::Num(n) => *n != 0.0 && !n.is_nan(),
            Val::BigInt(b) => !b.is_zero(),
            Val::Str(s) => !s.is_empty(),
            Val::Sym(_) | Val::Obj(_) => true,
        }
    }

    pub fn type_of(&self) -> &'static str {
        match self {
            Val::Undef | Val::Hole => "undefined",
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
