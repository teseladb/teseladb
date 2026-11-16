//! The JSON object model of the HTTP API.
//!
//! A JSON object is a type that implements [`JsonBase`] and holds named members: each
//! member is a [`JsonElement`] (one value) or a [`JsonListTemplate`] ([`JsonList`],
//! [`JsonChunkedList`]), all implementing [`JsonBaseElement`]. Members are named and
//! flagged mandatory at construction ([`json_base_add`]); a member is written only when
//! set, and [`JsonBase::is_verify`] checks that every mandatory member is set. [`Jsonable`] renders a value
//! to a string or streams it to an output stream. [`JsonReturnType`] is what JSON handlers
//! return: a rendered body or a [`BodyWriter`] that streams it, as produced by
//! [`stream_range_as_array`] and [`stream_object`].
//!
//! Objects hold no addresses of their own members, so they can move freely: they hand
//! their members out on demand, in registration order.

use crate::engine::core::chunked_fifo::ChunkedFifo;
use crate::engine::core::do_with::do_with;
use crate::engine::core::future::{ExceptionPtr, Future, make_exception_future, make_ready_future};
use crate::engine::core::iostream::{OutputStream, OutputStreamState, StateRef};
use crate::engine::core::loop_::do_for_each;
use crate::engine::core::sstring::Sstring;
use crate::engine::json::formatter::{Formatter, JsonWrite, State, ToJson};

/// The data every JSON element carries: its name, whether it has been set, and whether
/// it is mandatory.
///
/// An object with a mandatory element that is not set is not valid (see
/// [`JsonBase::is_verify`]).
#[derive(Clone, Debug)]
pub struct JsonBaseElementData {
    /// The element name.
    pub _name: String,
    /// Whether the element is mandatory.
    pub _mandatory: bool,
    /// Whether the element was set.
    pub _set: bool,
}

impl JsonBaseElementData {
    /// An unnamed, optional, unset element.
    pub const fn new() -> Self {
        JsonBaseElementData { _name: String::new(), _mandatory: false, _set: false }
    }

    /// Copies the "set" state of `o`; names and mandatory are never changed after creation.
    pub fn assign_from(&mut self, o: &JsonBaseElementData) {
        // Names and mandatory are never changed after creation
        self._set = o._set;
    }
}

impl Default for JsonBaseElementData {
    fn default() -> Self {
        Self::new()
    }
}

/// A named member of a JSON object: its common data, its JSON rendering and streaming.
pub trait JsonBaseElement {
    /// The common data.
    fn base(&self) -> &JsonBaseElementData;
    /// The common data, mutable.
    fn base_mut(&mut self) -> &mut JsonBaseElementData;

    /// Returns whether the element is valid: `true` unless it is mandatory and not set.
    fn is_verify(&self) -> bool {
        let b = self.base();
        !(b._mandatory && !b._set)
    }

    /// Returns the element's value formatted as JSON.
    ///
    /// # Errors
    ///
    /// Returns an error if the value cannot be formatted.
    fn to_string(&self) -> Result<Sstring, ExceptionPtr>;

    /// Writes the internal value to the stream in json format.
    ///
    /// The returned future holds the stream's state, so dropping the stream before it
    /// resolves aborts. A list member streams its elements from where they are, without a
    /// copy, so the future also reads the element after this call returns.
    ///
    /// # Safety
    /// The element stays alive, in place and unchanged until the returned future resolves
    /// ([`JsonBase::base_write`] guarantees it by owning the object for the whole write).
    unsafe fn write(&self, s: &mut OutputStreamState<u8>) -> Future<()>;
}

/// A JSON element holding a value of type `T`.
///
/// It adds the value to [`JsonBaseElementData`] and formats it with the JSON
/// [`Formatter`] rules of `T`.
#[derive(Clone, Debug)]
pub struct JsonElement<T> {
    base: JsonBaseElementData,
    _value: T,
}

impl<T: Default> Default for JsonElement<T> {
    fn default() -> Self {
        JsonElement { base: JsonBaseElementData::new(), _value: T::default() }
    }
}

