//! Call-site locations.
//!
//! [`internal::SourceLocation`] records the file, function, line and column of a call
//! site. Logging and error reporting take one so that messages point at the caller. Use
//! [`source_location_current!`](crate::source_location_current) to capture the function
//! name as well; [`internal::SourceLocation::current`] uses `#[track_caller]` and leaves
//! the function name empty.

/// The location type; not part of the stable API path (re-exported elsewhere).
pub mod internal {
    /// A source location: file, function, line and column of a call site.
    #[derive(Clone, Copy, Debug)]
    pub struct SourceLocation {
        _file: &'static str,
        _func: &'static str,
        _line: u32,
        _col: u32,
    }

    impl SourceLocation {
        const fn with(file: &'static str, func: &'static str, line: u32, col: u32) -> Self {
            SourceLocation { _file: file, _func: func, _line: line, _col: col }
        }

        /// Builds a location from explicit parts. Used by the expansion of
        /// [`source_location_current!`](crate::source_location_current), which supplies
        /// the caller's file, function, line and column.
        #[doc(hidden)]
        #[inline]
        pub const fn current_at(
            file: &'static str,
            func: &'static str,
            line: u32,
            col: u32,
        ) -> Self {
            Self::with(file, func, line, col)
        }

        /// The unknown location: file and function `"unknown"`, line and column 0.
        pub const fn new() -> Self {
            SourceLocation { _file: "unknown", _func: "unknown", _line: 0, _col: 0 }
        }

        /// The location of the caller (via `#[track_caller]`). The function name is left
        /// empty and the column is 0; use
        /// [`source_location_current!`](crate::source_location_current) to get the
        /// function name.
        #[track_caller]
        #[inline]
        pub fn current() -> Self {
            let l = std::panic::Location::caller();
            Self::with(l.file(), "", l.line(), 0)
        }

        /// The file name.
        #[inline]
        pub const fn file_name(&self) -> &'static str {
            self._file
        }
        /// The function name.
        #[inline]
        pub const fn function_name(&self) -> &'static str {
            self._func
        }
        /// The line.
        #[inline]
        pub const fn line(&self) -> u32 {
            self._line
        }
        /// The column.
        #[inline]
        pub const fn column(&self) -> u32 {
            self._col
        }
    }

    impl Default for SourceLocation {
        fn default() -> Self {
            Self::new()
        }
    }
}

/// Captures the location where the macro is written as a
/// [`SourceLocation`](crate::engine::util::source_location_compat::internal::SourceLocation).
///
/// `function_name()` is the full path of the enclosing function
/// (`crate::module::function`); the column is 0. Functions that want their caller's
/// location take a `SourceLocation` parameter and callers pass this macro.
#[macro_export]
macro_rules! source_location_current {
    () => {{
        fn __here() {}
        fn __type_name_of<T>(_: T) -> &'static str {
            ::std::any::type_name::<T>()
        }
        let name = __type_name_of(__here);
        let name = match name.strip_suffix("::__here") {
            Some(n) => n,
            None => name,
        };
        $crate::engine::util::source_location_compat::internal::SourceLocation::current_at(
            file!(),
            name,
            line!(),
            0,
        )
    }};
}
