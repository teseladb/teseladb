//! Shared enums: durability mode.

/// Durability mode of a write (constitution, principle IV).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
#[repr(u8)]
pub enum Durability {
    /// Persisted on disk (and, in the cluster, on a majority) before acknowledging.
    #[default]
    Strict = 0,
    /// In the RAM of a majority; on disk within ≤ 1 ms.
    Memory = 1,
    /// RAM only; lost on restart.
    None = 2,
}

impl Durability {
    /// Numeric code.
    pub const fn code(self) -> u8 {
        self as u8
    }

    /// Mode from its code.
    pub const fn from_code(c: u8) -> Option<Durability> {
        match c {
            0 => Some(Durability::Strict),
            1 => Some(Durability::Memory),
            2 => Some(Durability::None),
            _ => None,
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn durability_codes() {
        for d in [Durability::Strict, Durability::Memory, Durability::None] {
            assert_eq!(Durability::from_code(d.code()), Some(d));
        }
        assert_eq!(Durability::from_code(3), None);
        assert_eq!(Durability::default(), Durability::Strict);
    }
}
