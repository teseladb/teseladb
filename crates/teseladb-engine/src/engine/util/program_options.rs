//! Self-describing configuration options.
//!
//! Engine modules expose option structs built from an [`OptionGroup`] and typed values
//! ([`Value`], [`Switch`], [`SelectionValue`]), each with a name, a description and an
//! optional default. The tree of groups and values can be walked with
//! [`OptionGroup::describe`] (to build the command-line description) and
//! [`OptionGroup::mutate`] (to store parsed values back), so the same options can be set
//! from code, the command line or a configuration file.
//!
//! Groups and values are linked intrusively: each one embeds an auto-unlinking list hook,
//! a group holds the intrusive lists of its values and subgroups, and a value points at
//! its group. Rust moves run no code, so construction has two phases: the constructors
//! build unlinked objects, which move freely (the lists are empty); once the tree is at its
//! final place, `link` ([`OptionGroup::link`], [`OptionGroup::add_value`], or
//! [`link_option_group!`](crate::link_option_group) for a struct of options) links it
//! through `Pin`, which keeps it there.

use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::pin::Pin;
use std::ptr;

use crate::engine::core::api::intrusive::{List, ListHook};
use crate::engine::core::future::ExceptionPtr;
use crate::engine::core::sstring::Sstring;
use crate::engine::util::log::{LogLevel, LoggerOstreamType, LoggerTimestampStyle};
use crate::engine::util::memory_diagnostics::memory::AllocFailureKind;
use crate::intrusive_adapter;

/// The command-line parsing types (options description, parsed variables, validation
/// errors and validator helpers), defined with the command-line glue in the core
/// program-options module.
pub use crate::engine::core::program_options::bpo::{
    OptionsDescription, ValidationError, VariablesMap, validators,
};

//
// Infrastructure for configuring an engine application
//
// The program-options infrastructure allows configuring the engine both by
// code and by command-line and/or config files. This is achieved by
// providing a set of self-describing and self-validating value types as well
// as option groups to allow grouping them into arbitrary tree structures.
// Engine modules expose statically declared option structs, which embed an
// `OptionGroup` and contain various concrete `BasicValue` members
// comprising the required configuration. These structs are self-describing, and
// self-validating, the name of the option group as well as the list of its
// `BasicValue` member can be queried run-time.

///
/// Wrapper for command-line options with arbitrary string associations.
///
/// An option of this type stores an arbitrary number of string associations.
///
/// Values are specified in the form "key0=value0:[key1=value1:...]". Options of this type can be specified multiple
/// times, and the values will be merged (with the last-provided value for a key taking precedence).
///
/// It is a distinct type (rather than an alias of the map) so that its parsing and
/// printing can be implemented for it.
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct StringMap {
    base: HashMap<Sstring, Sstring>,
}

impl StringMap {
    /// An empty map.
    pub fn new() -> Self {
        StringMap { base: HashMap::new() }
    }
    /// The value of `k`, if present.
    pub fn at(&self, k: &Sstring) -> Option<&Sstring> {
        self.base.get(k)
    }
    /// The value of `k`, if present.
    pub fn find(&self, k: &Sstring) -> Option<&Sstring> {
        self.base.get(k)
    }
    /// 1 if `k` is present, else 0.
    pub fn count(&self, k: &Sstring) -> usize {
        self.base.contains_key(k) as usize
    }
    /// Inserts `k = v` unless `k` is present; returns whether it inserted.
    pub fn emplace(&mut self, k: Sstring, v: Sstring) -> bool {
        if self.base.contains_key(&k) {
            return false;
        }
        self.base.insert(k, v);
        true
    }
    /// Removes every association.
    pub fn clear(&mut self) {
        self.base.clear();
    }
    /// Returns the value of `k`, inserting an empty one if absent.
    pub fn get_or_insert(&mut self, k: Sstring) -> &mut Sstring {
        self.base.entry(k).or_default()
    }
    /// Iterates over the associations, in no particular order.
    pub fn iter(&self) -> std::collections::hash_map::Iter<'_, Sstring, Sstring> {
        self.base.iter()
    }
}

impl FromIterator<(Sstring, Sstring)> for StringMap {
    fn from_iter<I: IntoIterator<Item = (Sstring, Sstring)>>(iter: I) -> Self {
        StringMap { base: iter.into_iter().collect() }
    }
}

///
/// Query the value of a key in a `string_map`, or a default value if the key doesn't exist.
///
pub fn get_or_default(ss: &StringMap, key: &Sstring, def: &Sstring) -> Sstring {
    if let Some(v) = ss.find(key) {
        return v.clone();
    }
    def.clone()
}

/// Prints the associations as `key0=value0:key1=value1...`.
impl fmt::Display for StringMap {
    fn fmt(&self, os: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (n, (k, v)) in self.iter().enumerate() {
            if n > 0 {
                os.write_str(":")?;
            }
            write!(os, "{}={}", k, v)?;
        }
        Ok(())
    }
}

/// Parses the first whitespace-separated word of `is` as `key0=value0[:key1=value1...]`
/// and merges the associations into `ss`.
///
/// # Errors
///
/// Fails for an association without `=`.
pub fn read_string_map(is: &str, ss: &mut StringMap) -> Result<(), ValidationError> {
    let str = is.split_whitespace().next().unwrap_or("");
    crate::engine::util::log::log_cli::parse_map_associations(str, |k, v| {
        *ss.get_or_insert(Sstring::from(k)) = Sstring::from(v);
    })
}

/// Validator of [`StringMap`] options: merges every occurrence into `out`.
///
/// # Errors
///
/// Fails for an association without `=`.
pub fn validate_string_map(
    out: &mut Option<StringMap>,
    in_: &[String],
) -> Result<(), ValidationError> {
    if out.is_none() {
        *out = Some(StringMap::new());
    }
    let ss = out.as_mut().expect("set above");
    for s in in_ {
        crate::engine::util::log::log_cli::parse_map_associations(s, |k, v| {
            *ss.get_or_insert(Sstring::from(k)) = Sstring::from(v);
        })?;
    }
    Ok(())
}

