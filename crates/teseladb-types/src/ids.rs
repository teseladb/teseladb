//! Numeric identifiers with their own type so they cannot be mixed up.

use core::fmt;

macro_rules! id_type {
    ($(#[$doc:meta])* $name:ident, $prefix:literal) => {
        $(#[$doc])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
        pub struct $name(pub u64);

        impl $name {
            /// Numeric value.
            #[inline]
            pub const fn get(self) -> u64 {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!($prefix, "{}"), self.0)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(self, f)
            }
        }

        impl From<u64> for $name {
            #[inline]
            fn from(v: u64) -> Self {
                Self(v)
            }
        }
    };
}

id_type!(
    /// Partition: placement unit of objects and ref ranges.
    PartitionId,
    "p"
);
id_type!(
    /// Consensus group (Group 0 or a ref range).
    GroupId,
    "g"
);
id_type!(
    /// Cluster node (0 = none).
    NodeId,
    "n"
);
id_type!(
    /// Repository.
    RepoId,
    "r"
);

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn display_debug_and_conversions() {
        assert_eq!(PartitionId(7).to_string(), "p7");
        assert_eq!(format!("{:?}", GroupId(3)), "g3");
        assert_eq!(NodeId::from(9).get(), 9);
        assert_eq!(RepoId::default(), RepoId(0));
        assert!(GroupId(1) < GroupId(2));
    }
}
