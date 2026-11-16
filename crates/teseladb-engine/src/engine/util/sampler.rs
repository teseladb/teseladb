//! Byte-interval sampling of allocations.
//!
//! [`Sampler`] decides which allocations the memory profiler records. It samples on
//! average once every `sampling_interval` bytes: the distance to the next sample is drawn
//! from an exponential distribution (approximating the geometric distribution of a
//! per-byte Bernoulli trial), so the choice is unbiased with respect to allocation sizes.
//!
//! Differences from a textbook per-byte sampler:
//! - a sample always accounts for the sampling interval (or the allocation size if
//!   larger), never for several intervals, so a free only needs to know the interval;
//! - an interval of 0 turns sampling off;
//! - the fast path ([`Sampler::maybe_sample`]) is a single subtraction and compare;
//! - sampling can be paused temporarily with [`DisableSamplingTemporarily`] without
//!   redrawing the next interval.
//!
//! The pseudo-random source is a 64-bit Mersenne Twister ([`Mt19937_64`]) seeded from
//! `/dev/urandom` ([`RandomDevice`]).

/// The 64-bit Mersenne Twister (MT19937-64) pseudo-random generator.
#[derive(Clone)]
pub struct Mt19937_64 {
    mt: [u64; 312],
    mti: usize,
}

impl Mt19937_64 {
    const NN: usize = 312;
    const MM: usize = 156;
    const MATRIX_A: u64 = 0xB502_6F5A_A966_19E9;
    const UM: u64 = 0xFFFF_FFFF_8000_0000;
    const LM: u64 = 0x7FFF_FFFF;
    /// The standard default seed.
    pub const DEFAULT_SEED: u64 = 5489;

    /// Creates a generator seeded with `seed`.
    pub fn new(seed: u64) -> Self {
        let mut mt = [0u64; 312];
        mt[0] = seed;
        for i in 1..Self::NN {
            mt[i] = 6364136223846793005u64
                .wrapping_mul(mt[i - 1] ^ (mt[i - 1] >> 62))
                .wrapping_add(i as u64);
        }
        Mt19937_64 { mt, mti: Self::NN }
    }

    /// Reseeds the generator with `seed`.
    pub fn seed(&mut self, seed: u64) {
        *self = Self::new(seed);
    }

    fn twist(&mut self) {
        for i in 0..Self::NN {
            let x = (self.mt[i] & Self::UM) | (self.mt[(i + 1) % Self::NN] & Self::LM);
            let mut xa = x >> 1;
            if x & 1 != 0 {
                xa ^= Self::MATRIX_A;
            }
            self.mt[i] = self.mt[(i + Self::MM) % Self::NN] ^ xa;
        }
        self.mti = 0;
    }

    /// Returns the next 64-bit value.
    pub fn next_u64(&mut self) -> u64 {
        if self.mti >= Self::NN {
            self.twist();
        }
        let mut x = self.mt[self.mti];
        self.mti += 1;
        x ^= (x >> 29) & 0x5555_5555_5555_5555;
        x ^= (x << 17) & 0x71D6_7FFF_EDA6_0000;
        x ^= (x << 37) & 0xFFF7_EEE0_0000_0000;
        x ^= x >> 43;
        x
    }

    /// Returns a uniformly distributed `f64` in `[0, 1)` built from one 64-bit draw
    /// divided by 2^64 (results that round to 1.0 are clamped to the largest double
    /// below 1).
    pub fn generate_canonical(&mut self) -> f64 {
        // One 64-bit draw covers the 53 bits of a double.
        let r = 18446744073709551616.0f64; // max - min + 1 = 2^64
        let sum = self.next_u64() as f64;
        let ret = sum / r;
        if ret >= 1.0 {
            // The largest double below 1.0.
            return f64::from_bits(1.0f64.to_bits() - 1);
        }
        ret
    }
}

/// A source of non-deterministic random numbers: reads `/dev/urandom`.
pub struct RandomDevice;

impl RandomDevice {
    /// Creates the device (nothing is opened until [`call`](Self::call)).
    pub fn new() -> Self {
        RandomDevice
    }

    /// Returns a 32-bit random value read from `/dev/urandom`.
    ///
    /// # Panics
    ///
    /// Panics if `/dev/urandom` cannot be opened or read.
    pub fn call(&mut self) -> u32 {
        use std::io::Read;
        let mut b = [0u8; 4];
        if let Ok(mut f) = std::fs::File::open("/dev/urandom")
            && f.read_exact(&mut b).is_ok()
        {
            return u32::from_ne_bytes(b);
        }
        // There is no fallback source of entropy.
        panic!("random_device could not be read");
    }
}