/// Visitor interface for `OptionGroup::describe`.
///
/// See `OptionGroup::describe` for more details on the visiting algorithm.
pub trait OptionsDescriptor {
    /// Visit the start of the group.
    ///
    /// Called when entering a group. Groups can be nested, in which case there
    /// will be another call to this method, before the current groups is closed.
    /// Returns whether visitor is interested in the group: `true` - visit,
    /// `false` - skip.
    fn visit_group_start(&mut self, name: &str, used: bool) -> bool;

    /// Visit the end of the group.
    ///
    /// Called after all values and nested groups were visited in the current
    /// group.
    fn visit_group_end(&mut self);

    /// Visit value metadata, common across all value types.
    ///
    /// Called at the start of visiting a value. After this, a call to the
    /// appropriate `visit_value` overload (or `visit_selection_value`)
    /// follows.
    /// Returns whether visitor is interested in the value: `true` - visit,
    /// `false` - skip.
    fn visit_value_metadata(&mut self, name: &str, description: &str, used: bool) -> bool;

    /// Visit a switch ([`Switch`]).
    fn visit_value_switch(&mut self);
    /// Visit a value (`Value`), `default_val` is null when value has no default.
    fn visit_value_bool(&mut self, default_val: Option<&bool>);
    /// Visit a value (`Value`), `default_val` is null when value has no default.
    fn visit_value_int(&mut self, default_val: Option<&i32>);
    /// Visit a value (`Value`), `default_val` is null when value has no default.
    fn visit_value_unsigned(&mut self, default_val: Option<&u32>);
    /// Visit a value (`Value`), `default_val` is null when value has no default.
    fn visit_value_float(&mut self, default_val: Option<&f32>);
    /// Visit a value (`Value`), `default_val` is null when value has no default.
    fn visit_value_double(&mut self, default_val: Option<&f64>);
    /// Visit a value (`Value`), `default_val` is null when value has no default.
    fn visit_value_string(&mut self, default_val: Option<&String>);
    /// Visit a value (`Value`), `default_val` is null when value has no default.
    fn visit_value_unsigned_set(&mut self, default_val: Option<&BTreeSet<u32>>);
    /// Visit a value (`Value`), `default_val` is null when value has no default.
    fn visit_value_log_level(&mut self, default_val: Option<&LogLevel>);
    /// Visit a value (`Value`), `default_val` is null when value has no default.
    fn visit_value_logger_timestamp_style(&mut self, default_val: Option<&LoggerTimestampStyle>);
    /// Visit a value (`Value`), `default_val` is null when value has no default.
    fn visit_value_logger_ostream_type(&mut self, default_val: Option<&LoggerOstreamType>);
    /// Visit a value (`Value`), `default_val` is null when value has no default.
    fn visit_value_alloc_failure_kind(&mut self, default_val: Option<&AllocFailureKind>);
    /// Visit a value (`Value`), `default_val` is null when value has no default.
    fn visit_value_log_level_map(&mut self, default_val: Option<&HashMap<Sstring, LogLevel>>);

    /// Visit a selection value (`SelectionValue`), `default_candidate` is null when there is no default candidate.
    fn visit_selection_value(
        &mut self,
        candidate_names: &[String],
        default_candidate: Option<&usize>,
    );
}

/// Visitor interface `OptionGroup::mutate`.
///
/// See `OptionGroup::mutate` for more details on the visiting algorithm.
pub trait OptionsMutator {
    /// Visit the start of the group.
    ///
    /// Called when entering a group. Groups can be nested, in which case there
    /// will be another call to this method, before the current groups is closed.
    /// Returns whether visitor is interested in the group: `true` - visit,
    /// `false` - skip.
    fn visit_group_start(&mut self, name: &str, used: bool) -> bool;

    /// Visit the end of the group.
    ///
    /// Called after all values and nested groups were visited in the current
    /// group.
    fn visit_group_end(&mut self);

    /// Visit value metadata, common across all value types.
    ///
    /// Called at the start of visiting a value. After this, a call to the
    /// appropriate `visit_value` overload (or `visit_selection_value`)
    /// follows.
    /// Returns whether visitor is interested in the value: `true` - visit,
    /// `false` - skip.
    fn visit_value_metadata(&mut self, name: &str, used: bool) -> bool;

    /// Visit a switch ([`Switch`]), switch is set to returned value.
    fn visit_value_switch(&mut self) -> Result<bool, ExceptionPtr>;
    /// Visit and optionally mutate a value (`Value`), should return true if value was mutated.
    fn visit_value_bool(&mut self, val: &mut bool) -> Result<bool, ExceptionPtr>;
    /// Visit a value (`Value`), `default_val` is null when value has no default.
    fn visit_value_int(&mut self, val: &mut i32) -> Result<bool, ExceptionPtr>;
    /// Visit and optionally mutate a value (`Value`), should return true if value was mutated.
    fn visit_value_unsigned(&mut self, val: &mut u32) -> Result<bool, ExceptionPtr>;
    /// Visit and optionally mutate a value (`Value`), should return true if value was mutated.
    fn visit_value_float(&mut self, val: &mut f32) -> Result<bool, ExceptionPtr>;
    /// Visit and optionally mutate a value (`Value`), should return true if value was mutated.
    fn visit_value_double(&mut self, val: &mut f64) -> Result<bool, ExceptionPtr>;
    /// Visit and optionally mutate a value (`Value`), should return true if value was mutated.
    fn visit_value_string(&mut self, val: &mut String) -> Result<bool, ExceptionPtr>;
    /// Visit and optionally mutate a value (`Value`), should return true if value was mutated.
    fn visit_value_unsigned_set(&mut self, val: &mut BTreeSet<u32>) -> Result<bool, ExceptionPtr>;
    /// Visit and optionally mutate a value (`Value`), should return true if value was mutated.
    fn visit_value_log_level(&mut self, val: &mut LogLevel) -> Result<bool, ExceptionPtr>;
    /// Visit and optionally mutate a value (`Value`), should return true if value was mutated.
    fn visit_value_logger_timestamp_style(
        &mut self,
        val: &mut LoggerTimestampStyle,
    ) -> Result<bool, ExceptionPtr>;
    /// Visit and optionally mutate a value (`Value`), should return true if value was mutated.
    fn visit_value_logger_ostream_type(
        &mut self,
        val: &mut LoggerOstreamType,
    ) -> Result<bool, ExceptionPtr>;
    /// Visit and optionally mutate a value (`Value`), should return true if value was mutated.
    fn visit_value_alloc_failure_kind(
        &mut self,
        val: &mut AllocFailureKind,
    ) -> Result<bool, ExceptionPtr>;
    /// Visit and optionally mutate a value (`Value`), should return true if value was mutated.
    fn visit_value_log_level_map(
        &mut self,
        val: &mut HashMap<Sstring, LogLevel>,
    ) -> Result<bool, ExceptionPtr>;