impl<T> JsonElement<T> {
    /// Sets the value to `new_value` and marks the element as set.
    ///
    /// Returns `self`, so calls can be chained.
    pub fn set<C: Into<T>>(&mut self, new_value: C) -> &mut Self {
        self._value = new_value.into();
        self.base._set = true;
        self
    }

    /// Returns the value.
    #[inline]
    pub fn get(&self) -> &T {
        &self._value
    }
}

impl<T: ToJson + JsonWrite + Clone + 'static> JsonBaseElement for JsonElement<T> {
    fn base(&self) -> &JsonBaseElementData {
        &self.base
    }
    fn base_mut(&mut self) -> &mut JsonBaseElementData {
        &mut self.base
    }
    /// Returns the value formatted as JSON.
    fn to_string(&self) -> Result<Sstring, ExceptionPtr> {
        self._value.to_json()
    }
    unsafe fn write(&self, s: &mut OutputStreamState<u8>) -> Future<()> {
        // The value is formatted where it is, without a copy, before this call returns; the
        // formatter writes the same text.
        match self._value.to_json() {
            Ok(j) => s.write_sstring(&j),
            Err(e) => make_exception_future(e),
        }
    }
}

/// The container of a `JsonListTemplate`.
pub trait JsonListContainer<T>: Default {
    /// Appends an element.
    fn push_back(&mut self, x: T);
    /// Removes all elements.
    fn clear(&mut self);
    /// The iterator of the elements (the container's `const_iterator`).
    type Iter<'a>: Iterator<Item = &'a T>
    where
        Self: 'a,
        T: 'a;
    /// Iterates over the elements.
    fn iter_elements(&self) -> Self::Iter<'_>;
}

impl<T> JsonListContainer<T> for Vec<T> {
    type Iter<'a>
        = std::slice::Iter<'a, T>
    where
        T: 'a;
    #[inline]
    fn push_back(&mut self, x: T) {
        self.push(x);
    }
    #[inline]
    fn clear(&mut self) {
        Vec::clear(self);
    }
    #[inline]
    fn iter_elements(&self) -> std::slice::Iter<'_, T> {
        self.iter()
    }
}

impl<T, const N: usize> JsonListContainer<T> for ChunkedFifo<T, N> {
    type Iter<'a>
        = crate::engine::core::chunked_fifo::Iter<'a, T, N>
    where
        T: 'a;
    #[inline]
    fn push_back(&mut self, x: T) {
        ChunkedFifo::push_back(self, x);
    }
    #[inline]
    fn clear(&mut self) {
        ChunkedFifo::clear(self);
    }
    #[inline]
    fn iter_elements(&self) -> crate::engine::core::chunked_fifo::Iter<'_, T, N> {
        self.iter()
    }
}

/// A JSON array element backed by a container of type `Container`.
///
/// The container's requirements (iteration and `push_back`) are the
/// [`JsonListContainer`] trait. As a [`JsonBaseElement`], it can be a member of a JSON
/// object.
#[derive(Clone)]
pub struct JsonListTemplate<T, Container: JsonListContainer<T>> {
    base: JsonBaseElementData,
    /// The elements.
    pub _elements: Container,
    _t: std::marker::PhantomData<T>,
}

impl<T, Container: JsonListContainer<T>> Default for JsonListTemplate<T, Container> {
    fn default() -> Self {
        JsonListTemplate {
            base: JsonBaseElementData::new(),
            _elements: Container::default(),
            _t: std::marker::PhantomData,
        }
    }
}

impl<T, Container: JsonListContainer<T>> JsonListTemplate<T, Container> {
    /// Appends `element` to the end of the list and marks the list as set.
    pub fn push(&mut self, element: T) {
        self.base._set = true;
        self._elements.push_back(element);
    }

    /// Replaces the list's elements with the items of `list`, in order.
    ///
    /// The list is marked as set when `list` yields at least one item. Returns `self`, so
    /// calls can be chained.
    pub fn assign<I: IntoIterator<Item = T>>(&mut self, list: I) -> &mut Self {
        self._elements.clear();
        for i in list {
            self.push(i);
        }
        self
    }
}

