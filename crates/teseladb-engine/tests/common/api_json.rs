//! The REST API description of the `rest_api_httpd` test server (`tests/data/api.json`) as
//! Rust types: the models and operations a JSON-to-code generator would emit for it.
//!
//! It holds one struct per model ([`MyObject`], whose members register themselves for
//! formatting), an enum wrapper per enum member ([`EnumVarWrapper`]), the API name
//! ([`NAME`]), one [`PathDescription`] per operation ([`hello_world`]) and, per enum query
//! parameter, an enum and its parser in a module named after the operation
//! (`ns_hello_world::QueryEnum`, `ns_hello_world::str2query_enum`).
//!
//! The members register in a `JsonBase` where the object is formatted, as in
//! `teseladb::engine::http::api_docs`, because a Rust value moves.

#![allow(dead_code)]

pub mod api_json {
    use teseladb::engine::core::future::{ExceptionPtr, Future, make_exception_future};
    use teseladb::engine::core::iostream::OutputStreamState;
    use teseladb::engine::core::sstring::Sstring;
    use teseladb::engine::http::common::httpd::OperationType;
    use teseladb::engine::http::json_path::{PathDescription, PathPart, UrlComponentType};
    use teseladb::engine::json::formatter::{JsonWrite, ToJson};
    use teseladb::engine::json::json_elements::{
        JsonBase, JsonBaseElement, JsonChunkedList, JsonElement, JsonList, Jsonable, json_base_add,
    };

    /// The values of the `enum_var` member of [`MyObject`]; `NumItems` marks the end.
    #[allow(non_camel_case_types, clippy::upper_case_acronyms)]
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
    #[repr(i32)]
    pub enum MyObjectEnumVar {
        #[default]
        VAL1,
        VAL2,
        VAL3,
        NUM_ITEMS,
    }

    impl MyObjectEnumVar {
        fn from_pos(p: i32) -> Self {
            match p {
                0 => MyObjectEnumVar::VAL1,
                1 => MyObjectEnumVar::VAL2,
                2 => MyObjectEnumVar::VAL3,
                _ => MyObjectEnumVar::NUM_ITEMS,
            }
        }
    }

    /// The enumerations an [`EnumVarWrapper`] converts from and to: those with the entries
    /// `VAL1`, `VAL2` and `VAL3`.
    pub trait EnumVarCompatible: Copy {
        /// The entries `VAL1`, `VAL2`, `VAL3` as 0, 1, 2; any other value as `None`.
        fn entry(self) -> Option<i32>;
        /// The entry `i` (0, 1 or 2) of the enumeration.
        fn from_entry(i: i32) -> Self;
    }

