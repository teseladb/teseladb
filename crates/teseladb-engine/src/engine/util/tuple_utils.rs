//! Generic operations over tuples.
//!
//! Traits implemented for tuples of up to 12 elements: [`TupleMap`] maps each element,
//! [`TupleForEach`] visits each element, [`TupleMapTypes`] maps the element types, and
//! [`TupleFilter`] keeps the elements whose type passes a type-level filter.
//!
//! A Rust closure cannot be generic over its argument type, so the per-element function
//! is a type implementing [`TupleFunction<T>`] or [`TupleMapFunction<T>`] for every element
//! type it accepts (usually with one blanket impl). Likewise a filter is a marker type
//! implementing [`FilterClass<T>`] for each element type, with `Value` set to
//! [`TrueType`] or [`FalseType`]. Everything is resolved at compile time; the generated
//! code is the same as writing the element accesses by hand.

/// Internal helpers; not part of the stable API.
pub mod internal {
    /// Unwraps a one-element tuple into its element.
    pub trait Untuple {
        /// The result type.
        type Output;
        /// Unwraps a one-element tuple.
        fn untuple(self) -> Self::Output;
    }

    impl<T> Untuple for (T,) {
        type Output = T;
        #[inline]
        fn untuple(self) -> T {
            self.0
        }
    }

    /// Calls [`Untuple::untuple`].
    #[inline]
    pub fn untuple<T: Untuple>(t: T) -> T::Output {
        t.untuple()
    }
}

/// A function applied to each element of a tuple.
///
/// It is implemented for each element type it accepts, usually with a blanket
/// `impl<T: Bound> TupleFunction<T> for MyFn`, where `Bound` is what the body needs.
pub trait TupleFunction<T> {
    /// Called with each element in order.
    fn call(&mut self, x: T);
}

/// A function mapping each element of a tuple to a value, implemented for each element
/// type.
pub trait TupleMapFunction<T> {
    /// The result for an element of type `T`.
    type Output;
    /// Maps one element.
    fn call(&mut self, x: T) -> Self::Output;
}

/// Mapping a tuple element-wise with a function accepting each of its element types.
pub trait TupleMap<F> {
    /// The tuple of `F`'s result for each element.
    type Output;

    /// Applies `f` to every element, in order, and returns the tuple of results.
    fn tuple_map(self, f: F) -> Self::Output;
}

/// Visiting every element of a tuple with a function accepting each of its element types.
pub trait TupleForEach<F> {
    /// Calls `f` with each element of the tuple, in order.
    fn tuple_for_each(self, f: F);
}

/// The type a type maps to under the marker `M`; implemented once per element type.
pub trait TypeMap<M> {
    /// The mapped type.
    type Type;
}

/// The tuple of the element types mapped with [`TypeMap<M>`].
pub trait TupleMapTypes<M> {
    /// The mapped tuple type.
    type Type;
}

macro_rules! impl_tuple_utils {
    ($(($t:ident, $i:tt)),*) => {
        impl<F $(, $t)*> TupleMap<F> for ($($t,)*)
        where
            $(F: TupleMapFunction<$t>,)*
        {
            type Output = ($(<F as TupleMapFunction<$t>>::Output,)*);
            #[inline]
            #[allow(unused_mut, unused_variables, clippy::unused_unit)]
            fn tuple_map(self, mut f: F) -> Self::Output {
                ($(<F as TupleMapFunction<$t>>::call(&mut f, self.$i),)*)
            }
        }

        impl<F $(, $t)*> TupleForEach<F> for ($($t,)*)
        where
            $(F: TupleFunction<$t>,)*
        {
            #[inline]
            #[allow(unused_mut, unused_variables)]
            fn tuple_for_each(self, mut f: F) {
                $(<F as TupleFunction<$t>>::call(&mut f, self.$i);)*
            }
        }

        impl<M $(, $t: TypeMap<M>)*> TupleMapTypes<M> for ($($t,)*) {
            type Type = ($(<$t as TypeMap<M>>::Type,)*);
        }
    };
}

