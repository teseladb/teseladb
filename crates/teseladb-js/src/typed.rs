//! `ArrayBuffer`, the typed arrays and `DataView` (ECMA-262 §25.1, §23.2, §25.3), with the
//! messages of SpiderMonkey measured on a `refrouter` 9.0.4 on 02-11.

use std::cell::RefCell;
use std::rc::Rc;

use crate::bigint::BigInt;
use crate::builtins::{arg, ctor, method, new_target, to_string_tag};
use crate::interp::{Interp, R};
use crate::value::*;

/// The bytes of an `ArrayBuffer`.
#[derive(Debug)]
pub struct Buf {
    pub data: Vec<u8>,
    /// The `maxByteLength` of a resizable buffer.
    pub max: Option<usize>,
    pub detached: bool,
}

pub type BufRef = Rc<RefCell<Buf>>;

/// The element type of a typed array.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Ty {
    I8,
    U8,
    U8C,
    I16,
    U16,
    I32,
    U32,
    F16,
    F32,
    F64,
    I64,
    U64,
}

const TYPES: [(Ty, &str); 12] = [
    (Ty::I8, "Int8Array"),
    (Ty::U8, "Uint8Array"),
    (Ty::U8C, "Uint8ClampedArray"),
    (Ty::I16, "Int16Array"),
    (Ty::U16, "Uint16Array"),
    (Ty::I32, "Int32Array"),
    (Ty::U32, "Uint32Array"),
    (Ty::F16, "Float16Array"),
    (Ty::F32, "Float32Array"),
    (Ty::F64, "Float64Array"),
    (Ty::I64, "BigInt64Array"),
    (Ty::U64, "BigUint64Array"),
];

impl Ty {
    pub fn size(self) -> usize {
        match self {
            Ty::I8 | Ty::U8 | Ty::U8C => 1,
            Ty::I16 | Ty::U16 | Ty::F16 => 2,
            Ty::I32 | Ty::U32 | Ty::F32 => 4,
            Ty::F64 | Ty::I64 | Ty::U64 => 8,
        }
    }

    pub fn name(self) -> &'static str {
        TYPES.iter().find(|(t, _)| *t == self).map_or("", |(_, n)| n)
    }

    fn is_big(self) -> bool {
        matches!(self, Ty::I64 | Ty::U64)
    }
}

/// A typed array or a `DataView`: a window on a buffer.
#[derive(Clone, Debug)]
pub struct View {
    pub ty: Ty,
    pub buf: BufRef,
    pub offset: usize,
    /// The element count, `None` for a view that follows a resizable buffer's length.
    pub len: Option<usize>,
}

impl View {
    /// Whether the view lies outside its buffer: detached, or shrunk below the view's end.
    pub fn out_of_bounds(&self) -> bool {
        let b = self.buf.borrow();
        b.detached
            || self.offset > b.data.len()
            || self.len.is_some_and(|n| self.offset + n * self.ty.size() > b.data.len())
    }

    /// The current element count (zero once the buffer is detached or too short).
    pub fn length(&self) -> usize {
        let b = self.buf.borrow();
        if b.detached {
            return 0;
        }
        let avail = b.data.len().saturating_sub(self.offset) / self.ty.size();
        match self.len {
            Some(n) if self.offset + n * self.ty.size() <= b.data.len() => n,
            Some(_) => 0,
            None => avail,
        }
    }
}

fn f16_to_f64(h: u16) -> f64 {
    let sign = if h & 0x8000 != 0 { -1.0 } else { 1.0 };
    let exp = i32::from((h >> 10) & 0x1f);
    let frac = f64::from(h & 0x3ff);
    sign * match exp {
        0 => frac * 2f64.powi(-24),
        31 if frac == 0.0 => f64::INFINITY,
        31 => f64::NAN,
        e => (1.0 + frac / 1024.0) * 2f64.powi(e - 15),
    }
}

fn f64_to_f16(x: f64) -> u16 {
    if x.is_nan() {
        return 0x7e00;
    }
    let sign: u16 = if x.is_sign_negative() { 0x8000 } else { 0 };
    let a = x.abs();
    if a.is_infinite() || a >= 65520.0 {
        return sign | 0x7c00;
    }
    if a < 2f64.powi(-14) {
        // Subnormal: units of 2^-24, rounded to even.
        let q = a / 2f64.powi(-24);
        return sign | round_even(q) as u16;
    }
    let e = a.log2().floor() as i32;
    let e = if 2f64.powi(e) > a { e - 1 } else { e };
    let m = a / 2f64.powi(e) - 1.0;
    let mut frac = round_even(m * 1024.0) as u32;
    let mut exp = (e + 15) as u32;
    if frac == 1024 {
        frac = 0;
        exp += 1;
    }
    if exp >= 31 {
        return sign | 0x7c00;
    }
    sign | ((exp as u16) << 10) | frac as u16
}

fn round_even(x: f64) -> f64 {
    let r = x.round();
    if (x - x.trunc()).abs() == 0.5 && r % 2.0 != 0.0 { r - 1.0 } else { r }
}

/// Reads element `i` of `v`.
pub fn read(v: &View, i: usize) -> Val {
    let size = v.ty.size();
    let b = v.buf.borrow();
    let at = v.offset + i * size;
    let Some(bytes) = b.data.get(at..at + size) else { return Val::Undef };
    let mut raw = [0u8; 8];
    raw[..size].copy_from_slice(bytes);
    let u = u64::from_le_bytes(raw);
    match v.ty {
        Ty::I8 => Val::Num(f64::from(u as u8 as i8)),
        Ty::U8 | Ty::U8C => Val::Num(f64::from(u as u8)),
        Ty::I16 => Val::Num(f64::from(u as u16 as i16)),
        Ty::U16 => Val::Num(f64::from(u as u16)),
        Ty::I32 => Val::Num(f64::from(u as u32 as i32)),
        Ty::U32 => Val::Num(f64::from(u as u32)),
        Ty::F16 => Val::Num(f16_to_f64(u as u16)),
        Ty::F32 => Val::Num(f64::from(f32::from_bits(u as u32))),
        Ty::F64 => Val::Num(f64::from_bits(u)),
        Ty::I64 => Val::BigInt(Rc::new(BigInt::from_i64(u as i64))),
        Ty::U64 => Val::BigInt(Rc::new(BigInt::from_u64(u))),
    }
}

