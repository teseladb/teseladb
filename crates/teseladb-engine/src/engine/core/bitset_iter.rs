//! Fixed-size bitsets of up to 64 bits and iteration over their set bits.
//!
//! The reactor and the scheduler keep small sets (pending queues, active scheduling groups) as
//! bit masks; [`bitsets::for_each_set`] walks the set indices in increasing order using one
//! trailing-zero count per bit, without scanning the clear bits.

/// Bitsets stored in a `u64` and iteration over their set bits.
pub mod bitsets {
    /// Number of bits in the word that stores a [`Bitset`] (64).
    pub const ULONG_BITS: i32 = u64::BITS as i32;

    /// Leading and trailing zero counts of the 64-bit integer types.
    pub trait BitCount: Copy {
        /// Returns the number of leading zeros in the value's binary representation, counting
        /// only value bits (the sign bit of a signed type is excluded).
        ///
        /// The result is meaningless for zero and for negative signed values. The largest
        /// result is the number of value bits minus one, returned for 1.
        fn count_leading_zeros(self) -> usize;
        /// Returns the number of trailing zeros in the value's binary representation.
        ///
        /// The result is meaningless for zero and for negative signed values. The largest
        /// meaningful result is the number of value bits minus one.
        fn count_trailing_zeros(self) -> usize;
    }

    impl BitCount for u64 {
        #[inline]
        fn count_leading_zeros(self) -> usize {
            self.leading_zeros() as usize
        }
        #[inline]
        fn count_trailing_zeros(self) -> usize {
            self.trailing_zeros() as usize
        }
    }

    impl BitCount for i64 {
        #[inline]
        fn count_leading_zeros(self) -> usize {
            (self as u64).leading_zeros() as usize - 1
        }
        #[inline]
        fn count_trailing_zeros(self) -> usize {
            (self as u64).trailing_zeros() as usize
        }
    }

    /// Leading zeros of `value`; see [`BitCount::count_leading_zeros`].
    #[inline]
    pub fn count_leading_zeros<T: BitCount>(value: T) -> usize {
        value.count_leading_zeros()
    }

    /// Trailing zeros of `value`; see [`BitCount::count_trailing_zeros`].
    #[inline]
    pub fn count_trailing_zeros<T: BitCount>(value: T) -> usize {
        value.count_trailing_zeros()
    }

    /// A set of `N` bits (`N <= 64`, checked at compile time when iterated), stored in a `u64`;
    /// bit `i` is `1 << i`.
    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
    pub struct Bitset<const N: usize>(pub u64);

    impl<const N: usize> Bitset<N> {
        const _CHECK: () = assert!(N <= ULONG_BITS as usize, "bitset too large");

        /// Whether no bit is set.
        #[inline]
        pub fn none(&self) -> bool {
            self.0 == 0
        }
        /// Whether at least one bit is set.
        #[inline]
        pub fn any(&self) -> bool {
            self.0 != 0
        }
        /// The bits as a `u64`.
        #[inline]
        pub fn to_ulong(&self) -> u64 {
            self.0
        }
        /// Whether bit `i` is set (`i < 64`).
        #[inline]
        pub fn test(&self, i: usize) -> bool {
            (self.0 >> i) & 1 != 0
        }
        /// Sets bit `i` (`i < 64`).
        #[inline]
        pub fn set(&mut self, i: usize) {
            self.0 |= 1u64 << i;
        }
        /// Clears bit `i` (`i < 64`).
        #[inline]
        pub fn reset(&mut self, i: usize) {
            self.0 &= !(1u64 << i);
        }
        /// Shifts the bits right by `shift`; a shift of 64 or more clears the set.
        #[inline]
        fn shr_assign(&mut self, shift: usize) {
            self.0 = if shift >= 64 { 0 } else { self.0 >> shift };
        }
    }

    /// Returns the index of the lowest set bit. The result is meaningless when no bit is set.
    #[inline]
    pub fn get_first_set<const N: usize>(bitset: &Bitset<N>) -> usize {
        let () = Bitset::<N>::_CHECK;
        count_trailing_zeros(bitset.to_ulong())
    }

    /// Returns the index of the highest set bit. The result is meaningless when no bit is set.
    #[inline]
    pub fn get_last_set<const N: usize>(bitset: &Bitset<N>) -> usize {
        let () = Bitset::<N>::_CHECK;
        ULONG_BITS as usize - 1 - count_leading_zeros(bitset.to_ulong())
    }

    /// Iterator over the indices of the set bits, in increasing order; its position is -1 once
    /// exhausted.
    #[derive(Debug, Clone, Copy)]
    pub struct SetIterator<const N: usize> {
        _bitset: Bitset<N>,
        _index: i32,
    }

    impl<const N: usize> SetIterator<N> {
        #[inline]
        fn advance(&mut self) {
            if self._bitset.none() {
                self._index = -1;
            } else {
                let shift = get_first_set(&self._bitset) + 1;
                self._index += shift as i32;
                self._bitset.shr_assign(shift);
            }
        }

        /// Positions the iterator at the first set bit at or after `offset`.
        #[inline]
        pub fn new(bitset: Bitset<N>, offset: i32) -> Self {
            let () = Bitset::<N>::_CHECK; // One shift and count per set bit; only small bitsets are supported
            let mut it = Self { _bitset: bitset, _index: offset - 1 };
            it._bitset.shr_assign(offset as usize);
            it.advance();
            it
        }

        /// The current index (-1 at the end).
        #[inline]
        pub fn get(&self) -> i32 {
            self._index
        }
    }

    impl<const N: usize> PartialEq for SetIterator<N> {
        #[inline]
        fn eq(&self, other: &Self) -> bool {
            self._index == other._index
        }
    }

    impl<const N: usize> Iterator for SetIterator<N> {
        type Item = i32;

        #[inline]
        fn next(&mut self) -> Option<i32> {
            if self._index == -1 {
                return None;
            }
            let ret = self._index;
            self.advance();
            Some(ret)
        }
    }

    /// The set bits of a bitset, as a range.
    #[derive(Debug, Clone, Copy)]
    pub struct SetRange<const N: usize> {
        _bitset: Bitset<N>,
        _offset: i32,
    }

    impl<const N: usize> SetRange<N> {
        /// The set bits of `bitset` at or after `offset`.
        #[inline]
        pub const fn new(bitset: Bitset<N>, offset: i32) -> Self {
            Self { _bitset: bitset, _offset: offset }
        }

        /// An iterator positioned at the first set bit at or after the offset.
        #[inline]
        pub fn begin(&self) -> SetIterator<N> {
            SetIterator::new(self._bitset, self._offset)
        }

        /// The exhausted iterator, equal to any iterator that has passed the last set bit.
        #[inline]
        pub fn end(&self) -> SetIterator<N> {
            SetIterator::new(Bitset(0), 0)
        }
    }

    impl<const N: usize> IntoIterator for SetRange<N> {
        type Item = i32;
        type IntoIter = SetIterator<N>;

        #[inline]
        fn into_iter(self) -> SetIterator<N> {
            self.begin()
        }
    }

    /// The set bits of `bitset` at or after `offset`.
    #[inline]
    pub fn for_each_set<const N: usize>(bitset: Bitset<N>, offset: i32) -> SetRange<N> {
        SetRange::new(bitset, offset)
    }
}