impl Default for RandomDevice {
    fn default() -> Self {
        Self::new()
    }
}

/// The exponential distribution with rate `lambda`.
pub struct ExponentialDistribution {
    lambda: f64,
}

impl ExponentialDistribution {
    /// Creates the distribution with rate `lambda` (mean `1 / lambda`).
    pub fn new(lambda: f64) -> Self {
        ExponentialDistribution { lambda }
    }

    /// Draws a value: `-ln(1 - u) / lambda` with `u` from
    /// [`Mt19937_64::generate_canonical`].
    pub fn sample(&self, urng: &mut Mt19937_64) -> f64 {
        -(1.0 - urng.generate_canonical()).ln() / self.lambda
    }
}

/// Decides which allocations are sampled; see the [module documentation](self).
///
/// Use: call [`maybe_sample`](Self::maybe_sample) on every allocation (hot path) and,
/// only when it returns `true`, [`definitely_sample`](Self::definitely_sample).
pub struct Sampler {
    sampling_interval_: u64, // Sample every N bytes ; 0 means off
    sampling_rate_: f64,     // 1 / sampling_interval_ ; used by the exp distribution
    // How many bytes remain to be allocated before we take a sample.
    // Specifically, if this member has value N, a sample will be taken of the allocation
    // that allocates the Nth+1 byte.
    interval_to_next_sample_: i64,
    // Seeds `random_gen` in the constructor and is not read afterwards.
    #[allow(dead_code)]
    rd_device: RandomDevice,
    random_gen: Mt19937_64,
}

impl Sampler {
    /// Creates a sampler with sampling off (interval 0), its generator seeded with one
    /// 32-bit value from [`RandomDevice`].
    ///
    /// # Panics
    ///
    /// Panics if `/dev/urandom` cannot be read.
    pub fn new() -> Self {
        let mut rd_device = RandomDevice::new();
        // The generator is seeded with one 32-bit value.
        let seed = rd_device.call() as u64;
        let mut s = Sampler {
            sampling_interval_: 0,
            sampling_rate_: 0.0,
            interval_to_next_sample_: 0,
            rd_device,
            random_gen: Mt19937_64::new(seed),
        };
        s.set_sampling_interval(0);
        s
    }

    /// Sets the sampling interval in bytes and draws the distance to the next sample.
    /// An interval of 0 means never sample.
    pub fn set_sampling_interval(&mut self, sampling_interval: u64) {
        self.sampling_interval_ = sampling_interval;
        if self.sampling_interval_ == 0 {
            // Set the interval very large. This means in practice we will
            // likely never get this below zero and hence it's unlikely we will
            // ever have to run the reset path with sampling off
            self.interval_to_next_sample_ = i64::MAX;
            return;
        }
        self.sampling_rate_ = 1.0 / self.sampling_interval_ as f64;
        self.interval_to_next_sample_ = self.next_sampling_interval();
    }

    /// Updates the sampling state (bytes remaining until the next sample) and returns
    /// `true` if this allocation of `alloc_size` bytes may be sampled.
    ///
    /// If it returns `false`, the allocation is definitely not sampled. If it returns
    /// `true`, call [`definitely_sample`](Self::definitely_sample) to decide. Hot path:
    /// one subtraction and one compare.
    #[inline(always)]
    pub fn maybe_sample(&mut self, alloc_size: usize) -> bool {
        self.interval_to_next_sample_ =
            self.interval_to_next_sample_.wrapping_sub(alloc_size as i64);
        self.interval_to_next_sample_ < 0
    }

    /// To be called when [`maybe_sample`](Self::maybe_sample) returned `true` for an
    /// allocation. Returns `true` if a sample should be taken, and resets the countdown to
    /// the next sample.
    pub fn definitely_sample(&mut self, alloc_size: usize) -> bool {
        // this will hold if maybe_sample returned false for this allocation
        if self.interval_to_next_sample_ >= 0 {
            return false;
        }
        self.reset_interval_to_next_sample(alloc_size);
        self.sampling_interval_ != 0 // sampling interval 0 means off
    }

    /// The sampling interval in bytes (0 when off).
    pub fn sampling_interval(&self) -> u64 {
        self.sampling_interval_
    }