/// The bytes of `x` (already a number or a `BigInt` as the type needs) as an element.
fn encode(ty: Ty, x: &Val) -> u64 {
    let n = match x {
        Val::Num(n) => *n,
        Val::BigInt(b) => return b.as_i64_wrapping() as u64,
        _ => f64::NAN,
    };
    let int = || u64::from(crate::number::to_uint32(n));
    match ty {
        Ty::I8 | Ty::U8 | Ty::I16 | Ty::U16 | Ty::I32 | Ty::U32 => int(),
        Ty::U8C => {
            if n.is_nan() {
                0
            } else {
                round_even(n.clamp(0.0, 255.0)) as u64
            }
        }
        Ty::F16 => u64::from(f64_to_f16(n)),
        Ty::F32 => u64::from((n as f32).to_bits()),
        Ty::F64 => n.to_bits(),
        Ty::I64 | Ty::U64 => 0,
    }
}

/// The value an element of `ty` takes from `x`: `ToNumber`, or `ToBigInt` for the 64-bit
/// integer types.
pub fn convert(it: &mut Interp, ty: Ty, x: &Val) -> R {
    if ty.is_big() {
        return Ok(Val::BigInt(crate::bigints::to_bigint(it, x)?));
    }
    Ok(Val::Num(it.number_of(x)?))
}

/// Writes the converted `x` to element `i` if it is in range.
pub fn write(v: &View, i: usize, x: &Val) {
    let size = v.ty.size();
    let bits = encode(v.ty, x).to_le_bytes();
    let mut b = v.buf.borrow_mut();
    let at = v.offset + i * size;
    if let Some(dst) = b.data.get_mut(at..at + size) {
        dst.copy_from_slice(&bits[..size]);
    }
}

/// `CanonicalNumericIndexString`: `Some(Some(i))` for an index, `Some(None)` for another
/// number (`-0`, `1.5`, `-1`), `None` for other keys.
pub fn numeric_key(key: &str) -> Option<Option<usize>> {
    if let Some(i) = array_index(key) {
        return Some(Some(i));
    }
    if key == "-0" {
        return Some(None);
    }
    let n = crate::number::parse(key);
    (crate::number::to_string(n) == key).then_some(None)
}

pub fn view_of(v: &Val) -> Option<View> {
    match v {
        Val::Obj(o) => match &o.borrow().kind {
            Kind::Typed(t) => Some(t.clone()),
            _ => None,
        },
        _ => None,
    }
}

fn this_view(it: &mut Interp, this: &Val) -> R<View> {
    match view_of(this) {
        Some(v) => Ok(v),
        None => it.type_error(&format!(
            "{} method called on incompatible {}",
            it.native,
            crate::builtins::class_name(this)
        )),
    }
}

/// ValidateTypedArray for the methods typed arrays share with arrays: an error for a typed
/// array outside its buffer, nothing for anything else.
pub fn validate(it: &mut Interp, this: &Val) -> R<()> {
    if view_of(this).is_some() {
        live_view(it, this)?;
    }
    Ok(())
}

/// `this` for a method of a typed array (ValidateTypedArray): a view inside its buffer, with the
/// messages of a `refrouter` 9.0.4 (measured on 02-11).
fn live_view(it: &mut Interp, this: &Val) -> R<View> {
    let v = this_view(it, this)?;
    if v.buf.borrow().detached {
        return it.type_error("attempting to access detached ArrayBuffer");
    }
    if v.out_of_bounds() {
        return it.type_error("attempting to access outside the bounds of resized ArrayBuffer");
    }
    Ok(v)
}

fn buf_of(v: &Val) -> Option<BufRef> {
    match v {
        Val::Obj(o) => match &o.borrow().kind {
            Kind::ArrayBuffer(b) => Some(b.clone()),
            _ => None,
        },
        _ => None,
    }
}

fn new_buf(it: &mut Interp, data: Vec<u8>, max: Option<usize>) -> R<(ObjRef, BufRef)> {
    it.charge(max.unwrap_or(0).saturating_sub(data.len()))?;
    let b = Rc::new(RefCell::new(Buf { data, max, detached: false }));
    let o = it.alloc(Some(it.realm.proto("ArrayBuffer")), Kind::ArrayBuffer(b.clone()))?;
    Ok((o, b))
}

/// A new typed array of `ty` with `len` zero elements.
fn new_typed(it: &mut Interp, ty: Ty, len: usize) -> R<ObjRef> {
    let data = zeroed(it, len.saturating_mul(ty.size()))?;
    let (_, buf) = new_buf(it, data, None)?;
    it.alloc(
        Some(it.realm.proto(ty.name())),
        Kind::Typed(View { ty, buf, offset: 0, len: Some(len) }),
    )
}

fn typed_from(it: &mut Interp, ty: Ty, items: &[Val]) -> R<ObjRef> {
    let o = new_typed(it, ty, items.len())?;
    let v = view_of(&Val::Obj(o.clone())).ok_or_else(|| it.error("TypeError", "typed array"))?;
    for (i, x) in items.iter().enumerate() {
        let x = convert(it, ty, x)?;
        write(&v, i, &x);
    }
    Ok(o)
}

/// `n` zero bytes, charged to the heap before they are allocated.
fn zeroed(it: &mut Interp, n: usize) -> R<Vec<u8>> {
    it.charge(n)?;
    Ok(vec![0; n])
}

pub fn elements(v: &View) -> Vec<Val> {
    (0..v.length()).map(|i| read(v, i)).collect()
}

/// The longest typed array the engine makes (2³⁰ elements).
const MAX_LENGTH: f64 = (1u64 << 30) as f64;

fn to_index(it: &mut Interp, v: &Val, what: &str) -> R<usize> {
    if matches!(v, Val::Undef) {
        return Ok(0);
    }
    let n = it.number_of(v)?;
    let n = if n.is_nan() { 0.0 } else { n.trunc() };
    if !(0.0..=9_007_199_254_740_991.0).contains(&n) {
        return Err(it.error("RangeError", what));
    }
    Ok(n as usize)
}

