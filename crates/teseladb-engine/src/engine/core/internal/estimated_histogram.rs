//! Fixed-size, pseudo-exponential histograms for metrics.
//!
//! [`ApproximateExponentialHistogram`] counts values in buckets whose limits grow like a
//! floating-point number (a power-of-two exponent plus a linear part), so adding a value is a
//! few bit operations and no allocation. [`TimeEstimatedHistogram`] is the instance used for
//! short task timings (0.5 ms to 33 s). Both convert to the metrics layer's histogram type,
//! including the native (sparse) histogram description of the Prometheus exposition format.
use std::time::Duration;

use crate::engine::core::future::{ExceptionPtr, RuntimeError};
use crate::engine::core::metrics_types::{Histogram, HistogramBucket, NativeHistogramInfo};

/// Floor of the base-2 logarithm of `n`, usable in constant expressions.
#[inline]
const fn log2floor(n: u64) -> u32 {
    if n == 0 { 0 } else { u64::BITS - 1 - n.leading_zeros() }
}

/// Ceiling of the base-2 logarithm of `n`, usable in constant expressions.
#[inline]
const fn log2ceil(n: u64) -> u32 {
    if n == 1 { 0 } else { u64::BITS - (n - 1).leading_zeros() }
}

/// `NUM_BUCKETS = log2floor(Max/Min) * Precision + 1`, for the `NUM_BUCKETS` parameter of
/// [`ApproximateExponentialHistogram`] (Rust cannot size an array from an expression of
/// generic parameters).
pub const fn num_buckets(min: u64, max: u64, precision: usize) -> usize {
    log2floor(max / min) as usize * precision + 1
}

/// This is a pseudo-exponential implementation of an estimated histogram.
///
/// An exponential-histogram with coefficient 'coef', is a histogram where for bucket 'i'
/// the lower limit is coef^i and the higher limit is coef^(i+1).
///
/// A pseudo-exponential is similar but the bucket limits are an approximation.
///
/// [`ApproximateExponentialHistogram`] is an efficient pseudo-exponential implementation.
///
/// The histogram is defined by a Min and Max value limits, and a Precision (all should be power of 2
/// and will be explained).
///
/// When adding a value to a histogram:
/// All values lower than Min will be included in the first bucket (the assumption is that it's
/// not suppose to happen but it is ok if it does).
///
/// All values higher than Max will be included in the last bucket that serves as the
/// infinity bucket (the assumption is that it can happen but it is rare).
///
/// Note the difference between the first and last buckets.
/// The first bucket is just like a regular bucket but has a second roll to collect unexpected low values.
/// The last bucket, also known as the infinity bucket, collect all values that passes the defined Max,
/// it only collect those values.
///
/// Buckets Distribution (limits)
/// =============================
/// The buckets limits in the histogram are defined similar to a floating-point representation.
///
/// Buckets limits have an exponent part and a linear part.
///
/// The exponential part is a power of 2. Each power-of-2 range [2^n..2^n+1)
/// is split linearly to 'Precision' number of buckets.
///
/// The total number of buckets is:
///   NUM_BUCKETS = log2(Max/Min)*Precision +1
///
/// For example, if the Min value is 128, the Max is 1024 and the Precision is 4, the number of buckets is 13.
///
/// Anything below 160 will be in the bucket 0, anything above 1024 will be in bucket 13.
/// Note that the first bucket will include all values below Min.
///
/// the range [128, 1024) will be split into log2(1024/128) = 3 ranges:
/// 128, 256, 512, 1024
/// Or more mathematically: [128, 256), [256, 512), [512,1024)
///
/// Each range is split into 4 (The Precision).
/// 128            | 256            | 512            | 1024
/// 128 160 192 224| 256 320 384 448| 512 640 768 896|
///
/// To get the exponential part of an index you divide by the Precision.
/// The linear part of the index is Modulus the precision.
///
/// Calculating the bucket lower limit of bucket i:
/// The exponential part: exp_part = 2^floor(i/Precision)* Min
///    with the above example 2^floor(i/4)*128
/// The linear part: (i%Precision) * (exp_part/Precision)
///    With the example: (i%4) * (exp_part/4)
///
/// So the lower limit of bucket 6:
///    2^floor(6/4)* 128  = 256
///    (6%4) * 256/4 = 128
///    lower-limit   = 384
///
/// How to find a bucket index for a value
/// =======================================
/// The bucket index consist of two parts:
/// higher bits (exponential part) are based on log2(value/min)
///
/// lower bits (linear part) are based on the 'n' MSB (ignoring the leading 1) where n=log2(Precision).
/// Continuing with the example where the number of precision bits: PRECISION_BITS = log2(4) = 2
///
/// for example: 330 (101001010)
/// The number of precision_bits: PRECISION_BITS = log2(4) = 2
/// higher bits: log2(330/128) = 1
/// MSB: 01 (the highest two bits following the leading 1)
/// So the index: 101 = 5
///
/// About the Min, Max and Precision
/// ================================
/// For Min, Max and Precision, choose numbers that are a power of 2.
///
/// Limitation: You must set the MIN value to be higher or equal to the Precision.
///
/// * `Min` - The lowest expected value. A value below that will be set to the Min value.
/// * `Max` - The highest number to consider. A value above that is considered infinite.
/// * `Precision` - The number of buckets in each range (where a range is the i^2..i^(2+1) )
/// * `NUM_BUCKETS` - `num_buckets(MIN, MAX, PRECISION)`, checked at compile time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ApproximateExponentialHistogram<
    const MIN: u64,
    const MAX: u64,
    const PRECISION: usize,
    const NUM_BUCKETS: usize,