/// An element of a list written in place by reference: it formats the element with
/// [`ToJson`] and writes the result to the stream, without copying the element.
///
/// Only [`JsonListTemplate`]'s `write` makes one, under the contract of
/// [`JsonBaseElement::write`]: the list, and so the element, outlives the write.
struct ElementRef<T: 'static>(&'static T);

impl<T: ToJson + 'static> crate::engine::json::formatter::JsonWriteIn for ElementRef<T> {
    #[inline]
    fn write_in(self, stream: &mut OutputStreamState<u8>, _: State) -> Future<()> {
        match self.0.to_json() {
            Ok(j) => stream.write_sstring(&j),
            Err(e) => make_exception_future(e),
        }
    }
}

impl<T: ToJson + JsonWrite + Clone + 'static, Container: JsonListContainer<T> + 'static>
    JsonBaseElement for JsonListTemplate<T, Container>
{
    fn base(&self) -> &JsonBaseElementData {
        &self.base
    }
    fn base_mut(&mut self) -> &mut JsonBaseElementData {
        &mut self.base
    }
    /// Returns the value formatted as JSON.
    fn to_string(&self) -> Result<Sstring, ExceptionPtr> {
        Formatter::to_json_range(State::Array, self._elements.iter_elements())
    }
    /// The elements are written one by one, as they are formatted, so a long list
    /// ([`JsonChunkedList`]) never becomes one contiguous string.
    unsafe fn write(&self, s: &mut OutputStreamState<u8>) -> Future<()> {
        // SAFETY: the contract of `write`: the list stays alive, in place and unchanged
        // until the returned future resolves, so the borrow may last that long while the
        // continuations iterate `_elements`.
        let me: &'static Self = unsafe { &*(self as *const Self) };
        Formatter::write_range(s, State::Array, me._elements.iter_elements().map(ElementRef))
    }
}

/// A JSON array member backed by a `Vec`.
pub type JsonList<T> = JsonListTemplate<T, Vec<T>>;

/// A JSON array member backed by a chunked FIFO, for long lists streamed without one large allocation.
pub type JsonChunkedList<T> = JsonListTemplate<T, ChunkedFifo<T>>;

/// A value that can render itself as json.
pub trait Jsonable {
    /// The json representation.
    fn to_json(&self) -> Result<Sstring, ExceptionPtr>;

    /// Writes the object to the output stream `s`.
    ///
    /// The default implementation writes the result of [`Jsonable::to_json`]; objects
    /// override it to stream their members instead, usually with
    /// `(*self).base_write(s)` ([`JsonBase::base_write`]).
    ///
    /// The object comes boxed, by value, so the returned future owns it for as long as
    /// the write needs it. The future holds the stream's state: dropping the stream before
    /// it resolves aborts.
    fn write(self: Box<Self>, s: &mut OutputStreamState<u8>) -> Future<()> {
        match self.to_json() {
            Ok(j) => s.write_sstring(&j),
            Err(e) => make_exception_future(e),
        }
    }
}

/// A JSON object made of registered member elements.
///
/// Listing its members lets the object format itself as JSON and lets callers iterate
/// over them without knowing the members in advance, a simple form of reflection.
///
/// The object does not store the addresses of its members, which would break when it
/// moves: it gives its registered members on demand, in registration order (`element(i)`
/// is the `i`-th member); their names and mandatory flags are recorded in the members at
/// construction by [`json_base_add`].
pub trait JsonBase {
    /// The number of registered members.
    fn element_count(&self) -> usize;

    /// The `i`-th registered member, `None` past the end.
    fn element(&self, i: usize) -> Option<&dyn JsonBaseElement>;

    /// Returns whether every mandatory member is set.
    fn is_verify(&self) -> bool {
        (0..self.element_count()).filter_map(|i| self.element(i)).all(|e| e.is_verify())
    }

