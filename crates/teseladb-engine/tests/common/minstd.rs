//! A deterministic random engine (the minimal standard linear congruential generator) and
//! a uniform integer distribution over it, so the tests that draw pseudo-random values walk
//! fixed, reproducible sequences.

#![allow(dead_code)]

/// Minimal standard LCG: x = x * 16807 mod (2^31 - 1), seeded with 1 by default.
#[derive(Clone, Debug)]
pub struct DefaultRandomEngine {
    x: u64,
}

impl Default for DefaultRandomEngine {
    fn default() -> Self {
        DefaultRandomEngine { x: 1 }
    }
}

impl DefaultRandomEngine {
    pub const MIN: u64 = 1;
    pub const MAX: u64 = 2_147_483_646;

    pub fn new(seed: u64) -> Self {
        let s = seed % 2_147_483_647;
        DefaultRandomEngine { x: if s == 0 { 1 } else { s } }
    }

    /// The next value of the sequence, in [1, 2^31 - 2].
    pub fn next(&mut self) -> u64 {
        self.x = self.x * 16807 % 2_147_483_647;
        self.x
    }
}

/// A uniform integer distribution over [a, b], drawn with the rejection method for a
/// range smaller than the engine's.
#[derive(Clone, Copy, Debug)]
pub struct UniformIntDistribution {
    a: u64,
    b: u64,
}

impl UniformIntDistribution {
    pub fn new(a: u64, b: u64) -> Self {
        UniformIntDistribution { a, b }
    }

    pub fn sample(&self, g: &mut DefaultRandomEngine) -> u64 {
        self.sample_fn(&mut || g.next())
    }

    /// Draws from any engine with the same range given as its next-value function, such as
    /// `teseladb::engine::testing::random::testing::local_random`.
    pub fn sample_fn(&self, next: &mut dyn FnMut() -> u64) -> u64 {
        let urngrange = DefaultRandomEngine::MAX - DefaultRandomEngine::MIN;
        let urange = self.b - self.a;
        if urngrange > urange {
            let uerange = urange + 1;
            let scaling = urngrange / uerange;
            let past = uerange * scaling;
            loop {
                let ret = next() - DefaultRandomEngine::MIN;
                if ret < past {
                    return ret / scaling + self.a;
                }
            }
        }
        if urngrange == urange {
            return next() - DefaultRandomEngine::MIN + self.a;
        }
        // urngrange < urange: compose two draws.
        loop {
            let uerngrange = urngrange + 1;
            let tmp =
                uerngrange * UniformIntDistribution::new(0, urange / uerngrange).sample_fn(next);
            let ret = tmp + (next() - DefaultRandomEngine::MIN);
            if ret <= urange && ret >= tmp {
                return ret + self.a;
            }
        }
    }
}