> {
    _buckets: [u64; NUM_BUCKETS],
}

impl<const MIN: u64, const MAX: u64, const PRECISION: usize, const NUM_BUCKETS: usize>
    ApproximateExponentialHistogram<MIN, MAX, PRECISION, NUM_BUCKETS>
{
    const NUM_EXP_RANGES: u32 = log2floor(MAX / MIN);
    const CHECK_NUM_BUCKETS: () = assert!(
        NUM_BUCKETS == Self::NUM_EXP_RANGES as usize * PRECISION + 1,
        "NUM_BUCKETS must be num_buckets(MIN, MAX, PRECISION)"
    );
    const PRECISION_BITS: u32 = log2floor(PRECISION as u64);
    const BASESHIFT: u32 = log2floor(MIN);
    const LOWER_BITS_MASK: u64 = PRECISION as u64 - 1;
    const MIN_ID: usize = log2ceil(MIN) as usize * PRECISION + 1; // id0 (x,1]

    /// An empty histogram.
    #[inline]
    pub const fn new() -> Self {
        #[allow(clippy::let_unit_value)]
        let () = Self::CHECK_NUM_BUCKETS;
        // clear()
        Self { _buckets: [0; NUM_BUCKETS] }
    }

    /// Returns the bucket lower limit given the bucket id.
    /// The first and last bucket will always return the MIN and MAX respectively.
    #[inline]
    pub fn get_bucket_lower_limit(&self, bucket_id: u16) -> u64 {
        if bucket_id as usize == NUM_BUCKETS - 1 {
            return MAX;
        }
        let exp_rang = (bucket_id >> Self::PRECISION_BITS) as i16;
        (MIN << exp_rang)
            + ((bucket_id as u64 & Self::LOWER_BITS_MASK)
                << (exp_rang as i32 + Self::BASESHIFT as i32 - Self::PRECISION_BITS as i32))
    }

    /// Returns the bucket upper limit given the bucket id.
    /// The last bucket (infinity bucket) returns `u64::MAX`.
    #[inline]
    pub fn get_bucket_upper_limit(&self, bucket_id: u16) -> u64 {
        if bucket_id as usize == NUM_BUCKETS - 1 {
            return u64::MAX;
        }
        self.get_bucket_lower_limit(bucket_id + 1)
    }

    /// Find the bucket index for a given value
    /// The position of a value that is lower or equal to Min will always be 0.
    /// The position of a value that is higher or equal to MAX will always be NUM_BUCKETS - 1.
    #[inline]
    pub fn find_bucket_index(&self, mut val: u64) -> u16 {
        if val >= MAX {
            return (NUM_BUCKETS - 1) as u16;
        }
        if val <= MIN {
            return 0;
        }
        let range = log2floor(val) as u16;
        val >>= range as u32 - Self::PRECISION_BITS; // leave the top most N+1 bits where N is the resolution.
        (((range - Self::BASESHIFT as u16) << Self::PRECISION_BITS) as u64
            + (val & Self::LOWER_BITS_MASK)) as u16
    }

    /// clear the current values.
    #[inline]
    pub fn clear(&mut self) {
        self._buckets.fill(0);
    }

    /// Add an item to the histogram
    /// Increments the count of the bucket holding that value
    #[inline]
    pub fn add(&mut self, n: u64) {
        // `_buckets.at()`: the index is always in range, so the check never fires.
        let i = self.find_bucket_index(n) as usize;
        self._buckets[i] += 1;
    }

    /// returns the smallest value that could have been added to this histogram
    /// This method looks for the first non-empty bucket and returns its lower limit.
    /// Note that for non-empty histogram the lowest potentail value is Min.
    ///
    /// It will return 0 if the histogram is empty.
    pub fn min(&self) -> u64 {
        for i in 0..NUM_BUCKETS {
            if self._buckets[i] > 0 {
                return self.get_bucket_lower_limit(i as u16);
            }
        }
        0
    }

    /// returns the largest value that could have been added to this histogram.
    /// This method looks for the first non empty bucket and return its upper limit.
    /// If the histogram overflowed, it will returns UINT64_MAX.
    ///
    /// It will return 0 if the histogram is empty.
    pub fn max(&self) -> u64 {
        for i in (0..NUM_BUCKETS).rev() {
            if self._buckets[i] > 0 {
                return self.get_bucket_upper_limit(i as u16);
            }
        }
        0
    }

    /// merge a histogram to the current one.
    #[inline]
    pub fn merge(&mut self, b: &Self) -> &mut Self {
        for i in 0..NUM_BUCKETS {
            self._buckets[i] += b.get(i);
        }
        self
    }

    /// The count in bucket `bucket`.
    #[inline]
    pub fn get(&self, bucket: usize) -> u64 {
        self._buckets[bucket]
    }

    /// get a histogram quantile
    ///
    /// This method will returns the estimated value at a given quantile.
    /// If there are N values in the histogram.
    /// It would look for the bucket that the total number of elements in the buckets
    /// before it are less than N * quantile and return that bucket lower limit.
    ///
    /// For example, quantile(0.5) will find the bucket that that sum of all buckets values
    /// below it is less than half and will return that bucket lower limit.
    /// In this example, this is a median estimation.
    ///
    /// It will return 0 if the histogram is empty.
    pub fn quantile(&self, quantile: f32) -> Result<u64, ExceptionPtr> {
        if !(0.0..=1.0).contains(&quantile) {
            return Err(ExceptionPtr::from(RuntimeError(format!(
                "Invalid quantile value {:.6}. Value should be between 0 and 1",
                quantile
            ))));
        }
        let c = self.count();

        if c == 0 {
            return Ok(0); // no data
        }

        let pcount = (c as f32 * quantile).floor() as u64;
        let mut elements: u64 = 0;
        for i in 0..NUM_BUCKETS - 2 {
            if self._buckets[i] != 0 {
                elements += self._buckets[i];
                if elements >= pcount {
                    return Ok(self.get_bucket_lower_limit(i as u16));
                }
            }
        }
        Ok(MAX) // overflowed value is in the requested quantile
    }

    /// returns the mean histogram value (average of bucket offsets, weighted by count)
    /// It will return 0 if the histogram is empty.
    pub fn mean(&self) -> u64 {
        let mut elements: u64 = 0;
        let mut sum: f64 = 0.0;
        for i in 0..NUM_BUCKETS - 1 {
            elements += self._buckets[i];
            sum += (self._buckets[i] * self.get_bucket_lower_limit(i as u16)) as f64;
        }
        if elements != 0 { (sum / elements as f64) as u64 } else { 0 }
    }

    /// returns the number of buckets;
    #[inline]
    pub const fn size(&self) -> usize {
        NUM_BUCKETS
    }

    /// returns the total number of values inserted
    #[inline]
    pub fn count(&self) -> u64 {
        let mut sum: u64 = 0;
        for i in 0..NUM_BUCKETS {
            sum += self._buckets[i];
        }
        sum
    }

    /// get_schema the schema of a native histogram
    ///
    /// Native histograms (also known as sparse histograms), are an experimental Prometheus feature.
    ///
    /// schema defines the bucket schema. Currently, valid numbers are -4 <= n <= 8.
    /// They are all for base-2 bucket schemas, where 1 is a bucket boundary in each case, and
    /// then each power of two is divided into 2^n logarithmic buckets.
    /// Or in other words, each bucket boundary is the previous boundary times 2^(2^-n).
    #[inline]
    pub const fn get_schema(&self) -> i32 {
        Self::PRECISION_BITS as i32
    }

    /// return the histogram min as a native histogram ID
    ///
    /// Native histograms (also known as sparse histograms), are an experimental Prometheus feature.
    ///
    /// Native histograms always start from 1, while this histogram has a minimum first
    /// bucket. This function returns this histogram's minimum as a native histogram bucket id.
    #[inline]
    pub const fn min_as_native_histogram_id(&self) -> i32 {
        Self::MIN_ID as i32
    }

    /// Converts to the metrics layer's histogram: cumulative bucket counts with their upper
    /// limits, plus the native histogram schema and minimum id.
    pub fn to_metrics_histogram(&self) -> Histogram {
        let mut res = Histogram::default();
        res.buckets.resize(self.size() - 1, HistogramBucket::default());
        let mut cummulative_count: u64 = 0;
        res.sample_sum = 0.0;
        res.native_histogram = Some(NativeHistogramInfo {
            schema: self.get_schema(),
            min_id: self.min_as_native_histogram_id(),
        });

        for i in 0..NUM_BUCKETS - 1 {
            let upper_bound = self.get_bucket_lower_limit((i + 1) as u16);
            cummulative_count += self.get(i);
            let v = &mut res.buckets[i];
            v.upper_bound = upper_bound as f64;
            v.count = cummulative_count;
            res.sample_sum += (self.get(i) * upper_bound) as f64;
        }
        // The count serves as the infinite bucket
        res.sample_count = cummulative_count + self.get(NUM_BUCKETS - 1);
        res.sample_sum += (self.get(NUM_BUCKETS - 1)
            * self.get_bucket_lower_limit((NUM_BUCKETS - 1) as u16))
            as f64;
        res
    }
}