    /// Visit and optionally mutate a selection value (`SelectionValue`), should return true if value was mutated.
    fn visit_selection_value(
        &mut self,
        candidate_names: &[String],
        selected_candidate: &mut usize,
    ) -> Result<bool, ExceptionPtr>;
}

/// The overload resolution of `visit_value()`: each value type picks its visitor method.
pub trait VisitableValue: Sized + Default {
    /// Calls the `descriptor` method for this type, with the default value if any.
    fn describe_value(descriptor: &mut dyn OptionsDescriptor, default_val: Option<&Self>);
    /// Calls the `mutator` method for this type; returns whether it changed `val`.
    ///
    /// # Errors
    ///
    /// Returns the error of the mutator.
    fn mutate_value(mutator: &mut dyn OptionsMutator, val: &mut Self)
    -> Result<bool, ExceptionPtr>;
}

macro_rules! visitable {
    ($t:ty, $d:ident, $m:ident) => {
        impl VisitableValue for $t {
            fn describe_value(descriptor: &mut dyn OptionsDescriptor, default_val: Option<&Self>) {
                descriptor.$d(default_val)
            }
            fn mutate_value(
                mutator: &mut dyn OptionsMutator,
                val: &mut Self,
            ) -> Result<bool, ExceptionPtr> {
                mutator.$m(val)
            }
        }
    };
}
visitable!(bool, visit_value_bool, visit_value_bool);
visitable!(i32, visit_value_int, visit_value_int);
visitable!(u32, visit_value_unsigned, visit_value_unsigned);
visitable!(f32, visit_value_float, visit_value_float);
visitable!(f64, visit_value_double, visit_value_double);
visitable!(String, visit_value_string, visit_value_string);
visitable!(BTreeSet<u32>, visit_value_unsigned_set, visit_value_unsigned_set);
visitable!(LogLevel, visit_value_log_level, visit_value_log_level);
visitable!(
    LoggerTimestampStyle,
    visit_value_logger_timestamp_style,
    visit_value_logger_timestamp_style
);
visitable!(LoggerOstreamType, visit_value_logger_ostream_type, visit_value_logger_ostream_type);
visitable!(AllocFailureKind, visit_value_alloc_failure_kind, visit_value_alloc_failure_kind);
visitable!(HashMap<Sstring, LogLevel>, visit_value_log_level_map, visit_value_log_level_map);

// `T val;` in do_mutate needs a default-constructed value of each type: LogLevel,
// LoggerTimestampStyle, LoggerOstreamType and AllocFailureKind derive Default (their first
// variant).

/// A tag type used to construct unused `OptionGroup` and `BasicValue` objects.
#[derive(Clone, Copy, Default)]
pub struct Unused;

/// The type-specific operations of a [`BasicValue`]: describe and mutate.
pub struct BasicValueVtable {
    /// Describes the concrete value to `descriptor`.
    pub do_describe: unsafe fn(*const BasicValue, &mut dyn OptionsDescriptor),
    /// Lets `mutator` change the concrete value.
    pub do_mutate: unsafe fn(*mut BasicValue, &mut dyn OptionsMutator) -> Result<(), ExceptionPtr>,
}

/// A basic configuration option value.
///
/// This serves as the common base-class of all the concrete value types, which are
/// `#[repr(C)]` and begin with it.
#[repr(C)]
pub struct BasicValue {
    _vtable: &'static BasicValueVtable,
    /// The auto-unlinking hook in the group's list.
    _link: ListHook,
    /// The group of the value (null until linked).
    pub _group: *mut OptionGroup,
    /// Whether the value is used.
    pub _used: bool,
    /// The name of the value.
    pub _name: String,
    /// The description of the value.
    pub _description: String,
}

intrusive_adapter!(pub ValueListAdapter = BasicValue { _link });
intrusive_adapter!(pub OptionGroupListAdapter = OptionGroup { _link });

/// The intrusive list of the values of a group.
pub type ValueListType = List<ValueListAdapter>;
/// The intrusive list of the subgroups of a group.
pub type OptionGroupListType = List<OptionGroupListAdapter>;

/// A group of options.
///
/// `OptionGroup` is the basis for organizing options. It can hold a number
/// of `BasicValue` objects. These are typically fields of the same struct:
///
/// ```text
/// struct MyOptionGroup {
///     group: OptionGroup,        // OptionGroup::new("My option group")
///     opt1: Switch,              // Switch::new("opt1", ...)
///     opt2: Value<bool>,         // Value::new("opt2", ...)
/// }
/// ```
///
/// built unlinked and then linked in place with `link_option_group!`.
///
/// Option groups can also be nested and using this property one can build a
/// tree of option groups and values. This tree then can be visited using the
/// two visitor methods exposed by [`OptionGroup`]:
/// * `describe`
/// * `mutate`
///
/// Using these two visitors one can easily implement glue code to expose an
/// entire options tree to the command line. Use `describe` to build the
/// command-line level description of the objects and after parsing the provided command-line options
/// use `mutate` to propagate the extracted values back into the options
/// tree. How this is done is entirely up to the visitors, the above methods
/// only offer an API to visit each group and value in the tree, they don't make
/// any assumption about how the visitor works and what its purpose is.
///
/// In Rust the struct holds an `OptionGroup` field and its values, built unlinked, and
/// is linked once in place with `link_option_group`.
pub struct OptionGroup {
    /// The auto-unlinking hook in the group's list.
    _link: ListHook,
    _parent: *mut OptionGroup,
    _used: bool,
    _name: String,
    _values: ValueListType,
    _subgroups: OptionGroupListType,
}