    /// An enum value held by its position, formatted as a JSON string with its name.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct EnumVarWrapper {
        pub v: MyObjectEnumVar,
    }

    impl EnumVarWrapper {
        /// Wraps the value `v` of any compatible enumeration.
        pub fn from_value<T: EnumVarCompatible>(v: T) -> Self {
            let v = match v.entry() {
                Some(0) => MyObjectEnumVar::VAL1,
                Some(1) => MyObjectEnumVar::VAL2,
                Some(2) => MyObjectEnumVar::VAL3,
                _ => MyObjectEnumVar::NUM_ITEMS,
            };
            EnumVarWrapper { v }
        }

        /// Converts back to the compatible enumeration `T`.
        pub fn to_value<T: EnumVarCompatible>(self) -> T {
            match self.v {
                MyObjectEnumVar::VAL1 => T::from_entry(0),
                MyObjectEnumVar::VAL2 => T::from_entry(1),
                MyObjectEnumVar::VAL3 => T::from_entry(2),
                _ => T::from_entry(0),
            }
        }

        /// Advances to the next position.
        pub fn increment(&mut self) -> &mut Self {
            self.v = MyObjectEnumVar::from_pos(self.v as i32 + 1);
            self
        }

        /// Whether this position is at or before `c`.
        pub fn le(&self, c: &EnumVarWrapper) -> bool {
            (self.v as i32) <= (c.v as i32)
        }

        /// The signed distance from `c` to this position.
        pub fn sub(&self, c: &EnumVarWrapper) -> i32 {
            self.v as i32 - c.v as i32
        }

        /// The first position.
        pub fn begin() -> Self {
            EnumVarWrapper { v: MyObjectEnumVar::VAL1 }
        }

        /// The position past the last entry.
        pub fn end() -> Self {
            EnumVarWrapper { v: MyObjectEnumVar::NUM_ITEMS }
        }

        /// Every position from [`EnumVarWrapper::begin`] to [`EnumVarWrapper::end`].
        pub fn all_items() -> impl Iterator<Item = EnumVarWrapper> {
            let end = Self::end();
            let mut cur = Self::begin();
            std::iter::from_fn(move || {
                if cur == end {
                    return None;
                }
                let r = cur;
                cur.increment();
                Some(r)
            })
        }
    }

    impl Jsonable for EnumVarWrapper {
        /// The quoted name of the entry.
        fn to_json(&self) -> Result<Sstring, ExceptionPtr> {
            Ok(Sstring::from(match self.v {
                MyObjectEnumVar::VAL1 => "\"VAL1\"",
                MyObjectEnumVar::VAL2 => "\"VAL2\"",
                MyObjectEnumVar::VAL3 => "\"VAL3\"",
                _ => "\"Unknown\"",
            }))
        }
    }

    impl ToJson for EnumVarWrapper {
        fn to_json(&self) -> Result<Sstring, ExceptionPtr> {
            Jsonable::to_json(self)
        }
    }

    impl JsonWrite for EnumVarWrapper {
        fn write_json(self, s: &mut OutputStreamState<u8>) -> Future<()> {
            match Jsonable::to_json(&self) {
                Ok(j) => s.write_sstring(&j),
                Err(e) => make_exception_future(e),
            }
        }
    }

    impl<T: EnumVarCompatible> From<T> for EnumVarWrapper {
        fn from(v: T) -> Self {
            EnumVarWrapper::from_value(v)
        }
    }

    /// Demonstrate an object
    #[derive(Clone, Default)]
    pub struct MyObject {
        /// The first parameter in the path
        pub var1: JsonElement<Sstring>,

        /// The second parameter in the path
        pub var2: JsonElement<Sstring>,

        /// Demonstrate an enum returned, note this is not the same enum type of the request
        pub enum_var: JsonElement<EnumVarWrapper>,

        pub array_var: JsonList<i32>,

        pub chunked_array_var: JsonChunkedList<i32>,
    }

    impl MyObject {
        /// Registers each member with its name and mandatory flag (`JsonBase::element` lists
        /// them in this order).
        fn register_params(&mut self) {
            json_base_add(&mut self.var1, "var1", false);
            json_base_add(&mut self.var2, "var2", false);
            json_base_add(&mut self.enum_var, "enum_var", false);
            json_base_add(&mut self.array_var, "array_var", false);
            json_base_add(&mut self.chunked_array_var, "chunked_array_var", false);
        }

        /// An object with every member unset and registered.
        pub fn new() -> Self {
            let mut o = Self::default();
            o.register_params();
            o
        }

        /// Copies the members of `e` into this object.
        pub fn assign_from(&mut self, e: &MyObject) -> &mut Self {
            self.var1 = e.var1.clone();
            self.var2 = e.var2.clone();
            self.enum_var = e.enum_var.clone();
            self.array_var = e.array_var.clone();
            self.chunked_array_var = e.chunked_array_var.clone();
            self
        }

        /// Copies the members of this object into `e`.
        pub fn update(&self, e: &mut MyObject) -> &Self {
            e.var1 = self.var1.clone();
            e.var2 = self.var2.clone();
            e.enum_var = self.enum_var.clone();
            e.array_var = self.array_var.clone();
            e.chunked_array_var = self.chunked_array_var.clone();
            self
        }
    }

    impl JsonBase for MyObject {
        fn element_count(&self) -> usize {
            5
        }

        fn element(&self, i: usize) -> Option<&dyn JsonBaseElement> {
            match i {
                0 => Some(&self.var1),
                1 => Some(&self.var2),
                2 => Some(&self.enum_var),
                3 => Some(&self.array_var),
                4 => Some(&self.chunked_array_var),
                _ => None,
            }
        }
    }

    impl Jsonable for MyObject {
        fn to_json(&self) -> Result<Sstring, ExceptionPtr> {
            self.base_to_json()
        }
    }

    impl ToJson for MyObject {
        fn to_json(&self) -> Result<Sstring, ExceptionPtr> {
            Jsonable::to_json(self)
        }
    }

    impl JsonWrite for MyObject {
        fn write_json(self, s: &mut OutputStreamState<u8>) -> Future<()> {
            // The object moves into the write, which keeps it until it resolves.
            self.base_write(s)
        }
    }

    /// The name of the API.
    pub const NAME: &str = "api";

    thread_local! {
        /// Returns the number of seconds since the system was booted
        static HELLO_WORLD: &'static PathDescription = Box::leak(Box::new(PathDescription::with_parts(
            &Sstring::from("/hello/world"),
            OperationType::Get,
            &Sstring::from("hello_world"),
            &[
                PathPart { name: Sstring::from("var1"), type_: UrlComponentType::Param },
                PathPart { name: Sstring::from("var2"), type_: UrlComponentType::ParamUntilEndOfPath },
            ],
            &[Sstring::from("query_enum"), Sstring::from("use_streaming")],
        )));
    }

    /// The description of the `hello_world` operation: one per shard, because its
    /// rule cookie is a `Cell`.
    pub fn hello_world() -> &'static PathDescription {
        HELLO_WORLD.with(|p| *p)
    }

    pub mod ns_hello_world {
        use teseladb::engine::core::sstring::Sstring;

        use super::EnumVarCompatible;

        /// The values of the `query_enum` query parameter; `NumItems` marks the end.
        #[allow(non_camel_case_types, clippy::upper_case_acronyms)]
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        #[repr(i32)]
        pub enum QueryEnum {
            VAL1,
            VAL2,
            VAL3,
            NUM_ITEMS,
        }

        impl EnumVarCompatible for QueryEnum {
            fn entry(self) -> Option<i32> {
                match self {
                    QueryEnum::VAL1 => Some(0),
                    QueryEnum::VAL2 => Some(1),
                    QueryEnum::VAL3 => Some(2),
                    QueryEnum::NUM_ITEMS => None,
                }
            }
            fn from_entry(i: i32) -> Self {
                match i {
                    0 => QueryEnum::VAL1,
                    1 => QueryEnum::VAL2,
                    2 => QueryEnum::VAL3,
                    _ => QueryEnum::NUM_ITEMS,
                }
            }
        }

        /// Parses a `query_enum` parameter; an unknown name gives `NumItems`.
        pub fn str2query_enum(str: &Sstring) -> QueryEnum {
            const ARR: [&str; 3] = ["VAL1", "VAL2", "VAL3"];
            let mut i = 0;
            while i < 3 {
                if ARR[i] == str.as_str() {
                    return QueryEnum::from_entry(i as i32);
                }
                i += 1;
            }
            QueryEnum::from_entry(i as i32)
        }
    }
}