impl<const MIN: u64, const MAX: u64, const PRECISION: usize, const NUM_BUCKETS: usize> Default
    for ApproximateExponentialHistogram<MIN, MAX, PRECISION, NUM_BUCKETS>
{
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl<const MIN: u64, const MAX: u64, const PRECISION: usize, const NUM_BUCKETS: usize>
    std::ops::MulAssign<f64> for ApproximateExponentialHistogram<MIN, MAX, PRECISION, NUM_BUCKETS>
{
    /// multiple all the buckets content in the histogram by a constant
    #[inline]
    fn mul_assign(&mut self, v: f64) {
        for b in self._buckets.iter_mut() {
            *b = (*b as f64 * v) as u64;
        }
    }
}

impl<const MIN: u64, const MAX: u64, const PRECISION: usize, const NUM_BUCKETS: usize>
    std::ops::Index<usize> for ApproximateExponentialHistogram<MIN, MAX, PRECISION, NUM_BUCKETS>
{
    type Output = u64;
    #[inline]
    fn index(&self, b: usize) -> &u64 {
        &self._buckets[b]
    }
}

impl<const MIN: u64, const MAX: u64, const PRECISION: usize, const NUM_BUCKETS: usize>
    std::ops::IndexMut<usize>
    for ApproximateExponentialHistogram<MIN, MAX, PRECISION, NUM_BUCKETS>
{
    /// The count in bucket `b`, mutably.
    #[inline]
    fn index_mut(&mut self, b: usize) -> &mut u64 {
        &mut self._buckets[b]
    }
}

/// Merges `b` into `a` and returns the result.
#[inline]
pub fn base_estimated_histogram_merge<
    const MIN: u64,
    const MAX: u64,
    const P: usize,
    const N: usize,
>(
    mut a: ApproximateExponentialHistogram<MIN, MAX, P, N>,
    b: &ApproximateExponentialHistogram<MIN, MAX, P, N>,
) -> ApproximateExponentialHistogram<MIN, MAX, P, N> {
    a.merge(b);
    a
}

/// The bucket layout of [`TimeEstimatedHistogram`]: 512 us to 33554432 us, precision 4.
pub type TimeEstimatedHistogramBase =
    ApproximateExponentialHistogram<512, 33554432, 4, { num_buckets(512, 33554432, 4) }>;

/// estimated histogram for duration values
/// [`TimeEstimatedHistogram`] is used for short task timing.
/// It covers the range of 0.5ms to 33s with a precision of 4.
///
/// 512us, 640us, 768us, 896us, 1024us, 1280us, 1536us, 1792us...16s, 20s, 25s, 29s, 33s (33554432us)
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TimeEstimatedHistogram {
    base: TimeEstimatedHistogramBase,
}

impl TimeEstimatedHistogram {
    /// An empty histogram.
    #[inline]
    pub const fn new() -> Self {
        TimeEstimatedHistogram { base: TimeEstimatedHistogramBase::new() }
    }

    /// Adds the counts of `b` to this histogram.
    #[inline]
    pub fn merge(&mut self, b: &TimeEstimatedHistogram) -> &mut Self {
        self.base.merge(&b.base);
        self
    }

    /// Adds a value of `n` microseconds.
    #[inline]
    pub fn add_micro(&mut self, n: u64) {
        self.base.add(n);
    }

    /// Adds a latency (counted in microseconds).
    #[inline]
    pub fn add(&mut self, latency: Duration) {
        self.add_micro(latency.as_micros() as u64);
    }
}

impl std::ops::Deref for TimeEstimatedHistogram {
    type Target = TimeEstimatedHistogramBase;
    #[inline]
    fn deref(&self) -> &TimeEstimatedHistogramBase {
        &self.base
    }
}

impl std::ops::DerefMut for TimeEstimatedHistogram {
    #[inline]
    fn deref_mut(&mut self) -> &mut TimeEstimatedHistogramBase {
        &mut self.base
    }
}

/// Merges `b` into `a` and returns the result.
#[inline]
pub fn time_estimated_histogram_merge(
    mut a: TimeEstimatedHistogram,
    b: &TimeEstimatedHistogram,
) -> TimeEstimatedHistogram {
    a.merge(b);
    a
}