impl_tuple_utils!();
impl_tuple_utils!((A0, 0));
impl_tuple_utils!((A0, 0), (A1, 1));
impl_tuple_utils!((A0, 0), (A1, 1), (A2, 2));
impl_tuple_utils!((A0, 0), (A1, 1), (A2, 2), (A3, 3));
impl_tuple_utils!((A0, 0), (A1, 1), (A2, 2), (A3, 3), (A4, 4));
impl_tuple_utils!((A0, 0), (A1, 1), (A2, 2), (A3, 3), (A4, 4), (A5, 5));
impl_tuple_utils!((A0, 0), (A1, 1), (A2, 2), (A3, 3), (A4, 4), (A5, 5), (A6, 6));
impl_tuple_utils!((A0, 0), (A1, 1), (A2, 2), (A3, 3), (A4, 4), (A5, 5), (A6, 6), (A7, 7));
impl_tuple_utils!((A0, 0), (A1, 1), (A2, 2), (A3, 3), (A4, 4), (A5, 5), (A6, 6), (A7, 7), (A8, 8));
impl_tuple_utils!(
    (A0, 0),
    (A1, 1),
    (A2, 2),
    (A3, 3),
    (A4, 4),
    (A5, 5),
    (A6, 6),
    (A7, 7),
    (A8, 8),
    (A9, 9)
);
impl_tuple_utils!(
    (A0, 0),
    (A1, 1),
    (A2, 2),
    (A3, 3),
    (A4, 4),
    (A5, 5),
    (A6, 6),
    (A7, 7),
    (A8, 8),
    (A9, 9),
    (A10, 10)
);
impl_tuple_utils!(
    (A0, 0),
    (A1, 1),
    (A2, 2),
    (A3, 3),
    (A4, 4),
    (A5, 5),
    (A6, 6),
    (A7, 7),
    (A8, 8),
    (A9, 9),
    (A10, 10),
    (A11, 11)
);

/// The verdict of a [`FilterClass`]: [`TrueType`] keeps the element, [`FalseType`] drops it.
pub trait Select {
    /// The tuple `T` with `H` prepended when the element is kept, else `T`.
    type Out<H, T: Prepend<H>>;
    /// Prepends `h` to `t` when the element is kept, else drops it.
    fn select<H, T: Prepend<H>>(h: H, t: T) -> Self::Out<H, T>;
}

/// The element is kept.
pub struct TrueType;
/// The element is dropped.
pub struct FalseType;

impl Select for TrueType {
    type Out<H, T: Prepend<H>> = T::Output;
    #[inline(always)]
    fn select<H, T: Prepend<H>>(h: H, t: T) -> T::Output {
        t.prepend(h)
    }
}

impl Select for FalseType {
    type Out<H, T: Prepend<H>> = T;
    #[inline(always)]
    fn select<H, T: Prepend<H>>(_h: H, t: T) -> T {
        t
    }
}

/// Whether elements of type `T` pass the filter `Self`.
pub trait FilterClass<T> {
    /// [`TrueType`] or [`FalseType`].
    type Value: Select;
}

/// A tuple with one more element in front.
pub trait Prepend<H> {
    /// `(H, Self...)`
    type Output;
    /// `(h, self...)`
    fn prepend(self, h: H) -> Self::Output;
}

macro_rules! impl_prepend {
    ($(($t:ident, $i:tt)),*) => {
        impl<H $(, $t)*> Prepend<H> for ($($t,)*) {
            type Output = (H, $($t,)*);
            #[inline(always)]
            #[allow(clippy::unused_unit)]
            fn prepend(self, h: H) -> Self::Output {
                let _ = &self;
                (h, $(self.$i,)*)
            }
        }
    };
}