    /// The members formatted as a json object.
    fn base_to_json(&self) -> Result<Sstring, ExceptionPtr> {
        let mut res = JsonBuilder::new();
        for i in 0..self.element_count() {
            res.add_element(self.element(i))?;
        }
        Ok(res.as_json())
    }

    /// Streams the object to the output stream `s`: its set members, one at a time, then
    /// the closing brace.
    ///
    /// The object is taken by value and kept, with the writer's state, in one `do_with`
    /// until the returned future resolves, so its members can be streamed from where they
    /// are. The future holds the stream's state: dropping the stream before it resolves
    /// aborts.
    fn base_write(self, s: &mut OutputStreamState<u8>) -> Future<()>
    where
        Self: Sized + 'static,
    {
        let n = self.element_count();
        do_with((self, JsonStreamBuilder::new(s)), move |st: &mut (Self, JsonStreamBuilder)| {
            let st: *mut (Self, JsonStreamBuilder) = st;
            // SAFETY: do_with keeps the object and the builder alive and in place until the
            // chain resolves, nothing else reaches them, and the steps run one at a time.
            do_for_each(0..n, move |i| unsafe { JsonStreamBuilder::add_element(st, i) })
                // SAFETY: as above; the members are all written.
                .then(move |()| unsafe { (*st).1.done() })
        })
    }
}

/// Registers `element` as a member of an object, with the name `name` and the mandatory
/// flag `mandatory`.
///
/// Called at construction: the object's [`JsonBase::element`] lists the member, and this
/// records its name and whether it is mandatory.
#[inline]
pub fn json_base_add(element: &mut dyn JsonBaseElement, name: &str, mandatory: bool) {
    let b = element.base_mut();
    b._mandatory = mandatory;
    b._name = name.to_string();
}

/// An empty JSON result, so that a handler with nothing to return can still return a
/// [`Jsonable`] value.
#[derive(Default)]
pub struct JsonVoid;

impl Jsonable for JsonVoid {
    fn to_json(&self) -> Result<Sstring, ExceptionPtr> {
        Ok(Sstring::new())
    }

    /// Writes nothing and closes the stream `s`.
    fn write(self: Box<Self>, s: &mut OutputStreamState<u8>) -> Future<()> {
        s.close()
    }
}

/// A body writer that can be copied: copying it copies the callable. Every
/// `FnMut(OutputStream<u8>) -> Future<()> + Clone` closure is one.
pub trait BodyWriterFn: FnMut(OutputStream<u8>) -> Future<()> {
    /// A boxed copy of the callable.
    fn clone_box(&self) -> BodyWriter;
}

impl<F: FnMut(OutputStream<u8>) -> Future<()> + Clone + 'static> BodyWriterFn for F {
    #[inline]
    fn clone_box(&self) -> BodyWriter {
        Box::new(self.clone())
    }
}

impl Clone for Box<dyn BodyWriterFn> {
    #[inline]
    fn clone(&self) -> Self {
        (**self).clone_box()
    }
}

/// The body writer of a `JsonReturnType`.
pub type BodyWriter = Box<dyn BodyWriterFn>;

/// The result of a JSON handler: either a JSON-formatted string or a writer that streams
/// the body.
///
/// [`JsonReturnType::new`] formats any [`ToJson`] value, so a handler returning the
/// string `hello` produces the JSON string `"hello"` (quoted), not `hello`.
/// [`JsonReturnType::from_body_writer`] wraps a streaming [`BodyWriter`].
#[derive(Default, Clone)]
pub struct JsonReturnType {
    /// The formatted result.
    pub _res: Sstring,
    /// The streaming writer, when the result is streamed.
    pub _body_writer: Option<BodyWriter>,
}

impl JsonReturnType {
    /// A streamed result.
    pub fn from_body_writer(body_writer: BodyWriter) -> Self {
        JsonReturnType { _res: Sstring::new(), _body_writer: Some(body_writer) }
    }