impl OptionGroup {
    #[inline]
    fn construct(name: String, used: bool) -> Self {
        OptionGroup {
            _link: ListHook::new(),
            _parent: ptr::null_mut(),
            _used: used,
            _name: name,
            _values: List::new(),
            _subgroups: List::new(),
        }
    }

    /// Construct an option group.
    ///
    /// - `name`: the name of the option group
    ///
    /// The parent group is given later, to [`OptionGroup::link`].
    pub fn new(name: impl Into<String>) -> Self {
        Self::construct(name.into(), true)
    }

    /// Construct an unused option group.
    ///
    /// - `name`: the name of the option group
    pub fn new_unused(name: impl Into<String>, _: Unused) -> Self {
        Self::construct(name.into(), false)
    }

    /// Links the group, at its final place, as a sub option group of `parent` (appended
    /// to its subgroups). Linking happens once: a group already linked stays where it is
    /// and a second call does nothing.
    pub fn link(self: Pin<&mut Self>, parent: Option<Pin<&mut OptionGroup>>) {
        // SAFETY: the group is not moved out of the pin.
        let me = unsafe { self.get_unchecked_mut() };
        if me._link.is_linked() {
            return;
        }
        if let Some(p) = parent {
            // SAFETY: as above, for the parent.
            let p = unsafe { p.get_unchecked_mut() };
            me._parent = p;
            // SAFETY: both groups are pinned: the parent's list and this hook stay in
            // place; the hook unlinks itself when the group is dropped.
            unsafe { p._subgroups.push_back(me) };
        }
    }

    /// Links `v`, at its final place, into this group (`_group->_values.push_back(*this)`
    /// of the `BasicValue` constructor). A value is in one group: a value already linked
    /// (here or in another group) stays where it is and the call does nothing.
    pub fn add_value<V: AsBasicValue + ?Sized>(self: Pin<&mut Self>, v: Pin<&mut V>) {
        // SAFETY: neither is moved out of its pin.
        let (me, v) = unsafe { (self.get_unchecked_mut(), v.get_unchecked_mut()) };
        // The list keeps a pointer derived from the whole value, not from its base field:
        // `do_describe` and `do_mutate` turn it back into the concrete value (`container_of`).
        // SAFETY: `v` is a live, exclusively borrowed value.
        let b = unsafe { V::base_ptr(v) };
        // SAFETY: `b` points to the base of `v`, reached through no other reference here.
        unsafe {
            if (*b)._link.is_linked() {
                return;
            }
            (*b)._group = me;
        }
        // SAFETY: both are pinned: the list and the hook stay in place; the hook unlinks
        // itself when the value is dropped, and the group unlinks its values when dropped.
        unsafe { me._values.push_back(b) };
    }

    /// Does the option group has any values contained in it?
    #[inline]
    pub fn as_bool(&self) -> bool {
        !self._values.is_empty()
    }

    /// Whether the group is used.
    #[inline]
    pub fn used(&self) -> bool {
        self._used
    }

    /// The name of the group.
    #[inline]
    pub fn name(&self) -> &str {
        &self._name
    }

    /// The values of the group.
    #[inline]
    pub fn values(&self) -> &ValueListType {
        &self._values
    }

    /// The values of the group.
    #[inline]
    pub fn values_mut(&mut self) -> &mut ValueListType {
        &mut self._values
    }

    /// Describe the content of this option group to the visitor.
    ///
    /// The content is visited in a depth-first manner:
    /// * First the option groups itself is visited with
    ///   `OptionsDescriptor::visit_group_start`. If this returns `false`
    ///   the entire content of the group, including all its subgroups and values
    ///   are skipped and `OptionsDescriptor::visit_group_end` is called
    ///   immediately. Otherwise visiting the content of the group proceeds.
    /// * All the values contained therein are visited. For each value the
    ///   following happens:
    ///     - First `OptionsDescriptor::visit_value_metadata` is called
    ///       with generic metadata that all values have. If this return
    ///       `false` the value is skipped, otherwise visiting the value
    ///       proceeds.
    ///     - Then the appropriate overload of
    ///       `OptionsDescriptor::visit_value` is called, with a pointer
    ///       to the default value of the respective value. The pointer is null
    ///       if there is no default value.
    ///     - For `SelectionValue`,
    ///       `OptionsDescriptor::visit_selection_value` will be called
    ///       instead of `OptionsDescriptor::visit_value`. After the value
    ///       is visited, the `OptionGroup` instance belonging to each
    ///       candidate (if set) will be visited.
    /// * All the nested `OptionGroup` instances in the current group are
    ///   visited.
    /// * Finally `OptionsDescriptor::visit_group_end` is called.
    pub fn describe(&self, descriptor: &mut dyn OptionsDescriptor) {
        if descriptor.visit_group_start(&self._name, self._used) {
            let mut value = self._values.front();
            while let Some(v) = value {
                // SAFETY: a linked value is alive (it unlinks itself when dropped), and the
                // list pointer carries the provenance of the whole value (add_value).
                unsafe {
                    BasicValue::describe(v.as_ptr(), descriptor);
                    value = self._values.next_of(v.as_ptr());
                }
            }
            let mut grp = self._subgroups.front();
            while let Some(g) = grp {
                // SAFETY: a linked subgroup is alive (it unlinks itself when dropped).
                unsafe {
                    (*g.as_ptr()).describe(descriptor);
                    grp = self._subgroups.next_of(g.as_ptr());
                }
            }
        }
        descriptor.visit_group_end();
    }

    /// Mutate the content of this option group by the visitor.
    ///
    /// The visiting algorithm is identical to that of `describe`, with the
    /// following differences:
    /// * `OptionsMutator::visit_value` is allowed to mutate the value
    ///   through the passed-in reference. It should return `true` if it did so
    ///   and `false` otherwise.
    /// * When visiting a selection value, only the nested group belonging to
    ///   the selected value is visited afterwards.
    ///
    /// # Errors
    ///
    /// An error of the mutator stops the visit immediately and is returned, without the
    /// call to `visit_group_end()`.
    pub fn mutate(&mut self, mutator: &mut dyn OptionsMutator) -> Result<(), ExceptionPtr> {
        if mutator.visit_group_start(&self._name, self._used) {
            let mut value = self._values.front();
            while let Some(v) = value {
                // SAFETY: as in describe(); the tree is reached through `&mut self` only.
                unsafe {
                    BasicValue::mutate(v.as_ptr(), mutator)?;
                    value = self._values.next_of(v.as_ptr());
                }
            }
            let mut grp = self._subgroups.front();
            while let Some(g) = grp {
                // SAFETY: as above.
                unsafe {
                    (*g.as_ptr()).mutate(mutator)?;
                    grp = self._subgroups.next_of(g.as_ptr());
                }
            }
        }
        mutator.visit_group_end();
        Ok(())
    }
}

