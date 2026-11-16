//! Deterministic per-thread random numbers for tests.
//!
//! Tests that need reproducible pseudo-random input use [`testing::local_random`], backed
//! by a per-thread [`testing::DefaultRandomEngine`]. Each thread (each shard) has its own
//! engine, so no synchronization is involved; reseed it with
//! [`testing::seed_local_random_engine`] to replay a sequence.

/// Random number helpers.
pub mod testing {
    use std::cell::Cell;

    /// The minimal-standard linear congruential generator `x = x * 16807 mod 2147483647`
    /// (Park and Miller's `minstd_rand0`).
    ///
    /// A seed of 0 (modulo 2147483647) is replaced by 1, since 0 is a fixed point.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct DefaultRandomEngine {
        _x: u32,
    }

    impl DefaultRandomEngine {
        const MULTIPLIER: u64 = 16807;
        const MODULUS: u64 = 2147483647;
        /// The default seed.
        pub const DEFAULT_SEED: u32 = 1;

        /// An engine seeded with `s`.
        #[inline]
        pub const fn new(s: u32) -> Self {
            let x = (s as u64 % Self::MODULUS) as u32;
            DefaultRandomEngine { _x: if x == 0 { 1 } else { x } }
        }

        /// Reseeds the engine.
        #[inline]
        pub fn seed(&mut self, s: u32) {
            *self = Self::new(s);
        }

        /// The next value, in `[1, 2147483646]`.
        #[inline]
        #[allow(clippy::should_implement_trait)]
        pub fn next(&mut self) -> u32 {
            self._x = ((u64::from(self._x) * Self::MULTIPLIER) % Self::MODULUS) as u32;
            self._x
        }
    }

    impl Default for DefaultRandomEngine {
        fn default() -> Self {
            Self::new(Self::DEFAULT_SEED)
        }
    }

    thread_local! {
        /// This thread's engine, seeded with [`DefaultRandomEngine::DEFAULT_SEED`].
        pub static LOCAL_RANDOM_ENGINE: Cell<DefaultRandomEngine> =
            const { Cell::new(DefaultRandomEngine::new(DefaultRandomEngine::DEFAULT_SEED)) };
    }

    /// Returns the next value of this thread's engine.
    #[inline]
    pub fn local_random() -> u32 {
        LOCAL_RANDOM_ENGINE.with(|e| {
            let mut v = e.get();
            let r = v.next();
            e.set(v);
            r
        })
    }

    /// Reseeds this thread's engine with `s`.
    #[inline]
    pub fn seed_local_random_engine(s: u32) {
        LOCAL_RANDOM_ENGINE.with(|e| e.set(DefaultRandomEngine::new(s)));
    }
}