    /// A formatted result.
    pub fn new<T: ToJson + ?Sized>(res: &T) -> Result<Self, ExceptionPtr> {
        Ok(JsonReturnType { _res: res.to_json()?, _body_writer: None })
    }
}

/// Captures the range `val` and returns a [`BodyWriter`] that streams it as a JSON array,
/// each item mapped by `fun` before it is written.
///
/// For example, to stream the keys of a map `res`:
///
/// `make_ready_future(JsonReturnType::from_body_writer(stream_range_as_array(res, |i| i.0.clone())))`
pub fn stream_range_as_array<Container, Func, R>(val: Container, fun: Func) -> BodyWriter
where
    Container: IntoIterator + Clone + 'static,
    Container::Item: 'static,
    Func: Fn(&Container::Item) -> R + Clone + 'static,
    R: JsonWrite + 'static,
{
    // The closure moves the container and the function into its do_with; a second call
    // finds them moved out and writes an empty array.
    let mut val = Some(val);
    let mut fun = Some(fun);
    Box::new(move |s: OutputStream<u8>| {
        let val = val.take().into_iter().flatten();
        let f = fun.take();
        do_with((s, true), move |st: &mut (OutputStream<u8>, bool)| {
            let sp: *mut (OutputStream<u8>, bool) = st;
            // SAFETY: do_with keeps the stream and the flag alive for the whole chain.
            unsafe { &mut (*sp).0 }
                .write_str("[")
                .then(move |()| {
                    do_for_each(val, move |v: Container::Item| {
                        // SAFETY: do_with keeps the stream and the flag alive for the whole
                        // chain, and the steps run one at a time.
                        let (s, first) = unsafe { (&mut (*sp).0, &mut (*sp).1) };
                        let fut = if *first { make_ready_future(()) } else { s.write_str(", ") };
                        *first = false;
                        match &f {
                            Some(f) => {
                                let r = f(&v);
                                // SAFETY: do_with keeps the stream alive; the separator write
                                // has finished with it when this continuation runs.
                                fut.then(move |()| r.write_json(unsafe { &mut (*sp).0 }))
                            }
                            None => fut,
                        }
                    })
                })
                .then(move |()| unsafe { &mut (*sp).0 }.write_str("]"))
                .finally(move || unsafe { &mut (*sp).0 }.close())
        })
    })
}

/// Captures the object `val` and returns a [`BodyWriter`] that streams it as JSON.
///
/// For example:
/// `make_ready_future(JsonReturnType::from_body_writer(stream_object(res)))`
pub fn stream_object<T: JsonWrite + Clone + 'static>(val: T) -> BodyWriter {
    // The closure moves the object into its do_with; a second call finds it moved out.
    let mut val = Some(val);
    Box::new(move |s: OutputStream<u8>| {
        let val = val.take();
        do_with(s, move |s: &mut OutputStream<u8>| {
            let sp: *mut OutputStream<u8> = s;
            let written = match val {
                Some(v) => v.write_json(s),
                None => make_ready_future(()),
            };
            // SAFETY: do_with keeps the stream alive for the whole chain.
            written.finally(move || unsafe { (*sp).close() })
        })
    })
}

/// Builds the JSON text of an object in memory, one member at a time.
struct JsonBuilder {
    result: String,
    first: bool,
}

const OPEN: &str = "{";
const CLOSE: &str = "}";

impl JsonBuilder {
    fn new() -> Self {
        let mut result = String::new();
        result.push_str(OPEN);
        JsonBuilder { result, first: true }
    }

    /// Appends the member `name` with the already formatted JSON value `str`.
    fn add(&mut self, name: &str, str: &str) {
        if self.first {
            self.first = false;
        } else {
            self.result.push_str(", ");
        }
        self.result.push('"');
        self.result.push_str(name);
        self.result.push_str("\": ");
        self.result.push_str(str);
    }

