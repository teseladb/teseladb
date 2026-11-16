//! Type-safe booleans.
//!
//! [`BoolClass<Tag>`] is a boolean that does not convert implicitly to `bool`, to integers
//! or to a `BoolClass` with another tag, so a flag parameter cannot be passed in the wrong
//! position. It still supports `|`, `&`, `!` and equality.

use std::fmt;
use std::marker::PhantomData;

/// A boolean distinguished at the type level by `Tag`.
///
/// Values are [`BoolClass::YES`], [`BoolClass::NO`] or [`BoolClass::new`]; they convert to
/// `bool` only explicitly ([`BoolClass::as_bool`] or `bool::from`). Flags with different
/// tags are different types:
///
/// ```text
/// struct FooTag;
/// type Foo = BoolClass<FooTag>;
/// struct BarTag;
/// type Bar = BoolClass<BarTag>;
///
/// let v1 = Foo::YES;            // OK
/// let v2: Bar = Foo::YES;       // error: mismatched types
/// let v4 = v1 | Foo::NO;        // OK
/// let v5 = Bar::YES & Bar::new(true); // OK
/// let v6: bool = v5;            // error: no implicit conversion
/// ```
pub struct BoolClass<Tag> {
    _value: bool,
    _tag: PhantomData<fn() -> Tag>,
}

impl<Tag> Clone for BoolClass<Tag> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<Tag> Copy for BoolClass<Tag> {}

impl<Tag> BoolClass<Tag> {
    /// The `true` value.
    pub const YES: BoolClass<Tag> = BoolClass { _value: true, _tag: PhantomData };
    /// The `false` value.
    pub const NO: BoolClass<Tag> = BoolClass { _value: false, _tag: PhantomData };

    /// Creates a value equal to `v`.
    pub const fn new(v: bool) -> Self {
        BoolClass { _value: v, _tag: PhantomData }
    }

    /// Converts to an untyped `bool`.
    pub const fn as_bool(self) -> bool {
        self._value
    }

    /// Logical OR.
    pub const fn or(self, y: Self) -> Self {
        Self::new(self._value || y._value)
    }

    /// Logical AND.
    pub const fn and(self, y: Self) -> Self {
        Self::new(self._value && y._value)
    }
}

/// The default value is `false`.
impl<Tag> Default for BoolClass<Tag> {
    fn default() -> Self {
        Self::new(false)
    }
}

impl<Tag> From<BoolClass<Tag>> for bool {
    fn from(v: BoolClass<Tag>) -> bool {
        v._value
    }
}

impl<Tag> std::ops::BitOr for BoolClass<Tag> {
    type Output = Self;
    fn bitor(self, y: Self) -> Self {
        self.or(y)
    }
}

impl<Tag> std::ops::BitAnd for BoolClass<Tag> {
    type Output = Self;
    fn bitand(self, y: Self) -> Self {
        self.and(y)
    }
}

/// Logical NOT.
impl<Tag> std::ops::Not for BoolClass<Tag> {
    type Output = Self;
    fn not(self) -> Self {
        Self::new(!self._value)
    }
}

/// Equality of the boolean values.
impl<Tag> PartialEq for BoolClass<Tag> {
    fn eq(&self, y: &Self) -> bool {
        self._value == y._value
    }
}
impl<Tag> Eq for BoolClass<Tag> {}

/// Prints `true` or `false`.
impl<Tag> fmt::Display for BoolClass<Tag> {
    fn fmt(&self, os: &mut fmt::Formatter<'_>) -> fmt::Result {
        os.write_str(if self._value { "true" } else { "false" })
    }
}

impl<Tag> fmt::Debug for BoolClass<Tag> {
    fn fmt(&self, os: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, os)
    }
}
