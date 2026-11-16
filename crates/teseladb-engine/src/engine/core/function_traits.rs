//! Compile-time description of callables: return type, argument tuple and arity.
//!
//! [`FunctionTraits`] is implemented for function pointers (`fn`, `unsafe fn` and
//! `extern "C" fn`) of up to eight arguments, [`FunctionArg`] gives the type of argument `N`,
//! and [`FnTraits`] describes any `FnOnce` closure whose argument tuple is named explicitly,
//! because closure types cannot be introspected. Generic code uses these traits to name the
//! result or argument types of a callable it receives.

/// Compile-time information about a function pointer type.
pub trait FunctionTraits {
    /// The type the function returns.
    type ReturnType;
    /// The argument types as a tuple, `()` for a function with no arguments.
    type ArgsAsTuple;
    /// The safe, Rust-ABI function pointer type with the same signature.
    type Signature;
    /// The number of arguments.
    const ARITY: usize;
}

/// The type of argument `N` (zero based) of a function pointer type.
pub trait FunctionArg<const N: usize>: FunctionTraits {
    /// The type of argument `N`.
    type Type;
}

/// Compile-time information about any callable `F: FnOnce(Args...) -> Ret`, with `Args` given
/// as a tuple.
pub trait FnTraits<Args> {
    /// The type the callable returns.
    type ReturnType;
    /// The number of arguments, the length of `Args`.
    const ARITY: usize;
}

macro_rules! impl_function_traits {
    ($n:expr; $($idx:tt $a:ident),*) => {
        impl<Ret, $($a),*> FunctionTraits for fn($($a),*) -> Ret {
            type ReturnType = Ret;
            type ArgsAsTuple = ($($a,)*);
            type Signature = fn($($a),*) -> Ret;
            const ARITY: usize = $n;
        }
        impl<Ret, $($a),*> FunctionTraits for unsafe fn($($a),*) -> Ret {
            type ReturnType = Ret;
            type ArgsAsTuple = ($($a,)*);
            type Signature = fn($($a),*) -> Ret;
            const ARITY: usize = $n;
        }
        impl<Ret, $($a),*> FunctionTraits for extern "C" fn($($a),*) -> Ret {
            type ReturnType = Ret;
            type ArgsAsTuple = ($($a,)*);
            type Signature = fn($($a),*) -> Ret;
            const ARITY: usize = $n;
        }
        impl<F, Ret, $($a),*> FnTraits<($($a,)*)> for F where F: FnOnce($($a),*) -> Ret {
            type ReturnType = Ret;
            const ARITY: usize = $n;
        }
        impl_function_traits!(@args $($idx $a),* ; $($idx $a),*);
    };
    (@args ; $($all_idx:tt $all:ident),*) => {};
    (@args $i:tt $t:ident $(, $ri:tt $rt:ident)* ; $($all_idx:tt $all:ident),*) => {
        impl<Ret, $($all),*> FunctionArg<$i> for fn($($all),*) -> Ret {
            type Type = $t;
        }
        impl_function_traits!(@args $($ri $rt),* ; $($all_idx $all),*);
    };
}

impl_function_traits!(0;);
impl_function_traits!(1; 0 A0);
impl_function_traits!(2; 0 A0, 1 A1);
impl_function_traits!(3; 0 A0, 1 A1, 2 A2);
impl_function_traits!(4; 0 A0, 1 A1, 2 A2, 3 A3);
impl_function_traits!(5; 0 A0, 1 A1, 2 A2, 3 A3, 4 A4);
impl_function_traits!(6; 0 A0, 1 A1, 2 A2, 3 A3, 4 A4, 5 A5);
impl_function_traits!(7; 0 A0, 1 A1, 2 A2, 3 A3, 4 A4, 5 A5, 6 A6);
impl_function_traits!(8; 0 A0, 1 A1, 2 A2, 3 A3, 4 A4, 5 A5, 6 A6, 7 A7);