    /// How many bytes a sampled allocation of `allocation_size` bytes accounts for: the
    /// larger of the size and the sampling interval.
    pub fn sample_size(&self, allocation_size: usize) -> usize {
        std::cmp::max(allocation_size, self.sampling_interval_ as usize)
    }

    /// Pauses sampling until the returned guard is dropped.
    ///
    /// This is more efficient and statistically more correct than calling
    /// `set_sampling_interval(0)` and then `set_sampling_interval(rate)`, which would
    /// reset the distance to the next sample and redraw the exponential distribution;
    /// the guard restores the exact previous state instead.
    ///
    /// # Safety
    /// The same contract as [`DisableSamplingTemporarily::new`]: the guard keeps `this`
    /// and writes to it when dropped.
    pub unsafe fn pause_sampling(this: *mut Sampler) -> DisableSamplingTemporarily {
        // SAFETY: the caller upholds the contract of `DisableSamplingTemporarily::new`.
        unsafe { DisableSamplingTemporarily::new(this) }
    }

    /// Resets interval_to_next_sample_ by repeatedly drawing from the
    /// exponential distribution given an allocation of size `alloc_size`
    /// breached the current interval
    fn reset_interval_to_next_sample(&mut self, alloc_size: usize) {
        if self.sampling_interval_ == 0 {
            // sampling is off
            self.interval_to_next_sample_ = i64::MAX;
        } else {
            // Large allocations we will just consider in whole. This avoids
            // having to sample the distribution too many times if a large alloc
            // took us very negative we just add the alloc size back on
            if alloc_size as u64 > self.sampling_interval_ {
                self.interval_to_next_sample_ =
                    self.interval_to_next_sample_.wrapping_add(alloc_size as i64);
            } else {
                while self.interval_to_next_sample_ < 0 {
                    let n = self.next_sampling_interval();
                    self.interval_to_next_sample_ += n;
                }
            }
        }
    }

    fn next_sampling_interval(&mut self) -> i64 {
        let dist = ExponentialDistribution::new(self.sampling_rate_);
        let next = dist.sample(&mut self.random_gen) as i64;
        // We approximate the geometric distribution using an exponential
        // distribution.
        next
    }
}

impl Default for Sampler {
    fn default() -> Self {
        Self::new()
    }
}

/// Scope guard that pauses a [`Sampler`] and restores its exact state when dropped.
///
/// The default value holds no sampler and does nothing on drop.
pub struct DisableSamplingTemporarily {
    sampler_: *mut Sampler,
    previous_sampling_interval_: u64, // sampling interval before pausing
    previous_sampling_rate_: f64,     // sampling rate before pausing
    previous_interval_to_next_sample_: i64, // interval to next sample before pausing
}

impl Default for DisableSamplingTemporarily {
    fn default() -> Self {
        DisableSamplingTemporarily {
            sampler_: std::ptr::null_mut(),
            previous_sampling_interval_: 0,
            previous_sampling_rate_: 0.0,
            previous_interval_to_next_sample_: 0,
        }
    }
}

impl DisableSamplingTemporarily {
    /// Pauses `sampler` until the guard is dropped; the guard keeps the pointer.
    ///
    /// # Safety
    /// `sampler` points to a live sampler that stays in place until the guard is dropped,
    /// and no reference to it is live when the guard is created or dropped: the sampler
    /// may be used meanwhile (the allocator does), but only through pointers that do not
    /// invalidate this one, such as the same raw pointer.
    pub unsafe fn new(sampler: *mut Sampler) -> Self {
        // SAFETY: guaranteed by the caller.
        unsafe {
            let r = DisableSamplingTemporarily {
                sampler_: sampler,
                previous_sampling_interval_: (*sampler).sampling_interval_,
                previous_sampling_rate_: (*sampler).sampling_rate_,
                previous_interval_to_next_sample_: (*sampler).interval_to_next_sample_,
            };
            (*sampler).set_sampling_interval(0);
            r
        }
    }
}

impl Drop for DisableSamplingTemporarily {
    fn drop(&mut self) {
        if !self.sampler_.is_null() {
            // SAFETY: the contract of `new`: the sampler is live, in place, and not
            // referenced while the guard drops.
            unsafe {
                (*self.sampler_).sampling_interval_ = self.previous_sampling_interval_;
                (*self.sampler_).sampling_rate_ = self.previous_sampling_rate_;
                (*self.sampler_).interval_to_next_sample_ = self.previous_interval_to_next_sample_;
            }
        }
    }
}