impl Drop for OptionGroup {
    /// Unlinks the group's values and subgroups first: the group field of an option struct
    /// is dropped before the values, which would otherwise point at its dropped lists.
    fn drop(&mut self) {
        while let Some(v) = self._values.pop_front() {
            // SAFETY: a value linked here is alive (it unlinks itself when dropped); its
            // group pointer would dangle once this group is gone.
            unsafe { (*v.as_ptr())._group = ptr::null_mut() };
        }
        while let Some(g) = self._subgroups.pop_front() {
            // SAFETY: as above, for a linked subgroup and its parent pointer.
            unsafe { (*g.as_ptr())._parent = ptr::null_mut() };
        }
    }
}

/// The values that `OptionGroup::add_value` links: the concrete value types, which
/// begin with their `BasicValue`.
///
/// # Safety
/// `Self` is `#[repr(C)]` (or is `BasicValue`) and begins with its `BasicValue`, whose
/// vtable is the one of `Self`; `base_ptr` returns a pointer to that field derived from
/// `this` (`&raw mut (*this).base`), so it keeps the provenance of the whole value.
pub unsafe trait AsBasicValue {
    /// The base value, as a pointer with the provenance of the whole value.
    ///
    /// # Safety
    /// `this` points to a live `Self`.
    unsafe fn base_ptr(this: *mut Self) -> *mut BasicValue;
}

impl BasicValue {
    /// A value base with its vtable, `used` flag, name and description, unlinked; the
    /// group is given by [`OptionGroup::add_value`].
    #[inline]
    pub fn new(
        vtable: &'static BasicValueVtable,
        used: bool,
        name: String,
        description: String,
    ) -> Self {
        BasicValue {
            _vtable: vtable,
            _link: ListHook::new(),
            _group: ptr::null_mut(),
            _used: used,
            _name: name,
            _description: description,
        }
    }

    /// Whether the value is used.
    #[inline]
    pub fn used(&self) -> bool {
        self._used
    }

    /// The name of the value.
    #[inline]
    pub fn name(&self) -> &str {
        &self._name
    }

    /// The description of the value.
    #[inline]
    pub fn description(&self) -> &str {
        &self._description
    }

    /// Describes the value to `descriptor`.
    ///
    /// # Safety
    /// `this` points to a live value, with the provenance of the whole value that begins
    /// with this base (the pointer `OptionGroup::add_value` links), and no `&mut` to it
    /// is alive.
    pub unsafe fn describe(this: *const BasicValue, descriptor: &mut dyn OptionsDescriptor) {
        // SAFETY: guaranteed by the caller; the vtable is the one of the value this base begins.
        unsafe {
            if descriptor.visit_value_metadata(&(*this)._name, &(*this)._description, (*this)._used)
            {
                ((*this)._vtable.do_describe)(this, descriptor);
            }
        }
    }

    /// Lets `mutator` mutate the value.
    ///
    /// # Safety
    /// `this` must point to a live value, reached through no other reference, with the
    /// provenance of the whole value that begins with this base (as in `describe`).
    pub unsafe fn mutate(
        this: *mut BasicValue,
        mutator: &mut dyn OptionsMutator,
    ) -> Result<(), ExceptionPtr> {
        // SAFETY: guaranteed by the caller.
        unsafe {
            if mutator.visit_value_metadata(&(*this)._name, (*this)._used) {
                ((*this)._vtable.do_mutate)(this, mutator)?;
            }
        }
        Ok(())
    }
}

// SAFETY: a BasicValue is its own base.
unsafe impl AsBasicValue for BasicValue {
    #[inline]
    unsafe fn base_ptr(this: *mut Self) -> *mut BasicValue {
        this
    }
}

/// Links a struct of options in place: its group (to `parent`) and then each listed value
/// or nested option struct, in order.
///
/// `link_option_group!(this, parent, group, value1, [nested1], value2, ...)`: `this` is a
/// `Pin<&mut Struct>`, `parent` an `Option<Pin<&mut OptionGroup>>`, `group` the
/// `OptionGroup` field; each plain field is a value (linked with
/// `OptionGroup::add_value`) and each bracketed field a nested struct of options with
/// its own `link(self: Pin<&mut Self>, parent: Option<Pin<&mut OptionGroup>>)`.
#[macro_export]
macro_rules! link_option_group {
    (@one $this:ident, $group:ident, [$field:ident]) => {
        // SAFETY: pin projection: the fields of the pinned struct stay in place with it.
        unsafe { ::std::pin::Pin::new_unchecked(&mut $this.$field) }
            // SAFETY: `$group` points at the option group field of the same pinned struct, which stays in place with it; it is a different field from `$this.$field`, so the two references do not overlap.
            .link(Some(unsafe { ::std::pin::Pin::new_unchecked(&mut *$group) }));
    };
    (@one $this:ident, $group:ident, $field:ident) => {
        // SAFETY: as above.
        unsafe { ::std::pin::Pin::new_unchecked(&mut *$group) }
            // SAFETY: `$this.$field` is a field of the pinned struct, which stays in place with it; it is a different field from the group `$group` points at, so the two references do not overlap.
            .add_value(unsafe { ::std::pin::Pin::new_unchecked(&mut $this.$field) });
    };
    ($this:expr, $parent:expr, $group:ident $(, $item:tt)* $(,)?) => {{
        let pinned = $this;
        // SAFETY: pin projection: the fields of the pinned struct stay in place with it.
        let this = unsafe { ::std::pin::Pin::get_unchecked_mut(pinned) };
        let group: *mut $crate::engine::util::program_options::OptionGroup = &mut this.$group;
        // SAFETY: as above.
        unsafe { ::std::pin::Pin::new_unchecked(&mut *group) }.link($parent);
        $(
            $crate::link_option_group!(@one this, group, $item);
        )*
    }};
}