    /// Appends `element` under its name when it is set; does nothing for `None` or an
    /// unset element.
    ///
    /// # Errors
    ///
    /// Returns a runtime error naming the member, with the formatting error nested in
    /// it, if the element cannot be formatted.
    fn add_element(&mut self, element: Option<&dyn JsonBaseElement>) -> Result<(), ExceptionPtr> {
        let element = match element {
            Some(e) if e.base()._set => e,
            _ => return Ok(()),
        };
        match element.to_string() {
            Ok(s) => {
                self.add(&element.base()._name, s.as_str());
                Ok(())
            }
            // Fail with a runtime error naming the field, with the cause nested in it.
            Err(e) => Err(ExceptionPtr::with_nested(
                crate::engine::core::future::RuntimeError(format!(
                    "Json generation failed for field: {}",
                    element.base()._name
                )),
                e,
            )),
        }
    }

    /// Closes the object and returns its JSON text, leaving the builder empty.
    fn as_json(&mut self) -> Sstring {
        self.result.push_str(CLOSE);
        Sstring::from(std::mem::take(&mut self.result))
    }
}

/// Writes the JSON text of an object to an output stream, one member at a time.
///
/// It lives next to the object in the `do_with` of [`JsonBase::base_write`] and holds the
/// stream's state (see `StateRef`), so the stream cannot be destroyed under it.
struct JsonStreamBuilder {
    first: bool,
    open: bool,
    _s: StateRef<OutputStreamState<u8>>,
}

impl JsonStreamBuilder {
    fn new(s: &OutputStreamState<u8>) -> Self {
        JsonStreamBuilder { first: true, open: false, _s: s.hold() }
    }

    /// Writes the `i`-th member of the object of `st` under `name`.
    ///
    /// # Safety
    /// `st` is the `do_with` state of `JsonBase::base_write`: alive and in place until the
    /// returned future resolves, and reached by nothing else meanwhile.
    unsafe fn add<O: JsonBase + 'static>(
        st: *mut (O, JsonStreamBuilder),
        name: String,
        i: usize,
    ) -> Future<()> {
        // SAFETY: the contract above; this reference ends before the continuations run.
        let b = unsafe { &mut (*st).1 };
        // SAFETY: `_s` keeps the stream's state alive; the previous write has resolved and
        // the stream's operations run one at a time, so no other reference to it is live.
        let s = unsafe { &mut *b._s.ptr() };
        if !b.open {
            b.open = true;
            // SAFETY: the contract above covers the continuation.
            return s.write_str(OPEN).then(move |()| unsafe { Self::add(st, name, i) });
        }
        let mut prefix = String::with_capacity(name.len() + 4);
        if !b.first {
            prefix.push(',');
        }
        prefix.push('"');
        prefix.push_str(&name);
        prefix.push_str("\":");
        s.write_str(&prefix).then(move |()| {
            // SAFETY: the state is alive (the contract above) and its two members are
            // borrowed apart; the member is looked up again in the object, where it is now.
            // The object stays in the state, unchanged, until the member's write resolves,
            // as `JsonBaseElement::write` requires.
            unsafe {
                let (obj, b) = (&(*st).0, &mut (*st).1);
                b.first = false;
                match obj.element(i) {
                    Some(e) => e.write(&mut *b._s.ptr()),
                    None => make_ready_future(()),
                }
            }
        })
    }

    /// Writes the `i`-th member of the object of `st` when it is set.
    ///
    /// # Safety
    /// As `JsonStreamBuilder::add`.
    unsafe fn add_element<O: JsonBase + 'static>(
        st: *mut (O, JsonStreamBuilder),
        i: usize,
    ) -> Future<()> {
        // SAFETY: the object is alive (the contract above); this reference ends here.
        match unsafe { (*st).0.element(i) } {
            Some(e) if e.base()._set => {
                let name = e.base()._name.clone();
                // SAFETY: the same contract.
                unsafe { Self::add(st, name, i) }
            }
            _ => make_ready_future(()),
        }
    }

    fn done(&mut self) -> Future<()> {
        // SAFETY: `_s` keeps the stream's state alive; the members are all written and
        // nothing else uses the stream.
        unsafe { &mut *self._s.ptr() }.write_str(CLOSE)
    }
}