fn typed_ctor(it: &mut Interp, this: &Val, a: &[Val], ty: Ty) -> R {
    if new_target(this).is_none() {
        return it.type_error("calling a builtin typed array constructor without new is forbidden");
    }
    let first = arg(a, 0);
    let o = match &first {
        Val::Obj(src) => {
            if let Some(buf) = buf_of(&first) {
                let name = ty.name();
                let offset = to_index(it, &arg(a, 1), "invalid or out-of-range index")?;
                if offset % ty.size() != 0 {
                    let m = format!("start offset of {name} should be a multiple of {}", ty.size());
                    return Err(it.error("RangeError", &m));
                }
                let total = buf.borrow().data.len();
                let resizable = buf.borrow().max.is_some();
                let len = match arg(a, 2) {
                    Val::Undef if resizable => None,
                    Val::Undef => {
                        if total % ty.size() != 0 {
                            let m = format!(
                                "buffer length for {name} should be a multiple of {}",
                                ty.size()
                            );
                            return Err(it.error("RangeError", &m));
                        }
                        if offset > total {
                            let m =
                                format!("size of buffer is too small for {name} with byteOffset");
                            return Err(it.error("RangeError", &m));
                        }
                        Some((total - offset) / ty.size())
                    }
                    l => {
                        let n = to_index(it, &l, "invalid or out-of-range index")?;
                        if offset + n * ty.size() > total {
                            let m = format!(
                                "attempting to construct out-of-bounds {name} on ArrayBuffer"
                            );
                            return Err(it.error("RangeError", &m));
                        }
                        Some(n)
                    }
                };
                let o = it.alloc(
                    Some(it.realm.proto(name)),
                    Kind::Typed(View { ty, buf, offset, len }),
                )?;
                o.borrow_mut().props.push(("@@buffer".into(), Prop::hidden(first.clone())));
                o
            } else if let Some(v) = view_of(&first) {
                let items = elements(&v);
                if ty.is_big() != v.ty.is_big() {
                    let m = format!("{} elements are incompatible with {}", v.ty.name(), ty.name());
                    return it.type_error(&m);
                }
                typed_from(it, ty, &items)?
            } else {
                let _ = src;
                let items = array_like(it, &first)?;
                typed_from(it, ty, &items)?
            }
        }
        _ => {
            let n = if matches!(first, Val::Undef) { 0.0 } else { it.number_of(&first)? };
            let n = if n.is_nan() { 0.0 } else { n.trunc() };
            if !(0.0..=(1u64 << 30) as f64).contains(&n) {
                return Err(it.error("RangeError", "invalid array length"));
            }
            new_typed(it, ty, n as usize)?
        }
    };
    Ok(Val::Obj(o))
}

/// The values of an iterable, or of an array-like object without an iterator.
fn array_like(it: &mut Interp, v: &Val) -> R<Vec<Val>> {
    let f = it.get(v, "@@Symbol.iterator:0")?;
    if f.is_callable() {
        return it.iterate(v);
    }
    let n = it.get(v, "length")?;
    let n = it.number_of(&n)?;
    if n > MAX_LENGTH {
        return Err(it.error("RangeError", "invalid array length"));
    }
    let n = if n > 0.0 { n as usize } else { 0 };
    let mut out = Vec::with_capacity(n.min(1 << 20));
    for i in 0..n {
        out.push(it.get(v, &i.to_string())?);
    }
    Ok(out)
}

macro_rules! ctors {
    ($($f:ident => $t:expr),*) => {
        $(fn $f(it: &mut Interp, this: &Val, a: &[Val]) -> R { typed_ctor(it, this, a, $t) })*
    };
}
ctors!(c_i8 => Ty::I8, c_u8 => Ty::U8, c_u8c => Ty::U8C, c_i16 => Ty::I16, c_u16 => Ty::U16,
    c_i32 => Ty::I32, c_u32 => Ty::U32, c_f16 => Ty::F16, c_f32 => Ty::F32, c_f64 => Ty::F64,
    c_i64 => Ty::I64, c_u64 => Ty::U64);

fn ty_of_ctor(it: &mut Interp, c: &Val) -> R<Ty> {
    let name = it.get(c, "name")?;
    let name = it.str_of(&name)?;
    match TYPES.iter().find(|(_, n)| **n == *name) {
        Some((t, _)) => Ok(*t),
        None => it.type_error("not a typed array constructor"),
    }
}

fn ta_from(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let ty = ty_of_ctor(it, this)?;
    let f = arg(a, 1);
    if !matches!(f, Val::Undef) && !f.is_callable() {
        let text = crate::decompile::value_source(it, &f);
        return it.type_error(&format!("{text} is not a function"));
    }
    let mut items = array_like(it, &arg(a, 0))?;
    if f.is_callable() {
        for (i, x) in items.iter_mut().enumerate() {
            *x = it.call(&f, &arg(a, 2), &[x.clone(), Val::Num(i as f64)])?;
        }
    }
    Ok(Val::Obj(typed_from(it, ty, &items)?))
}

fn ta_of(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let ty = ty_of_ctor(it, this)?;
    Ok(Val::Obj(typed_from(it, ty, a)?))
}

fn abstract_ctor(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    let how = if new_target(this).is_some() { "construct" } else { "call" };
    it.type_error(&format!("cannot directly {how} builtin %TypedArray%"))
}

// Getters.

fn g_length(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    Ok(Val::Num(this_view(it, this)?.length() as f64))
}

fn g_byte_length(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    let v = this_view(it, this)?;
    Ok(Val::Num((v.length() * v.ty.size()) as f64))
}

fn g_byte_offset(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    let v = this_view(it, this)?;
    Ok(Val::Num(if v.out_of_bounds() { 0.0 } else { v.offset as f64 }))
}

fn g_buffer(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    let v = this_view(it, this)?;
    let o = it.alloc(Some(it.realm.proto("ArrayBuffer")), Kind::ArrayBuffer(v.buf.clone()))?;
    if let Val::Obj(t) = this {
        // The same buffer object each time.
        if let Some(p) = t.borrow().get_own("@@buffer") {
            return Ok(p.value.clone());
        }
        t.borrow_mut().props.push(("@@buffer".into(), Prop::hidden(Val::Obj(o.clone()))));
    }
    Ok(Val::Obj(o))
}