impl_prepend!();
impl_prepend!((A0, 0));
impl_prepend!((A0, 0), (A1, 1));
impl_prepend!((A0, 0), (A1, 1), (A2, 2));
impl_prepend!((A0, 0), (A1, 1), (A2, 2), (A3, 3));
impl_prepend!((A0, 0), (A1, 1), (A2, 2), (A3, 3), (A4, 4));
impl_prepend!((A0, 0), (A1, 1), (A2, 2), (A3, 3), (A4, 4), (A5, 5));
impl_prepend!((A0, 0), (A1, 1), (A2, 2), (A3, 3), (A4, 4), (A5, 5), (A6, 6));
impl_prepend!((A0, 0), (A1, 1), (A2, 2), (A3, 3), (A4, 4), (A5, 5), (A6, 6), (A7, 7));
impl_prepend!((A0, 0), (A1, 1), (A2, 2), (A3, 3), (A4, 4), (A5, 5), (A6, 6), (A7, 7), (A8, 8));
impl_prepend!(
    (A0, 0),
    (A1, 1),
    (A2, 2),
    (A3, 3),
    (A4, 4),
    (A5, 5),
    (A6, 6),
    (A7, 7),
    (A8, 8),
    (A9, 9)
);
impl_prepend!(
    (A0, 0),
    (A1, 1),
    (A2, 2),
    (A3, 3),
    (A4, 4),
    (A5, 5),
    (A6, 6),
    (A7, 7),
    (A8, 8),
    (A9, 9),
    (A10, 10)
);

/// The elements of a tuple whose type passes the filter `F`, in order.
///
/// Implemented recursively over the head and the tail of the tuple.
pub trait TupleFilter<F> {
    /// The filtered tuple type.
    type Output;
    /// The filtered tuple.
    fn tuple_filter(self) -> Self::Output;
}

impl<F> TupleFilter<F> for () {
    type Output = ();
    #[inline(always)]
    fn tuple_filter(self) {}
}

macro_rules! impl_tuple_filter {
    ($h:ident $(, $t:ident)*) => {
        impl<F, $h $(, $t)*> TupleFilter<F> for ($h, $($t,)*)
        where
            F: FilterClass<$h>,
            ($($t,)*): TupleFilter<F>,
            <($($t,)*) as TupleFilter<F>>::Output: Prepend<$h>,
        {
            type Output = <<F as FilterClass<$h>>::Value as Select>::Out<$h, <($($t,)*) as TupleFilter<F>>::Output>;
            #[inline(always)]
            #[allow(non_snake_case)]
            fn tuple_filter(self) -> Self::Output {
                let ($h, $($t,)*) = self;
                <<F as FilterClass<$h>>::Value as Select>::select($h, ($($t,)*).tuple_filter())
            }
        }
    };
}

impl_tuple_filter!(A0);
impl_tuple_filter!(A0, A1);
impl_tuple_filter!(A0, A1, A2);
impl_tuple_filter!(A0, A1, A2, A3);
impl_tuple_filter!(A0, A1, A2, A3, A4);
impl_tuple_filter!(A0, A1, A2, A3, A4, A5);
impl_tuple_filter!(A0, A1, A2, A3, A4, A5, A6);
impl_tuple_filter!(A0, A1, A2, A3, A4, A5, A6, A7);
impl_tuple_filter!(A0, A1, A2, A3, A4, A5, A6, A7, A8);
impl_tuple_filter!(A0, A1, A2, A3, A4, A5, A6, A7, A8, A9);
impl_tuple_filter!(A0, A1, A2, A3, A4, A5, A6, A7, A8, A9, A10);
impl_tuple_filter!(A0, A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11);

/// Filters the elements of `t` by type: returns a tuple of only the elements whose type
/// `T` has `<F as FilterClass<T>>::Value = TrueType`, keeping their order.
#[inline]
pub fn tuple_filter_by_type<F, T: TupleFilter<F>>(t: T) -> T::Output {
    t.tuple_filter()
}

/// Like [`tuple_filter_by_type`], on a clone of `t`.
#[inline]
pub fn tuple_filter_by_type_ref<F, T: TupleFilter<F> + Clone>(t: &T) -> T::Output {
    t.clone().tuple_filter()
}

/// Free-function form of [`TupleMap::tuple_map`].
#[inline]
pub fn tuple_map<T: TupleMap<F>, F>(t: T, f: F) -> T::Output {
    t.tuple_map(f)
}

/// Free-function form of [`TupleForEach::tuple_for_each`].
#[inline]
pub fn tuple_for_each<T: TupleForEach<F>, F>(t: T, f: F) {
    t.tuple_for_each(f)
}