/// A configuration option value.
///
/// `T`: the type of the contained value.
#[repr(C)]
pub struct Value<T: VisitableValue + 'static> {
    base: BasicValue,
    _value: Option<T>,
    _defaulted: bool,
}

impl<T: VisitableValue + 'static> Value<T> {
    const VTABLE: &'static BasicValueVtable =
        &BasicValueVtable { do_describe: Self::do_describe, do_mutate: Self::do_mutate };

    unsafe fn do_describe(this: *const BasicValue, descriptor: &mut dyn OptionsDescriptor) {
        // SAFETY: this vtable is only installed in the base of a Value<T>.
        let me = unsafe { &*(this as *const Self) };
        T::describe_value(descriptor, me._value.as_ref());
    }

    unsafe fn do_mutate(
        this: *mut BasicValue,
        mutator: &mut dyn OptionsMutator,
    ) -> Result<(), ExceptionPtr> {
        // SAFETY: as in do_describe().
        let me = unsafe { &mut *(this as *mut Self) };
        let mut val = T::default();
        if T::mutate_value(mutator, &mut val)? {
            me._value = Some(val);
            me._defaulted = false;
        }
        Ok(())
    }

    #[inline]
    fn do_set_value(&mut self, value: T, defaulted: bool) {
        self._value = Some(value);
        self._defaulted = defaulted;
    }

    /// Construct a value.
    ///
    /// - `name`: the name of this value
    /// - `default_value`: the default value, can be unset
    /// - `description`: the description of the value
    ///
    /// It is linked to its group later, with [`OptionGroup::add_value`].
    pub fn new(
        name: impl Into<String>,
        default_value: Option<T>,
        description: impl Into<String>,
    ) -> Self {
        Value {
            base: BasicValue::new(Self::VTABLE, true, name.into(), description.into()),
            _value: default_value,
            _defaulted: true,
        }
    }

    /// Construct an unused value.
    pub fn new_unused(name: impl Into<String>, _: Unused) -> Self {
        Value {
            base: BasicValue::new(Self::VTABLE, false, name.into(), String::new()),
            _value: None,
            _defaulted: true,
        }
    }

    /// Is there a contained value?
    #[inline]
    pub fn as_bool(&self) -> bool {
        self._value.is_some()
    }

    /// Does this value still contain a default-value?
    #[inline]
    pub fn defaulted(&self) -> bool {
        self._defaulted
    }

    /// Returns the contained value; there must be one (see `as_bool`).
    ///
    /// # Panics
    /// When there is no value.
    #[inline]
    pub fn get_value(&self) -> &T {
        match &self._value {
            Some(v) => v,
            None => panic!("option {} has no value", self.base._name),
        }
    }

    /// Returns the contained value mutably; there must be one (see `as_bool`).
    ///
    /// # Panics
    /// As [`Value::get_value`].
    #[inline]
    pub fn get_value_mut(&mut self) -> &mut T {
        match &mut self._value {
            Some(v) => v,
            None => panic!("option {} has no value", self.base._name),
        }
    }

    /// Sets the value, still a default.
    #[inline]
    pub fn set_default_value(&mut self, value: T) {
        self.do_set_value(value, true);
    }

    /// Sets the value.
    #[inline]
    pub fn set_value(&mut self, value: T) {
        self.do_set_value(value, false);
    }
}

impl<T: VisitableValue + 'static> std::ops::Deref for Value<T> {
    type Target = BasicValue;
    #[inline]
    fn deref(&self) -> &BasicValue {
        &self.base
    }
}

// SAFETY: the type is `#[repr(C)]` and begins with its base value, built with its own
// vtable; the pointer is projected from `this`.
unsafe impl<T: VisitableValue + 'static> AsBasicValue for Value<T> {
    #[inline]
    unsafe fn base_ptr(this: *mut Self) -> *mut BasicValue {
        // SAFETY: guaranteed by the caller.
        unsafe { &raw mut (*this).base }
    }
}

/// A switch-style configuration option value.
///
/// Contains no value, can be set or not.
#[repr(C)]
pub struct Switch {
    base: BasicValue,
    _set: Option<bool>,
}

impl Switch {
    const VTABLE: &'static BasicValueVtable =
        &BasicValueVtable { do_describe: Self::do_describe, do_mutate: Self::do_mutate };

    unsafe fn do_describe(_this: *const BasicValue, descriptor: &mut dyn OptionsDescriptor) {
        descriptor.visit_value_switch();
    }

    unsafe fn do_mutate(
        this: *mut BasicValue,
        mutator: &mut dyn OptionsMutator,
    ) -> Result<(), ExceptionPtr> {
        // SAFETY: this vtable is only installed in the base of a Switch.
        let me = unsafe { &mut *(this as *mut Self) };
        let is_set = mutator.visit_value_switch()?;
        if me._set.is_some() {
            // override the value only if it is not preset
            if is_set {
                me._set = Some(true);
            }
        } else {
            me._set = Some(is_set);
        }
        Ok(())
    }

    /// Construct a value.
    ///
    /// - `name`: the name of this value
    /// - `description`: the description of the value
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Switch {
            base: BasicValue::new(Self::VTABLE, true, name.into(), description.into()),
            _set: None,
        }
    }

    /// Construct an unused value.
    pub fn new_unused(name: impl Into<String>, _: Unused) -> Self {
        Switch {
            base: BasicValue::new(Self::VTABLE, false, name.into(), String::new()),
            _set: None,
        }
    }

    /// Is the option set?
    #[inline]
    pub fn as_bool(&self) -> bool {
        self._set.unwrap_or(false)
    }

    /// Sets the switch.
    #[inline]
    pub fn set_value(&mut self) {
        self._set = Some(true);
    }

    /// Clears the switch.
    #[inline]
    pub fn unset_value(&mut self) {
        self._set = Some(false);
    }
}

impl std::ops::Deref for Switch {
    type Target = BasicValue;
    #[inline]
    fn deref(&self) -> &BasicValue {
        &self.base
    }
}