fn g_tag(_: &mut Interp, this: &Val, _: &[Val]) -> R {
    Ok(match view_of(this) {
        Some(v) => Val::str(v.ty.name()),
        None => Val::Undef,
    })
}

// Methods that make or change typed arrays.

fn same_type(it: &mut Interp, v: &View, items: &[Val]) -> R {
    Ok(Val::Obj(typed_from(it, v.ty, items)?))
}

fn ta_map(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let v = live_view(it, this)?;
    let f = callable(it, &arg(a, 0))?;
    let mut out = Vec::new();
    for (i, x) in elements(&v).into_iter().enumerate() {
        out.push(it.call(&f, &arg(a, 1), &[x, Val::Num(i as f64), this.clone()])?);
    }
    same_type(it, &v, &out)
}

fn ta_filter(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let v = live_view(it, this)?;
    let f = callable(it, &arg(a, 0))?;
    let mut out = Vec::new();
    for (i, x) in elements(&v).into_iter().enumerate() {
        if it.call(&f, &arg(a, 1), &[x.clone(), Val::Num(i as f64), this.clone()])?.truthy() {
            out.push(x);
        }
    }
    same_type(it, &v, &out)
}

/// The methods of `%TypedArray%.prototype` that take a callback and do what the array ones do.
const CALLBACK_METHODS: [&str; 9] = [
    "every",
    "find",
    "findIndex",
    "findLast",
    "findLastIndex",
    "forEach",
    "reduce",
    "reduceRight",
    "some",
];

/// `CALLBACK_METHODS[I]` of a typed array: its callback checked as the `refrouter` 9.0.4 does, then
/// the array method.
fn with_callback<const I: usize>(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    live_view(it, this)?;
    let name = CALLBACK_METHODS[I];
    if matches!(arg(a, 0), Val::Undef) {
        let owner = if name == "forEach" { "TypedArray" } else { "%TypedArray%" };
        let msg = format!("missing argument 0 when calling function {owner}.prototype.{name}");
        return it.type_error(&msg);
    }
    let f = it.get(&Val::Obj(it.realm.array.clone()), name)?;
    it.call(&f, this, a)
}

fn callable(it: &mut Interp, f: &Val) -> R {
    if matches!(f, Val::Undef) {
        // Measured on a `refrouter` 9.0.4 on 02-11 (`forEach` without the `%`).
        let m = it.native.clone();
        let owner = if &*m == "forEach" { "TypedArray" } else { "%TypedArray%" };
        let msg = format!("missing argument 0 when calling function {owner}.prototype.{m}");
        return it.type_error(&msg);
    }
    if !f.is_callable() {
        return it.type_error(&format!("{} is not a function", it.show(f)));
    }
    Ok(f.clone())
}

fn relative(it: &mut Interp, v: &Val, len: usize, default: usize) -> R<usize> {
    if matches!(v, Val::Undef) {
        return Ok(default);
    }
    let n = it.number_of(v)?;
    let n = if n.is_nan() { 0.0 } else { n.trunc() };
    Ok(if n < 0.0 { (len as f64 + n).max(0.0) as usize } else { (n.min(len as f64)) as usize })
}

fn ta_slice(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let v = live_view(it, this)?;
    let all = elements(&v);
    let s = relative(it, &arg(a, 0), all.len(), 0)?;
    let e = relative(it, &arg(a, 1), all.len(), all.len())?;
    same_type(it, &v, all.get(s..e.max(s)).unwrap_or(&[]))
}

fn ta_subarray(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let v = live_view(it, this)?;
    let n = v.length();
    let s = relative(it, &arg(a, 0), n, 0)?;
    let e = relative(it, &arg(a, 1), n, n)?;
    let view = View {
        ty: v.ty,
        buf: v.buf.clone(),
        offset: v.offset + s * v.ty.size(),
        len: Some(e.saturating_sub(s)),
    };
    // The new view's `buffer` is the same object.
    let buffer = g_buffer(it, this, &[])?;
    let o = it.alloc(Some(it.realm.proto(v.ty.name())), Kind::Typed(view))?;
    o.borrow_mut().props.push(("@@buffer".into(), Prop::hidden(buffer)));
    Ok(Val::Obj(o))
}

/// Numbers in ascending order, NaN last (the default order of a typed array).
fn numeric_sort(items: &mut [Val]) {
    items.sort_by(|x, y| match (x, y) {
        (Val::Num(p), Val::Num(q)) => match (p.is_nan(), q.is_nan()) {
            (true, true) => std::cmp::Ordering::Equal,
            (true, false) => std::cmp::Ordering::Greater,
            (false, true) => std::cmp::Ordering::Less,
            _ => p
                .partial_cmp(q)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| p.is_sign_positive().cmp(&q.is_sign_positive())),
        },
        (Val::BigInt(p), Val::BigInt(q)) => p.cmp(q),
        _ => std::cmp::Ordering::Equal,
    });
}

fn sorted(it: &mut Interp, v: &View, cmp: &Val) -> R<Vec<Val>> {
    let mut items = elements(v);
    if matches!(cmp, Val::Undef) {
        numeric_sort(&mut items);
        return Ok(items);
    }
    let cmp = callable(it, cmp)?;
    // Insertion sort: the comparator may throw, and runs on few elements in practice.
    for i in 1..items.len() {
        let mut j = i;
        while j > 0 {
            let r = it.call(&cmp, &Val::Undef, &[items[j - 1].clone(), items[j].clone()])?;
            if it.number_of(&r)? > 0.0 {
                items.swap(j - 1, j);
                j -= 1;
            } else {
                break;
            }
        }
    }
    Ok(items)
}

fn store(v: &View, items: &[Val]) {
    for (i, x) in items.iter().enumerate() {
        write(v, i, x);
    }
}

fn ta_sort(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let v = live_view(it, this)?;
    if !matches!(arg(a, 0), Val::Undef) && !arg(a, 0).is_callable() {
        return it.type_error("invalid %TypedArray%.prototype.sort argument");
    }
    let items = sorted(it, &v, &arg(a, 0))?;
    store(&v, &items);
    Ok(this.clone())
}

fn ta_to_sorted(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let v = live_view(it, this)?;
    let items = sorted(it, &v, &arg(a, 0))?;
    same_type(it, &v, &items)
}

