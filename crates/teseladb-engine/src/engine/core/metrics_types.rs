//! Value types shared by the metrics layer: histograms and their buckets.
//!
//! A [`Histogram`] is a snapshot of a histogram's state, not an accumulator: a metric's source
//! fills one when the metrics are read, and the exporters (Prometheus, collectd) format it.
//! Histograms with the same bucket bounds can be added, which is how per-shard histograms are
//! aggregated. Everything here is re-exported by [`metrics`](crate::engine::core::metrics).

use crate::engine::core::future::ExceptionPtr;

/// One histogram bucket: an inclusive upper bound and the number of events in it.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct HistogramBucket {
    /// Number of events.
    pub count: u64,
    /// Upper bound of the bucket, inclusive.
    pub upper_bound: f64,
}

/// Parameters of a native (sparse, exponential-bucket) histogram, a Prometheus extension.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NativeHistogramInfo {
    /// The bucket schema `n`, with `-4 <= n <= 8`. All schemas are base 2: 1 is a bucket
    /// boundary and each power of two is divided into `2^n` logarithmic buckets, so each
    /// boundary is the previous one times `2^(2^-n)`.
    pub schema: i32,
    /// The id of the first bucket in this schema.
    pub min_id: i32,
}

/// The values of a histogram at one point in time.
///
/// This is a container, not a histogram implementation: histogram implementations return their
/// internal state in it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Histogram {
    /// Number of samples.
    pub sample_count: u64,
    /// Sum of the samples.
    pub sample_sum: f64,
    /// Buckets in increasing order of `upper_bound`; the `+Inf` bucket is optional.
    pub buckets: Vec<HistogramBucket>,
    /// The native histogram parameters, if this is one.
    pub native_histogram: Option<NativeHistogramInfo>,
}

impl Histogram {
    /// Adds `c` into `self`: counts per bucket, sample count and sample sum. An empty `c`
    /// (no samples) is a no-op; buckets `c` has beyond `self`'s are appended.
    ///
    /// # Errors
    ///
    /// Returns an out-of-range error when a shared bucket has a different upper bound; buckets
    /// before the mismatch have already been added.
    pub fn add_assign(&mut self, c: &Histogram) -> Result<&mut Self, ExceptionPtr> {
        if c.sample_count == 0 {
            return Ok(self);
        }
        for (i, b) in c.buckets.iter().enumerate() {
            if self.buckets.len() <= i {
                self.buckets.push(*b);
            } else {
                if self.buckets[i].upper_bound != b.upper_bound {
                    return Err(ExceptionPtr::new(crate::engine::core::future::OutOfRange::new(
                        "Trying to add histogram with different bucket limits".to_string(),
                    )));
                }
                self.buckets[i].count += b.count;
            }
        }
        self.sample_count += c.sample_count;
        self.sample_sum += c.sample_sum;
        Ok(self)
    }

    /// Returns the sum of `self` and `c` as a new histogram.
    ///
    /// # Errors
    ///
    /// As [`add_assign`](Self::add_assign), when the bucket bounds differ.
    pub fn add(&self, c: &Histogram) -> Result<Histogram, ExceptionPtr> {
        let mut res = self.clone();
        res.add_assign(c)?;
        Ok(res)
    }

    /// Returns the sum of `self` and `c`, reusing `c`'s storage for the result.
    ///
    /// # Errors
    ///
    /// As [`add_assign`](Self::add_assign), when the bucket bounds differ.
    pub fn add_owned(&self, mut c: Histogram) -> Result<Histogram, ExceptionPtr> {
        c.add_assign(self)?;
        Ok(c)
    }
}