// SAFETY: the type is `#[repr(C)]` and begins with its base value, built with its own
// vtable; the pointer is projected from `this`.
unsafe impl AsBasicValue for Switch {
    #[inline]
    unsafe fn base_ptr(this: *mut Self) -> *mut BasicValue {
        // SAFETY: guaranteed by the caller.
        unsafe { &raw mut (*this).base }
    }
}

/// Alias of [`Switch`], the value type of options without an argument.
pub type MonostateValue = Switch;

/// The function that frees a candidate value.
pub type Deleter<T> = Box<dyn Fn(*mut T)>;

/// An owned candidate value, freed by its [`Deleter`].
pub struct ValueHandle<T> {
    ptr: *mut T,
    deleter: Deleter<T>,
}

impl<T> ValueHandle<T> {
    /// Owns `ptr`, freed with `deleter`.
    ///
    /// # Safety
    /// `ptr` is non-null, points to a live `T` that nothing else accesses while the handle
    /// lives (the selection dereferences it in `get_selected_candidate*`), and `deleter`
    /// frees it exactly as it was allocated.
    pub unsafe fn new(ptr: *mut T, deleter: Deleter<T>) -> Self {
        ValueHandle { ptr, deleter }
    }

    /// Owns the boxed value.
    pub fn from_box(b: Box<T>) -> Self
    where
        T: 'static,
    {
        ValueHandle {
            ptr: Box::into_raw(b),
            // SAFETY: the pointer comes from Box::into_raw above.
            deleter: Box::new(|p| unsafe { drop(Box::from_raw(p)) }),
        }
    }
}

impl<T> Drop for ValueHandle<T> {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            (self.deleter)(self.ptr);
        }
    }
}

/// One candidate of a [`SelectionValue`].
pub struct Candidate<T> {
    /// The name the users choose by.
    pub name: String,
    /// The candidate.
    pub value: ValueHandle<T>,
    /// Its options, if any; boxed, and linked in its box by its maker.
    pub opts: Option<Pin<Box<dyn AsOptionGroup>>>,
}

/// A candidate-specific option group, usually a struct embedding an [`OptionGroup`].
pub trait AsOptionGroup {
    /// The group.
    fn group(&self) -> &OptionGroup;
    /// The group.
    fn group_mut(&mut self) -> &mut OptionGroup;
    /// For the downcast to the concrete options.
    fn as_any(&self) -> &dyn std::any::Any;
    /// For the downcast to the concrete options.
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any;
}

impl AsOptionGroup for OptionGroup {
    fn group(&self) -> &OptionGroup {
        self
    }
    fn group_mut(&mut self) -> &mut OptionGroup {
        self
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

/// The candidates of a [`SelectionValue`].
pub type Candidates<T> = Vec<Candidate<T>>;

const NO_SELECTED_CANDIDATE: usize = usize::MAX;

/// A selection value, allows selection from multiple candidates.
///
/// The candidates objects are of an opaque type which may not accessible to
/// whoever is choosing between the available candidates. This allows the user
/// selecting between engine internal types without exposing them.
/// Each candidate has a name, which is what the users choose based on. Each
/// candidate can also have an associated `OptionGroup` containing related
/// candidate-specific configuration options, allowing further configuring the
/// selected candidate. The code exposing the candidates should document the
/// concrete types these can be down-casted to.
#[repr(C)]
pub struct SelectionValue<T: 'static> {
    base: BasicValue,
    _candidates: Candidates<T>,
    _selected_candidate: usize,
    _defaulted: bool,
}

impl<T: 'static> SelectionValue<T> {
    const VTABLE: &'static BasicValueVtable =
        &BasicValueVtable { do_describe: Self::do_describe, do_mutate: Self::do_mutate };

    /// The names of the candidates.
    pub fn get_candidate_names(&self) -> Vec<String> {
        let mut candidate_names = Vec::with_capacity(self._candidates.len());
        for c in &self._candidates {
            candidate_names.push(c.name.clone());
        }
        candidate_names
    }

    unsafe fn do_describe(this: *const BasicValue, descriptor: &mut dyn OptionsDescriptor) {
        // SAFETY: this vtable is only installed in the base of a SelectionValue<T>.
        let me = unsafe { &*(this as *const Self) };
        let sel = me._selected_candidate;
        descriptor.visit_selection_value(
            &me.get_candidate_names(),
            if sel == NO_SELECTED_CANDIDATE { None } else { Some(&sel) },
        );
        for c in &me._candidates {
            if let Some(opts) = &c.opts {
                opts.group().describe(descriptor);
            }
        }
    }

    unsafe fn do_mutate(
        this: *mut BasicValue,
        mutator: &mut dyn OptionsMutator,
    ) -> Result<(), ExceptionPtr> {
        // SAFETY: as in do_describe().
        let me = unsafe { &mut *(this as *mut Self) };
        let names = me.get_candidate_names();
        if mutator.visit_selection_value(&names, &mut me._selected_candidate)? {
            me._defaulted = false;
        }
        if me._selected_candidate != NO_SELECTED_CANDIDATE {
            let sel = me._selected_candidate;
            // An out-of-range selection is an error.
            let Some(c) = me._candidates.get_mut(sel) else {
                return Err(ExceptionPtr::new(crate::engine::core::future::OutOfRange::new(
                    format!(
                        "vector::_M_range_check: __n (which is {}) >= this->size() (which is {})",
                        sel,
                        names.len()
                    ),
                )));
            };
            if let Some(opts) = &mut c.opts {
                // SAFETY: the options are only mutated in place, never moved.
                unsafe { opts.as_mut().get_unchecked_mut() }.group_mut().mutate(mutator)?;
            }
        }
        Ok(())
    }

    fn find_candidate(&self, candidate_name: &str) -> Result<usize, ExceptionPtr> {
        match self._candidates.iter().position(|candidate| candidate.name == candidate_name) {
            Some(i) => Ok(i),
            None => Err(ExceptionPtr::new(crate::engine::core::future::InvalidArgument::new(
                format!("find_candidate(): failed to find candidate {}", candidate_name),
            ))),
        }
    }