fn ta_reverse(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    let v = live_view(it, this)?;
    let mut items = elements(&v);
    items.reverse();
    store(&v, &items);
    Ok(this.clone())
}

fn ta_to_reversed(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    let v = live_view(it, this)?;
    let mut items = elements(&v);
    items.reverse();
    same_type(it, &v, &items)
}

fn ta_with(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let v = live_view(it, this)?;
    let mut items = elements(&v);
    let n = it.number_of(&arg(a, 0))?;
    let n = if n.is_nan() { 0.0 } else { n.trunc() };
    let i = if n < 0.0 { items.len() as f64 + n } else { n };
    let x = convert(it, v.ty, &arg(a, 1))?;
    if i < 0.0 || i >= items.len() as f64 {
        return Err(it.error("RangeError", "invalid or out-of-range index"));
    }
    items[i as usize] = x;
    same_type(it, &v, &items)
}

fn ta_fill(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let v = live_view(it, this)?;
    let x = convert(it, v.ty, &arg(a, 0))?;
    let n = v.length();
    let s = relative(it, &arg(a, 1), n, 0)?;
    let e = relative(it, &arg(a, 2), n, n)?;
    for i in s..e {
        write(&v, i, &x);
    }
    Ok(this.clone())
}

fn ta_set(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let v = live_view(it, this)?;
    let offset = to_index(it, &arg(a, 1), "invalid or out-of-range index")?;
    let items = match view_of(&arg(a, 0)) {
        Some(src) => elements(&src),
        None => {
            let src = arg(a, 0);
            let o = Val::Obj(it.object_of(&src)?);
            let n = it.get(&o, "length")?;
            let n = it.number_of(&n)?;
            let n = if n.is_finite() && n > 0.0 { n as usize } else { 0 };
            if offset + n > v.length() {
                return Err(it.error("RangeError", "source array is too long"));
            }
            let mut out = Vec::with_capacity(n);
            for i in 0..n {
                out.push(it.get(&o, &i.to_string())?);
            }
            out
        }
    };
    if offset + items.len() > v.length() {
        return Err(it.error("RangeError", "source array is too long"));
    }
    for (i, x) in items.iter().enumerate() {
        let x = convert(it, v.ty, x)?;
        write(&v, offset + i, &x);
    }
    Ok(Val::Undef)
}

fn ta_copy_within(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let v = live_view(it, this)?;
    let items = elements(&v);
    let n = items.len();
    let target = relative(it, &arg(a, 0), n, 0)?;
    let s = relative(it, &arg(a, 1), n, 0)?;
    let e = relative(it, &arg(a, 2), n, n)?;
    for (k, x) in items.iter().take(e).skip(s).enumerate() {
        if target + k < n {
            write(&v, target + k, x);
        }
    }
    Ok(this.clone())
}

const HEX: &[u8; 16] = b"0123456789abcdef";
const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn bytes_of(it: &mut Interp, this: &Val) -> R<Vec<u8>> {
    let v = live_view(it, this)?;
    if v.ty != Ty::U8 {
        let m = format!("{} method called on incompatible {}", it.native, v.ty.name());
        return it.type_error(&m);
    }
    let n = v.length();
    Ok(v.buf.borrow().data[v.offset..v.offset + n].to_vec())
}

fn to_hex(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    let b = bytes_of(it, this)?;
    it.string(
        b.iter()
            .flat_map(|x| [HEX[usize::from(x >> 4)] as char, HEX[usize::from(x & 15)] as char])
            .collect(),
    )
}

