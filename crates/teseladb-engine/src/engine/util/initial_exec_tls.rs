//! Thread-local variables of the initial-exec TLS model, for the hot paths.
//!
//! Code compiled directly into an executable can address thread-locals with the cheap
//! local-exec model. A `thread_local!` in a Rust library crate is compiled for the
//! general-dynamic model instead, whatever executable it ends up in: a TLS descriptor call, which the linker
//! relaxes to `movz/movk/nop/nop`, but only after the compiler has given the function a frame
//! (the call clobbers the link register) and moved its first argument out of `x0`.
//!
//! On aarch64 (the measured and deployed target) and x86-64 the variables declared here are
//! ELF TLS symbols in `.tbss` addressed with the initial-exec sequence (a GOT slot holding
//! the offset from the thread pointer), which the linker relaxes to a constant in an
//! executable (`movz/movk` on aarch64, `mov $imm` on x86-64). It is the technique of the
//! allocator's thread-locals (`core::memory`, `with_allocator_tls!`), which uses
//! [`thread_pointer`] and [`initial_exec_offset!`] from here; elsewhere the variables are
//! const `thread_local!`s.

/// The thread pointer (`tpidr_el0`). `pure` and `nomem`: constant for the thread, so LLVM
/// reads it once per function.
#[cfg(target_arch = "aarch64")]
#[inline(always)]
pub(crate) fn thread_pointer() -> usize {
    let tp: usize;
    // SAFETY: reads a system register that user code may read; no memory, stack or flags.
    unsafe {
        std::arch::asm!("mrs {tp}, tpidr_el0", tp = out(reg) tp, options(pure, nomem, nostack, preserves_flags));
    }
    tp
}

/// The thread pointer (`%fs:0`, the TCB's pointer to itself, which the x86-64 TLS ABI keeps
/// there so that code can learn the `%fs` base). `pure` and `nomem`: the word is written by
/// the C runtime before the thread runs and never changes; it is not memory of the program,
/// and LLVM models its own thread pointer read the same way (`llvm.thread.pointer` is
/// `readnone`). LLVM reads it once per function.
#[cfg(target_arch = "x86_64")]
#[inline(always)]
pub(crate) fn thread_pointer() -> usize {
    let tp: usize;
    // SAFETY: reads the TCB self pointer through `%fs`, which every thread of a program
    // linked with the C runtime has; no other memory, stack or flags.
    unsafe {
        std::arch::asm!("mov {tp}, qword ptr fs:[0]", tp = out(reg) tp, options(pure, nomem, nostack, preserves_flags));
    }
    tp
}

/// `initial_exec_offset!(<pieces of the symbol name>)`: the offset of the ELF TLS symbol from
/// the thread pointer, read from its GOT slot with the initial-exec sequence (the pieces are
/// arguments of `concat!`). In an executable the linker turns it into the constant.
macro_rules! initial_exec_offset {
    ($($sym:tt)+) => {{
        let off: usize;
        // SAFETY: reads a GOT slot, writes only the output register; no stack, no flags.
        // `pure` and `nomem`: the slot holds the symbol's constant offset from the thread
        // pointer; the dynamic loader fills it before any code of the program runs and it is
        // read-only afterwards (RELRO), so LLVM may merge repeated reads, as it does with the
        // address of a `#[thread_local]`.
        #[cfg(target_arch = "aarch64")]
        unsafe {
            ::std::arch::asm!(
                concat!("adrp {o}, :gottprel:", $($sym)+),
                concat!("ldr {o}, [{o}, #:gottprel_lo12:", $($sym)+, "]"),
                o = out(reg) off,
                options(pure, nomem, nostack, preserves_flags),
            );
        }
        // SAFETY: as above (R_X86_64_GOTTPOFF, relaxed to `mov $tpoff, reg` by the linker).
        #[cfg(target_arch = "x86_64")]
        unsafe {
            ::std::arch::asm!(
                concat!("mov {o}, qword ptr [rip + ", $($sym)+, "@GOTTPOFF]"),
                o = out(reg) off,
                options(pure, nomem, nostack, preserves_flags),
            );
        }
        off
    }};
}

pub(crate) use initial_exec_offset;

/// `initial_exec_thread_local! { fn name() -> Type, symbol = "symbol"; }` defines a
/// thread-local `Type` whose initial value is all zero bits, and `fn name() -> *const Type`,
/// the address of the calling thread's copy (valid for the life of the thread).
///
/// The type must be one for which all zero bits are a valid value (a null pointer, a zero
/// integer, a `Cell` of one of those), and it should give access through interior mutability
/// (`Cell`), since the variable is reached through a shared pointer. The symbol must be unique
/// in the program; it is global and hidden, so code of other crates into which the accessor
/// is inlined links to the same variable, and nothing interposes it.
macro_rules! initial_exec_thread_local {
    ($(#[$attr:meta])* $vis:vis fn $name:ident() -> $ty:ty, symbol = $sym:literal;) => {
        // The variable: `.tbss` (all zero bits in every thread), aligned and sized as
        // the type. `%nobits` and `%object` are accepted by both targets' assemblers.
        #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
        ::std::arch::global_asm!(
            concat!(".pushsection .tbss.", $sym, ",\"awT\",%nobits"),
            ".p2align {align}",
            concat!(".globl ", $sym),
            concat!(".hidden ", $sym),
            concat!(".type ", $sym, ",%object"),
            concat!(".size ", $sym, ", {size}"),
            concat!($sym, ":"),
            ".zero {size}",
            ".popsection",
            align = const ::std::mem::align_of::<$ty>().trailing_zeros(),
            size = const ::std::mem::size_of::<$ty>(),
        );
        $(#[$attr])*
        #[inline(always)]
        $vis fn $name() -> *const $ty {
            #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
            {
                let off = $crate::engine::util::initial_exec_tls::initial_exec_offset!($sym);
                // The calling thread's copy, which the C runtime allocates and zeroes before
                // the thread runs, aligned as declared above, and keeps until the thread is
                // gone.
                $crate::engine::util::initial_exec_tls::thread_pointer().wrapping_add(off) as *const $ty
            }
            #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
            {
                ::std::thread_local! {
                    // SAFETY: all zero bits are a valid value of the type (see the macro).
                    static VAR: $ty = const { unsafe { ::std::mem::zeroed() } };
                }
                VAR.with(|v| v as *const $ty)
            }
        }
    };
}

pub(crate) use initial_exec_thread_local;