    fn do_select_candidate(
        &mut self,
        candidate_name: &str,
        defaulted: bool,
    ) -> Result<Option<&mut OptionGroup>, ExceptionPtr> {
        self._selected_candidate = self.find_candidate(candidate_name)?;
        self._defaulted = defaulted;
        Ok(self.get_selected_candidate_opts_mut())
    }

    #[inline]
    fn construct(used: bool, name: String, candidates: Candidates<T>, description: String) -> Self {
        SelectionValue {
            base: BasicValue::new(Self::VTABLE, used, name, description),
            _candidates: candidates,
            _selected_candidate: NO_SELECTED_CANDIDATE,
            _defaulted: true,
        }
    }

    /// Construct a value.
    ///
    /// - `name`: the name of this value
    /// - `candidates`: the available candidates
    /// - `default_candidates`: the name of the default candidate
    /// - `description`: the description of the value
    pub fn new_with_default(
        name: impl Into<String>,
        candidates: Candidates<T>,
        default_candidate: &str,
        description: impl Into<String>,
    ) -> Result<Self, ExceptionPtr> {
        let mut v = Self::construct(true, name.into(), candidates, description.into());
        v._selected_candidate = v.find_candidate(default_candidate)?;
        Ok(v)
    }

    /// Construct a value without a default candidate.
    pub fn new(
        name: impl Into<String>,
        candidates: Candidates<T>,
        description: impl Into<String>,
    ) -> Self {
        Self::construct(true, name.into(), candidates, description.into())
    }

    /// Construct an unused value.
    pub fn new_unused(name: impl Into<String>, _: Unused) -> Self {
        Self::construct(false, name.into(), Vec::new(), String::new())
    }

    /// Was there a candidate selected (default also counts)?
    #[inline]
    pub fn as_bool(&self) -> bool {
        self._selected_candidate != NO_SELECTED_CANDIDATE
    }

    /// Is the currently selected candidate the default one?
    #[inline]
    pub fn defaulted(&self) -> bool {
        self._defaulted
    }

    /// The selected candidate.
    ///
    /// # Panics
    /// When no candidate is selected.
    #[inline]
    fn selected(&self) -> &Candidate<T> {
        match self._candidates.get(self._selected_candidate) {
            Some(c) => c,
            None => panic!("out_of_range: no candidate selected for {}", self.base._name),
        }
    }

    /// The selected candidate, mutably.
    ///
    /// # Panics
    /// When no candidate is selected.
    #[inline]
    fn selected_mut(&mut self) -> &mut Candidate<T> {
        let sel = self._selected_candidate;
        match self._candidates.get_mut(sel) {
            Some(c) => c,
            None => panic!("out_of_range: no candidate selected for {}", self.base._name),
        }
    }

    /// Get the name of the currently selected candidate (there must be one selected, see `as_bool`).
    #[inline]
    pub fn get_selected_candidate_name(&self) -> &str {
        &self.selected().name
    }

    /// Get the options of the currently selected candidate (there must be one selected, see `as_bool`).
    #[inline]
    pub fn get_selected_candidate_opts(&self) -> Option<&OptionGroup> {
        self.selected().opts.as_ref().map(|o| o.group())
    }

    /// Get the options of the currently selected candidate (there must be one selected, see `as_bool`).
    #[inline]
    pub fn get_selected_candidate_opts_mut(&mut self) -> Option<&mut OptionGroup> {
        // SAFETY: the options are only mutated in place, never moved.
        self.selected_mut()
            .opts
            .as_mut()
            .map(|o| unsafe { o.as_mut().get_unchecked_mut() }.group_mut())
    }

    /// The candidate-specific options downcast to their concrete type.
    #[inline]
    pub fn get_selected_candidate_opts_as<O: 'static>(&self) -> Option<&O> {
        self.selected().opts.as_ref().and_then(|o| o.as_any().downcast_ref::<O>())
    }

    /// The candidate-specific options downcast to their concrete type, mutably
    /// (see [`get_selected_candidate_opts_mut`](Self::get_selected_candidate_opts_mut)).
    #[inline]
    pub fn get_selected_candidate_opts_as_mut<O: 'static>(&mut self) -> Option<&mut O> {
        self.selected_mut().opts.as_mut().and_then(
            // SAFETY: the options are only mutated in place, never moved.
            |o| unsafe { o.as_mut().get_unchecked_mut() }.as_any_mut().downcast_mut::<O>(),
        )
    }

    /// The selected candidate (there must be one selected, see `as_bool`).
    #[inline]
    pub fn get_selected_candidate(&self) -> &T {
        // SAFETY: the candidate value is owned by its handle, which lives as long as self.
        unsafe { &*self.selected().value.ptr }
    }

    /// The selected candidate (there must be one selected, see `as_bool`).
    #[inline]
    pub fn get_selected_candidate_mut(&mut self) -> &mut T {
        // SAFETY: as above, reached through `&mut self`.
        unsafe { &mut *self.selected_mut().value.ptr }
    }

    /// Select a candidate.
    ///
    /// - `candidate_name`: the name of the to-be-selected candidate.
    pub fn select_candidate(
        &mut self,
        candidate_name: &str,
    ) -> Result<Option<&mut OptionGroup>, ExceptionPtr> {
        self.do_select_candidate(candidate_name, false)
    }

    /// Select a candidate and make it the default.
    ///
    /// - `candidate_name`: the name of the to-be-selected candidate.
    pub fn select_default_candidate(
        &mut self,
        candidate_name: &str,
    ) -> Result<Option<&mut OptionGroup>, ExceptionPtr> {
        self.do_select_candidate(candidate_name, true)
    }
}

impl<T: 'static> std::ops::Deref for SelectionValue<T> {
    type Target = BasicValue;
    #[inline]
    fn deref(&self) -> &BasicValue {
        &self.base
    }
}

// SAFETY: the type is `#[repr(C)]` and begins with its base value, built with its own
// vtable; the pointer is projected from `this`.
unsafe impl<T: 'static> AsBasicValue for SelectionValue<T> {
    #[inline]
    unsafe fn base_ptr(this: *mut Self) -> *mut BasicValue {
        // SAFETY: guaranteed by the caller.
        unsafe { &raw mut (*this).base }
    }
}