fn to_base64(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    let b = bytes_of(it, this)?;
    let mut out = String::new();
    for chunk in b.chunks(3) {
        let n =
            chunk.iter().fold(0u32, |acc, x| (acc << 8) | u32::from(*x)) << (8 * (3 - chunk.len()));
        for k in 0..4 {
            if k <= chunk.len() {
                out.push(B64[((n >> (18 - 6 * k)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    it.string(out)
}

fn from_hex(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    let s = string_arg(it, a)?;
    if s.chars().count() % 2 != 0 {
        return Err(it.error("SyntaxError", "hex-string must have an even number of characters"));
    }
    let mut d = Vec::with_capacity(s.len());
    for c in s.chars() {
        match c.to_digit(16) {
            Some(x) => d.push(x as u8),
            None => {
                return Err(it.error("SyntaxError", &format!("'{c}' is not a valid hex-digit")));
            }
        }
    }
    let bytes: Vec<Val> = d.chunks(2).map(|p| Val::Num(f64::from(p[0] * 16 + p[1]))).collect();
    Ok(Val::Obj(typed_from(it, Ty::U8, &bytes)?))
}

fn from_base64(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    let s = string_arg(it, a)?;
    let chunks = base64_chunks(&s).map_err(|m| it.error("SyntaxError", &m))?;
    let out: Vec<Val> =
        chunks.iter().flat_map(|(b, _)| b.iter().map(|x| Val::Num(f64::from(*x)))).collect();
    Ok(Val::Obj(typed_from(it, Ty::U8, &out)?))
}

/// The string argument of `fromHex` and `fromBase64` (measured on a `refrouter` 9.0.4 on 02-11).
fn string_arg(it: &mut Interp, a: &[Val]) -> R<Name> {
    match arg(a, 0) {
        Val::Str(s) => Ok(s),
        v => {
            let text = crate::decompile::value_source(it, &v);
            it.type_error(&format!("{text} is not a string"))
        }
    }
}

/// The chunks of a base64 text (ECMA-262 FromBase64, with ASCII spaces anywhere and the last
/// chunk loose): each one's bytes and the characters read up to its end, or the message of a
/// `refrouter` 9.0.4 for what is wrong.
fn base64_chunks(s: &str) -> Result<Vec<(Vec<u8>, usize)>, String> {
    let chars: Vec<char> = s.chars().collect();
    let mut out = Vec::new();
    let mut chunk: Vec<u32> = Vec::new();
    let mut i = 0;
    let decode = |chunk: &[u32]| -> Vec<u8> {
        let n = chunk.iter().fold(0u32, |acc, d| (acc << 6) | d) << (6 * (4 - chunk.len()));
        let bytes = [(n >> 16) as u8, (n >> 8) as u8, n as u8];
        bytes[..chunk.len() - 1].to_vec()
    };
    while i < chars.len() {
        let c = chars[i];
        i += 1;
        if c.is_ascii_whitespace() {
            continue;
        }
        if c == '=' {
            if chunk.len() < 2 {
                return Err("unexpected incomplete base64 chunk".into());
            }
            if chunk.len() == 2 {
                while i < chars.len() && chars[i].is_ascii_whitespace() {
                    i += 1;
                }
                if chars.get(i) != Some(&'=') {
                    return Err("missing padding '=' character".into());
                }
                i += 1;
            }
            if let Some(c) = chars[i..].iter().find(|c| !c.is_ascii_whitespace()) {
                return Err(format!("unexpected '{c}' after base64 padding"));
            }
            out.push((decode(&chunk), i));
            return Ok(out);
        }
        let Some(d) = B64.iter().position(|b| *b as char == c) else {
            return Err(format!("'{c}' is not a valid base64 character"));
        };
        chunk.push(d as u32);
        if chunk.len() == 4 {
            out.push((decode(&chunk), i));
            chunk.clear();
        }
    }
    match chunk.len() {
        0 => {}
        1 => return Err("unexpected incomplete base64 chunk".into()),
        _ => out.push((decode(&chunk), i)),
    }
    Ok(out)
}

// ArrayBuffer.

fn this_buf(it: &mut Interp, this: &Val) -> R<BufRef> {
    match buf_of(this) {
        Some(b) => Ok(b),
        None => it.type_error(&format!(
            "{} method called on incompatible {}",
            it.native,
            crate::builtins::class_name(this)
        )),
    }
}

fn ab_ctor(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    if new_target(this).is_none() {
        return it.type_error("calling a builtin ArrayBuffer constructor without new is forbidden");
    }
    let n = to_index(it, &arg(a, 0), "invalid array buffer length")?;
    let max = match arg(a, 1) {
        Val::Obj(_) => match it.get(&arg(a, 1), "maxByteLength")? {
            Val::Undef => None,
            m => Some(to_index(it, &m, "invalid array buffer max length")?),
        },
        _ => None,
    };
    // Measured on a `refrouter` 9.0.4 on 02-11.
    if n > (1 << 30) || max.is_some_and(|m| m > (1 << 30)) {
        return Err(it.error("RangeError", "invalid array length"));
    }
    if max.is_some_and(|m| m < n) {
        return Err(it.error("RangeError", OVER_MAX));
    }
    let data = zeroed(it, n)?;
    Ok(Val::Obj(new_buf(it, data, max)?.0))
}

fn ab_is_view(_: &mut Interp, _: &Val, a: &[Val]) -> R {
    Ok(Val::Bool(
        matches!(arg(a, 0), Val::Obj(o) if matches!(o.borrow().kind, Kind::Typed(_) | Kind::DataView(_))),
    ))
}

fn ab_byte_length(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    Ok(Val::Num(this_buf(it, this)?.borrow().data.len() as f64))
}

fn ab_max(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    let b = this_buf(it, this)?;
    let b = b.borrow();
    Ok(Val::Num(b.max.unwrap_or(b.data.len()) as f64))
}

fn ab_resizable(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    Ok(Val::Bool(this_buf(it, this)?.borrow().max.is_some()))
}

fn ab_detached(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    Ok(Val::Bool(this_buf(it, this)?.borrow().detached))
}

fn ab_slice(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let b = this_buf(it, this)?;
    if b.borrow().detached {
        return it.type_error(DETACHED);
    }
    let data = b.borrow().data.clone();
    let s = relative(it, &arg(a, 0), data.len(), 0)?;
    let e = relative(it, &arg(a, 1), data.len(), data.len())?;
    let part = data.get(s..e.max(s)).unwrap_or(&[]);
    it.charge(part.len())?;
    Ok(Val::Obj(new_buf(it, part.to_vec(), None)?.0))
}

fn ab_resize(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let b = this_buf(it, this)?;
    let n = to_index(it, &arg(a, 0), "invalid array buffer length")?;
    let max = b.borrow().max;
    if b.borrow().detached {
        return it.type_error("resize method called on incompatible ArrayBuffer");
    }
    match max {
        None => it.type_error("ArrayBuffer.prototype.resize called on a non-resizable buffer"),
        Some(m) if n > m => Err(it.error("RangeError", OVER_MAX)),
        Some(_) => {
            b.borrow_mut().data.resize(n, 0);
            Ok(Val::Undef)
        }
    }
}

/// The error of a detached buffer (measured on a `refrouter` 9.0.4 on 02-11).
const DETACHED: &str = "attempting to access detached ArrayBuffer";
/// A length over the buffer's maximum.
const OVER_MAX: &str = "byte length is larger than maximum byte length";

fn transfer_in(it: &mut Interp, this: &Val, a: &[Val], keep_max: bool) -> R {
    let b = this_buf(it, this)?;
    if b.borrow().detached {
        return it.type_error(DETACHED);
    }
    let mut data = std::mem::take(&mut b.borrow_mut().data);
    if !matches!(arg(a, 0), Val::Undef) {
        let n = to_index(it, &arg(a, 0), "invalid array buffer length")?;
        if keep_max && b.borrow().max.is_some_and(|m| n > m) {
            b.borrow_mut().data = data;
            return Err(it.error("RangeError", OVER_MAX));
        }
        it.charge(n.saturating_sub(data.len()))?;
        data.resize(n, 0);
    }
    let max = if keep_max { b.borrow().max } else { None };
    b.borrow_mut().detached = true;
    Ok(Val::Obj(new_buf(it, data, max)?.0))
}

fn ab_transfer(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    transfer_in(it, this, a, true)
}

fn ab_transfer_fixed(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    transfer_in(it, this, a, false)
}

// DataView.

fn dv_ctor(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    if new_target(this).is_none() {
        return it.type_error("calling a builtin DataView constructor without new is forbidden");
    }
    let Some(buf) = buf_of(&arg(a, 0)) else {
        let shown = it.show(&arg(a, 0));
        return it.type_error(&format!("{shown} is not an object"));
    };
    let total = buf.borrow().data.len();
    let offset = to_index(it, &arg(a, 1), "invalid or out-of-range index")?;
    if offset > total {
        return Err(it.error("RangeError", "start offset is outside the bounds of the buffer"));
    }
    let len = match arg(a, 2) {
        Val::Undef => total - offset,
        l => to_index(it, &l, "invalid or out-of-range index")?,
    };
    if offset + len > total {
        return Err(it.error("RangeError", "invalid data view length"));
    }
    let view = View { ty: Ty::U8, buf, offset, len: Some(len) };
    let o = it.alloc(Some(it.realm.proto("DataView")), Kind::DataView(view))?;
    // Its `buffer` is the buffer it was made on.
    o.borrow_mut().props.push(("@@buffer".into(), Prop::hidden(arg(a, 0))));
    Ok(Val::Obj(o))
}

fn this_dv(it: &mut Interp, this: &Val) -> R<View> {
    if let Val::Obj(o) = this
        && let Kind::DataView(v) = &o.borrow().kind
    {
        return Ok(v.clone());
    }
    it.type_error(&format!(
        "{} method called on incompatible {}",
        it.native,
        crate::builtins::class_name(this)
    ))
}

fn dv_byte_length(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    Ok(Val::Num(this_dv(it, this)?.length() as f64))
}

fn dv_byte_offset(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    Ok(Val::Num(this_dv(it, this)?.offset as f64))
}

fn dv_buffer(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    this_dv(it, this)?;
    let Val::Obj(o) = this else { return Ok(Val::Undef) };
    Ok(o.borrow().get_own("@@buffer").map_or(Val::Undef, |p| p.value.clone()))
}

/// The element view of `ty` at byte `index` of a `DataView`, checked against its bounds.
fn dv_at(it: &mut Interp, this: &Val, index: &Val, ty: Ty) -> R<View> {
    let v = this_dv(it, this)?;
    let i = to_index(it, index, "invalid or out-of-range index")?;
    if v.buf.borrow().detached {
        return it.type_error(DETACHED);
    }
    if i + ty.size() > v.length() {
        return Err(it.error("RangeError", "offset is outside the bounds of the DataView"));
    }
    Ok(View { ty, buf: v.buf, offset: v.offset + i, len: Some(1) })
}

fn swap_if(v: &View, little: bool) {
    if little || v.ty.size() == 1 {
        return;
    }
    let mut b = v.buf.borrow_mut();
    if let Some(s) = b.data.get_mut(v.offset..v.offset + v.ty.size()) {
        s.reverse();
    }
}

fn dv_get(it: &mut Interp, this: &Val, a: &[Val], ty: Ty) -> R {
    let v = dv_at(it, this, &arg(a, 0), ty)?;
    let little = arg(a, 1).truthy();
    swap_if(&v, little);
    let x = read(&v, 0);
    swap_if(&v, little);
    Ok(x)
}

fn dv_set(it: &mut Interp, this: &Val, a: &[Val], ty: Ty) -> R {
    let x = convert(it, ty, &arg(a, 1))?;
    let v = dv_at(it, this, &arg(a, 0), ty)?;
    write(&v, 0, &x);
    swap_if(&v, arg(a, 2).truthy());
    Ok(Val::Undef)
}

macro_rules! dv_methods {
    ($($get:ident, $set:ident => $t:expr),*) => {
        $(
            fn $get(it: &mut Interp, this: &Val, a: &[Val]) -> R { dv_get(it, this, a, $t) }
            fn $set(it: &mut Interp, this: &Val, a: &[Val]) -> R { dv_set(it, this, a, $t) }
        )*
    };
}
dv_methods!(g_i8, s_i8 => Ty::I8, g_u8, s_u8 => Ty::U8, g_i16, s_i16 => Ty::I16,
    g_u16, s_u16 => Ty::U16, g_i32, s_i32 => Ty::I32, g_u32, s_u32 => Ty::U32,
    g_f16, s_f16 => Ty::F16, g_f32, s_f32 => Ty::F32, g_f64, s_f64 => Ty::F64,
    g_i64, s_i64 => Ty::I64, g_u64, s_u64 => Ty::U64);

fn getter(it: &mut Interp, o: &ObjRef, name: &str, f: NativeFn) -> R<()> {
    let g = it.native(&format!("get {name}"), f, 0)?;
    it.define(o, name, Prop { getter: Some(g), ..Prop::hidden(Val::Undef) })
}

pub fn install(it: &mut Interp, g: &ObjRef) -> R<()> {
    // ArrayBuffer.
    let abp = it.alloc(Some(it.realm.object.clone()), Kind::Ordinary)?;
    let abc = ctor(it, g, "ArrayBuffer", ab_ctor, 1, &abp)?;
    method(it, &abc, "isView", ab_is_view, 1)?;
    for (name, f) in [
        ("byteLength", ab_byte_length as NativeFn),
        ("maxByteLength", ab_max),
        ("resizable", ab_resizable),
        ("detached", ab_detached),
    ] {
        getter(it, &abp, name, f)?;
    }
    for (name, f, n) in [
        ("slice", ab_slice as NativeFn, 2),
        ("resize", ab_resize, 1),
        ("transfer", ab_transfer, 0),
        ("transferToFixedLength", ab_transfer_fixed, 0),
    ] {
        method(it, &abp, name, f, n)?;
    }
    to_string_tag(it, &abp, "ArrayBuffer")?;
    it.realm.protos.push(("ArrayBuffer", abp));

    // %TypedArray% and its prototype: the generic methods are those of arrays.
    let tap = it.alloc(Some(it.realm.object.clone()), Kind::Ordinary)?;
    let ta = it.native("TypedArray", abstract_ctor, 0)?;
    it.define(&ta, "prototype", Prop { writable: false, ..Prop::hidden(Val::Obj(tap.clone())) })?;
    it.define(&tap, "constructor", Prop::hidden(Val::Obj(ta.clone())))?;
    method(it, &ta, "from", ta_from, 1)?;
    method(it, &ta, "of", ta_of, 0)?;
    let ap = Val::Obj(it.realm.array.clone());
    for name in [
        "at",
        "entries",
        "includes",
        "indexOf",
        "join",
        "keys",
        "lastIndexOf",
        "toLocaleString",
        "toString",
        "values",
    ] {
        let f = it.get(&ap, name)?;
        it.define(&tap, name, Prop::hidden(f))?;
    }
    let values = it.get(&ap, "values")?;
    it.define(&tap, "@@Symbol.iterator:0", Prop::hidden(values))?;
    let with_callback: [NativeFn; 9] = [
        with_callback::<0>,
        with_callback::<1>,
        with_callback::<2>,
        with_callback::<3>,
        with_callback::<4>,
        with_callback::<5>,
        with_callback::<6>,
        with_callback::<7>,
        with_callback::<8>,
    ];
    for (name, f) in CALLBACK_METHODS.iter().zip(with_callback) {
        method(it, &tap, name, f, 1)?;
    }
    for (name, f, n) in [
        ("map", ta_map as NativeFn, 1),
        ("filter", ta_filter, 1),
        ("slice", ta_slice, 2),
        ("subarray", ta_subarray, 2),
        ("sort", ta_sort, 1),
        ("toSorted", ta_to_sorted, 1),
        ("reverse", ta_reverse, 0),
        ("toReversed", ta_to_reversed, 0),
        ("with", ta_with, 2),
        ("fill", ta_fill, 1),
        ("set", ta_set, 1),
        ("copyWithin", ta_copy_within, 2),
    ] {
        method(it, &tap, name, f, n)?;
    }
    for (name, f) in [
        ("length", g_length as NativeFn),
        ("byteLength", g_byte_length),
        ("byteOffset", g_byte_offset),
        ("buffer", g_buffer),
    ] {
        getter(it, &tap, name, f)?;
    }
    let tag = it.native("get [Symbol.toStringTag]", g_tag, 0)?;
    it.define(
        &tap,
        &crate::builtins::well_known("toStringTag"),
        Prop { getter: Some(tag), ..Prop::hidden(Val::Undef) },
    )?;
    let ctors: [NativeFn; 12] =
        [c_i8, c_u8, c_u8c, c_i16, c_u16, c_i32, c_u32, c_f16, c_f32, c_f64, c_i64, c_u64];
    for ((ty, name), f) in TYPES.into_iter().zip(ctors) {
        let p = it.alloc(Some(tap.clone()), Kind::Ordinary)?;
        let c = ctor(it, g, name, f, 3, &p)?;
        c.borrow_mut().proto = Some(ta.clone());
        let bpe = Prop { writable: false, ..Prop::hidden(Val::Num(ty.size() as f64)) };
        it.define(&c, "BYTES_PER_ELEMENT", bpe.clone())?;
        it.define(&p, "BYTES_PER_ELEMENT", bpe)?;
        if ty == Ty::U8 {
            method(it, &c, "fromHex", from_hex, 1)?;
            method(it, &c, "fromBase64", from_base64, 1)?;
            method(it, &p, "toHex", to_hex, 0)?;
            method(it, &p, "toBase64", to_base64, 0)?;
            method(it, &p, "setFromHex", set_from_hex, 1)?;
            method(it, &p, "setFromBase64", set_from_base64, 1)?;
        }
        it.realm.protos.push((name, p));
    }

    // DataView.
    let dvp = it.alloc(Some(it.realm.object.clone()), Kind::Ordinary)?;
    ctor(it, g, "DataView", dv_ctor, 1, &dvp)?;
    for (name, f) in [
        ("byteLength", dv_byte_length as NativeFn),
        ("byteOffset", dv_byte_offset),
        ("buffer", dv_buffer),
    ] {
        getter(it, &dvp, name, f)?;
    }
    for (name, f, n) in [
        ("getInt8", g_i8 as NativeFn, 1),
        ("setInt8", s_i8, 2),
        ("getUint8", g_u8, 1),
        ("setUint8", s_u8, 2),
        ("getInt16", g_i16, 1),
        ("setInt16", s_i16, 2),
        ("getUint16", g_u16, 1),
        ("setUint16", s_u16, 2),
        ("getInt32", g_i32, 1),
        ("setInt32", s_i32, 2),
        ("getUint32", g_u32, 1),
        ("setUint32", s_u32, 2),
        ("getFloat16", g_f16, 1),
        ("setFloat16", s_f16, 2),
        ("getFloat32", g_f32, 1),
        ("setFloat32", s_f32, 2),
        ("getFloat64", g_f64, 1),
        ("setFloat64", s_f64, 2),
        ("getBigInt64", g_i64, 1),
        ("setBigInt64", s_i64, 2),
        ("getBigUint64", g_u64, 1),
        ("setBigUint64", s_u64, 2),
    ] {
        method(it, &dvp, name, f, n)?;
    }
    to_string_tag(it, &dvp, "DataView")?;
    it.realm.protos.push(("DataView", dvp));
    Ok(())
}

/// `setFromHex` and `setFromBase64`: the bytes decoded into the array, `{read, written}`.
fn set_from(it: &mut Interp, this: &Val, decoded: Val, read: usize) -> R {
    let v = live_view(it, this)?;
    let src = view_of(&decoded).map(|d| elements(&d)).unwrap_or_default();
    let n = src.len().min(v.length());
    for (i, x) in src.iter().take(n).enumerate() {
        write(&v, i, x);
    }
    let o = it.object()?;
    it.define(&o, "read", Prop::data(Val::Num(read as f64)))?;
    it.define(&o, "written", Prop::data(Val::Num(n as f64)))?;
    Ok(Val::Obj(o))
}

fn set_from_hex(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let d = from_hex(it, &Val::Undef, a)?;
    let room = live_view(it, this)?.length();
    let n = view_of(&d).map_or(0, |v| v.length()).min(room);
    // Two characters each byte written.
    set_from(it, this, d, 2 * n)
}

fn set_from_base64(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let room = live_view(it, this)?.length();
    let s = string_arg(it, a)?;
    let chunks = base64_chunks(&s).map_err(|m| it.error("SyntaxError", &m))?;
    // Whole chunks while they fit.
    let (mut bytes, mut read) = (Vec::new(), 0);
    for (b, end) in chunks {
        if bytes.len() + b.len() > room {
            break;
        }
        bytes.extend(b.into_iter().map(|x| Val::Num(f64::from(x))));
        read = end;
    }
    let d = Val::Obj(typed_from(it, Ty::U8, &bytes)?);
    set_from(it, this, d, read)
}
