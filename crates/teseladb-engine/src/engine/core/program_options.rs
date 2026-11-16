//! Command-line and configuration-file parsing for the engine's option groups.
//!
//! [`bpo`] is a small option parser: option descriptions with typed values and defaults, nested
//! groups, positional options, the command-line and configuration-file parsers, the map of parsed
//! values, [`bpo::store`], [`bpo::notify`] and the `--help` output.
//!
//! Two visitors connect it to the engine's option trees
//! ([`util::program_options`](crate::engine::util::program_options)):
//! [`OptionsDescriptionBuildingVisitor`] walks a tree and builds the parser's description of every
//! used option, and [`VariablesMapExtractingVisitor`] walks it again after parsing and writes the
//! parsed values into the tree. [`AppTemplate`](crate::engine::core::app_template::AppTemplate)
//! drives both.

use std::any::Any;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::rc::Rc;

use crate::engine::core::future::{ExceptionPtr, RuntimeError, make_exception_ptr};
use crate::engine::core::resource;
use crate::engine::core::sstring::Sstring;
use crate::engine::util::log::{LogLevel, LoggerOstreamType, LoggerTimestampStyle};
use crate::engine::util::log_cli;
use crate::engine::util::memory_diagnostics::memory::AllocFailureKind;
use crate::engine::util::program_options::{OptionsDescriptor, OptionsMutator};

/// The option parser: descriptions, parsers, the value map and the help output.
pub mod bpo {
    use super::*;

    /// An option parsing or validation error, carrying a human-readable message.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Error {
        msg: String,
    }

    impl Error {
        /// An error with `msg`.
        pub fn new(msg: impl Into<String>) -> Self {
            Self { msg: msg.into() }
        }

        /// Returns the message.
        pub fn what(&self) -> &str {
            &self.msg
        }
    }

    impl fmt::Display for Error {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(&self.msg)
        }
    }

    impl std::error::Error for Error {}

    /// The ways a value can fail validation. The messages do not name the option; the caller
    /// adds it.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum ValidationError {
        /// The option was given more than once.
        MultipleOccurrences,
        /// The option takes a single argument but got several.
        MultipleValuesNotAllowed,
        /// The option requires an argument but got none.
        AtLeastOneValueRequired,
        /// The argument could not be parsed.
        InvalidOptionValue(String),
    }

    impl fmt::Display for ValidationError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                ValidationError::MultipleOccurrences => {
                    f.write_str("option cannot be specified more than once")
                }
                ValidationError::MultipleValuesNotAllowed => {
                    f.write_str("option only takes a single argument")
                }
                ValidationError::AtLeastOneValueRequired => {
                    f.write_str("option requires at least one argument")
                }
                ValidationError::InvalidOptionValue(v) => {
                    write!(f, "the argument ('{}') for option is invalid", v)
                }
            }
        }
    }

    impl std::error::Error for ValidationError {}

    impl From<ValidationError> for Error {
        #[inline]
        fn from(e: ValidationError) -> Self {
            Error::new(e.to_string())
        }
    }

    /// Helpers for implementing [`Validate`].
    pub mod validators {
        use super::ValidationError;

        /// Fails with [`ValidationError::MultipleOccurrences`] if a value was already set
        /// (`value_set`).
        #[inline]
        pub fn check_first_occurrence(value_set: bool) -> Result<(), ValidationError> {
            if value_set {
                return Err(ValidationError::MultipleOccurrences);
            }
            Ok(())
        }

        /// Returns the only token of `v`.
        ///
        /// # Errors
        /// Fails if `v` has no token or more than one.
        #[inline]
        pub fn get_single_string(v: &[String]) -> Result<&str, ValidationError> {
            get_single_string_with(v, false)
        }

        /// Returns the only token of `v`, or the empty string when there is none and
        /// `allow_empty` is set.
        ///
        /// # Errors
        /// Fails if `v` has more than one token, or none and `allow_empty` is not set.
        #[inline]
        pub fn get_single_string_with(
            v: &[String],
            allow_empty: bool,
        ) -> Result<&str, ValidationError> {
            if v.len() > 1 {
                return Err(ValidationError::MultipleValuesNotAllowed);
            } else if v.len() == 1 {
                return Ok(&v[0]);
            } else if !allow_empty {
                return Err(ValidationError::AtLeastOneValueRequired);
            }
            Ok("")
        }
    }

    /// How an option's tokens become a typed value: arity, defaults, parsing and notification.
    pub trait ValueSemantic {
        /// Whether the option takes an argument.
        fn takes_value(&self) -> bool;
        /// Whether several occurrences accumulate into one value (as for vectors).
        fn is_composing(&self) -> bool;
        /// Whether one occurrence takes several tokens.
        fn is_multitoken(&self) -> bool;
        /// Whether the option must be given.
        fn is_required(&self) -> bool;
        /// Parses `tokens` into a value, merging with `previous` for composing options.
        fn parse(
            &self,
            previous: Option<Box<dyn Any>>,
            tokens: &[String],
        ) -> Result<Box<dyn Any>, Error>;
        /// The default value and its text, if any.
        fn default_value(&self) -> Option<(Box<dyn Any>, String)>;
        /// The value given when the option appears without argument, if any.
        fn implicit_value(&self) -> Option<String>;
        /// Runs the notifier with the final value.
        fn notify(&self, value: &dyn Any);
    }

    /// A type that can be parsed from one command-line token.
    pub trait ParseToken: Sized + 'static {
        /// Parses one token; the error is a message describing why it is invalid.
        fn parse_token(s: &str) -> Result<Self, String>;
    }

    /// Implements [`ParseToken`] through `FromStr`.
    macro_rules! parse_from_str {
        ($($t:ty),*) => {$(
            impl ParseToken for $t {
                fn parse_token(s: &str) -> Result<Self, String> {
                    s.parse::<$t>().map_err(|e| e.to_string())
                }
            }
        )*};
    }

    parse_from_str!(i8, i16, i32, i64, u8, u16, u32, u64, usize, isize, f32, f64, String, char);

    impl ParseToken for bool {
        fn parse_token(s: &str) -> Result<Self, String> {
            match s.to_ascii_lowercase().as_str() {
                "" | "on" | "yes" | "1" | "true" => Ok(true),
                "off" | "no" | "0" | "false" => Ok(false),
                _ => Err(format!("'{}' is not a valid boolean", s)),
            }
        }
    }

    impl ParseToken for Sstring {
        fn parse_token(s: &str) -> Result<Self, String> {
            Ok(Sstring::from(s))
        }
    }

    /// Turns the tokens of one occurrence of an option into its value, given the value stored
    /// from earlier occurrences.
    ///
    /// [`TypedValue<T>`] calls it for each occurrence. Every [`ParseToken`] type gets the default
    /// rule (a single occurrence of a single token); a type that accumulates, such as
    /// [`StringMap`](crate::engine::util::program_options::StringMap), implements it by hand.
    pub trait Validate: Clone + fmt::Display + 'static {
        /// Returns the value after this occurrence (`previous` is the stored one, if any).
        ///
        /// # Errors
        /// Fails if the occurrence is not allowed or its tokens cannot be parsed.
        fn validate(previous: Option<Self>, tokens: &[String]) -> Result<Self, Error>;
    }

    impl<T: ParseToken + Clone + fmt::Display> Validate for T {
        fn validate(previous: Option<Self>, tokens: &[String]) -> Result<Self, Error> {
            if previous.is_some() {
                return Err(Error::new("option cannot be specified more than once"));
            }
            let tok = match tokens {
                [t] => t,
                _ => return Err(Error::new("option cannot be specified more than once")),
            };
            T::parse_token(tok)
                .map_err(|e| Error::new(format!("the argument ('{}') is invalid: {}", tok, e)))
        }
    }

    /// Every occurrence adds its `key=value` associations, and the last one of a key wins.
    impl Validate for crate::engine::util::program_options::StringMap {
        fn validate(previous: Option<Self>, tokens: &[String]) -> Result<Self, Error> {
            let mut out = previous;
            crate::engine::util::program_options::validate_string_map(&mut out, tokens)?;
            Ok(out.unwrap_or_default())
        }
    }

    /// The callback run by [`notify`] with an option's final value.
    type Notifier<T> = Box<dyn Fn(&T)>;

    /// The value semantic of an option with a single value of type `T`.
    pub struct TypedValue<T: Validate> {
        default: Option<(T, String)>,
        implicit: Option<String>,
        required: bool,
        notifier: Option<Notifier<T>>,
    }

    /// Returns the semantic of an option with a single value of type `T`, with no default.
    pub fn value<T: Validate>() -> TypedValue<T> {
        TypedValue { default: None, implicit: None, required: false, notifier: None }
    }

    impl<T: Validate> TypedValue<T> {
        /// Sets the value used when the option is not given; it is shown in the help text.
        pub fn default_value(mut self, v: T) -> Self {
            let text = v.to_string();
            self.default = Some((v, text));
            self
        }

        /// Sets the value used when the option is given without an argument.
        pub fn implicit_value(mut self, v: T) -> Self {
            self.implicit = Some(v.to_string());
            self
        }

        /// Marks the option as required: [`notify`] fails if it was not given.
        pub fn required(mut self) -> Self {
            self.required = true;
            self
        }

        /// Sets a callback that [`notify`] runs with the final value.
        pub fn notifier(mut self, f: impl Fn(&T) + 'static) -> Self {
            self.notifier = Some(Box::new(f));
            self
        }
    }

    impl<T: Validate> ValueSemantic for TypedValue<T> {
        fn takes_value(&self) -> bool {
            true
        }
        fn is_composing(&self) -> bool {
            false
        }
        fn is_multitoken(&self) -> bool {
            false
        }
        fn is_required(&self) -> bool {
            self.required
        }

        fn parse(
            &self,
            previous: Option<Box<dyn Any>>,
            tokens: &[String],
        ) -> Result<Box<dyn Any>, Error> {
            // The stored value is always a `T` (this semantic is the only one that writes it).
            let previous = previous.and_then(|p| p.downcast::<T>().ok()).map(|b| *b);
            let v = if tokens.is_empty() {
                let tok = self
                    .implicit
                    .clone()
                    .ok_or_else(|| Error::new("the required argument for option is missing"))?;
                T::validate(previous, std::slice::from_ref(&tok))?
            } else {
                T::validate(previous, tokens)?
            };
            Ok(Box::new(v))
        }

        fn default_value(&self) -> Option<(Box<dyn Any>, String)> {
            self.default.as_ref().map(|(v, s)| (Box::new(v.clone()) as Box<dyn Any>, s.clone()))
        }

        fn implicit_value(&self) -> Option<String> {
            self.implicit.clone()
        }

        fn notify(&self, value: &dyn Any) {
            if let (Some(n), Some(v)) = (&self.notifier, value.downcast_ref::<T>()) {
                n(v);
            }
        }
    }

    /// The value semantic of an option that collects every occurrence and token into a `Vec<T>`.
    pub struct VectorValue<T: ParseToken + Clone> {
        default: Option<Vec<T>>,
        multitoken: bool,
        required: bool,
    }

    /// Returns the semantic of a vector option, with no default.
    pub fn vector_value<T: ParseToken + Clone>() -> VectorValue<T> {
        VectorValue { default: None, multitoken: false, required: false }
    }

    impl<T: ParseToken + Clone> VectorValue<T> {
        /// Lets one occurrence take several tokens (up to the next option).
        pub fn multitoken(mut self) -> Self {
            self.multitoken = true;
            self
        }

        /// Accumulates occurrences; vector options always do, so this changes nothing.
        pub fn composing(self) -> Self {
            self
        }

        /// Sets the value used when the option is not given.
        pub fn default_value(mut self, v: Vec<T>) -> Self {
            self.default = Some(v);
            self
        }

        /// Marks the option as required: [`notify`] fails if it was not given.
        pub fn required(mut self) -> Self {
            self.required = true;
            self
        }
    }

    impl<T: ParseToken + Clone> ValueSemantic for VectorValue<T> {
        fn takes_value(&self) -> bool {
            true
        }
        fn is_composing(&self) -> bool {
            true
        }
        fn is_multitoken(&self) -> bool {
            self.multitoken
        }
        fn is_required(&self) -> bool {
            self.required
        }

        fn parse(
            &self,
            previous: Option<Box<dyn Any>>,
            tokens: &[String],
        ) -> Result<Box<dyn Any>, Error> {
            let mut v: Vec<T> = match previous.and_then(|p| p.downcast::<Vec<T>>().ok()) {
                Some(p) => *p,
                None => Vec::new(),
            };
            for tok in tokens {
                v.push(T::parse_token(tok).map_err(|e| {
                    Error::new(format!("the argument ('{}') is invalid: {}", tok, e))
                })?);
            }
            Ok(Box::new(v))
        }

        fn default_value(&self) -> Option<(Box<dyn Any>, String)> {
            self.default.as_ref().map(|v| (Box::new(v.clone()) as Box<dyn Any>, String::new()))
        }

        fn implicit_value(&self) -> Option<String> {
            None
        }

        fn notify(&self, _value: &dyn Any) {}
    }

    /// One option: its long and optional short name, value semantic and help text.
    pub struct OptionDescription {
        long_name: String,
        short_name: Option<char>,
        semantic: Option<Box<dyn ValueSemantic>>,
        description: String,
    }

    impl OptionDescription {
        /// Creates an option. `name` is `"long"` or `"long,s"` (with a one-letter short name);
        /// `semantic` is `None` for a flag without value.
        pub fn new(
            name: &str,
            semantic: Option<Box<dyn ValueSemantic>>,
            description: &str,
        ) -> Self {
            let (long_name, short_name) = match name.split_once(',') {
                Some((l, s)) => (l.to_string(), s.chars().next()),
                None => (name.to_string(), None),
            };
            Self { long_name, short_name, semantic, description: description.to_string() }
        }

        /// The long name.
        pub fn long_name(&self) -> &str {
            &self.long_name
        }

        /// The value semantic, if the option takes a value.
        pub fn semantic(&self) -> Option<&dyn ValueSemantic> {
            self.semantic.as_deref()
        }

        /// Returns the name as the help text shows it.
        fn format_name(&self) -> String {
            match self.short_name {
                Some(s) => format!("-{} [ --{} ]", s, self.long_name),
                None => format!("--{}", self.long_name),
            }
        }

        /// Returns the ` arg (=default)` part of the help text.
        fn format_parameter(&self) -> String {
            match &self.semantic {
                None => String::new(),
                Some(s) => {
                    let mut p = " arg".to_string();
                    if let Some((_, text)) = s.default_value()
                        && !text.is_empty()
                    {
                        p.push_str(&format!(" (={})", text));
                    }
                    p
                }
            }
        }
    }

    /// A captioned list of options and nested groups; also the source of the `--help` text.
    #[derive(Default, Clone)]
    pub struct OptionsDescription {
        caption: String,
        options: Vec<Rc<OptionDescription>>,
        groups: Vec<OptionsDescription>,
    }

    /// Chained builder returned by [`OptionsDescription::add_options`].
    pub struct EasyInit<'a> {
        owner: &'a mut OptionsDescription,
    }

    impl EasyInit<'_> {
        /// Adds a flag (an option without value).
        pub fn flag(self, name: &str, description: &str) -> Self {
            self.owner.add_option(OptionDescription::new(name, None, description));
            self
        }

        /// Adds an option with a value described by `semantic`.
        pub fn opt(
            self,
            name: &str,
            semantic: impl ValueSemantic + 'static,
            description: &str,
        ) -> Self {
            self.owner.add_option(OptionDescription::new(
                name,
                Some(Box::new(semantic)),
                description,
            ));
            self
        }
    }

    impl OptionsDescription {
        /// Returns an empty description captioned `caption`.
        pub fn new(caption: &str) -> Self {
            Self { caption: caption.to_string(), options: Vec::new(), groups: Vec::new() }
        }

        /// Starts adding options with a chained builder.
        pub fn add_options(&mut self) -> EasyInit<'_> {
            EasyInit { owner: self }
        }

        /// Adds one option.
        pub fn add_option(&mut self, d: OptionDescription) {
            self.options.push(Rc::new(d));
        }

        /// Nests `group` inside this description.
        pub fn add(&mut self, group: OptionsDescription) {
            self.groups.push(group);
        }

        /// Returns every option, own and nested, in order. O(n).
        fn all_options(&self) -> Vec<Rc<OptionDescription>> {
            let mut v: Vec<Rc<OptionDescription>> = self.options.clone();
            for g in &self.groups {
                v.extend(g.all_options());
            }
            v
        }

        /// Finds an option by long name. O(n).
        fn find_long(&self, name: &str) -> Option<Rc<OptionDescription>> {
            self.all_options().into_iter().find(|o| o.long_name == name)
        }

        /// Finds an option by short name. O(n).
        fn find_short(&self, c: char) -> Option<Rc<OptionDescription>> {
            self.all_options().into_iter().find(|o| o.short_name == Some(c))
        }

        /// Writes the help text of this description and its groups, with names padded to `width`.
        fn print(&self, f: &mut fmt::Formatter<'_>, width: usize) -> fmt::Result {
            if !self.caption.is_empty() {
                writeln!(f, "{}:", self.caption)?;
            }
            for o in &self.options {
                let left = format!("  {}{}", o.format_name(), o.format_parameter());
                if left.len() >= width {
                    writeln!(f, "{}", left)?;
                    writeln!(f, "{:width$}{}", "", o.description, width = width)?;
                } else {
                    writeln!(f, "{:<width$}{}", left, o.description, width = width)?;
                }
            }
            for g in &self.groups {
                writeln!(f)?;
                g.print(f, width)?;
            }
            Ok(())
        }

        /// Returns the column at which descriptions start (at most 40).
        fn name_width(&self) -> usize {
            let own = self
                .options
                .iter()
                .map(|o| 2 + o.format_name().len() + o.format_parameter().len() + 2)
                .max()
                .unwrap_or(0);
            let nested = self.groups.iter().map(|g| g.name_width()).max().unwrap_or(0);
            own.max(nested).min(40)
        }
    }

    /// Formats the help text: the caption, one line per option, then each nested group.
    impl fmt::Display for OptionsDescription {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            let w = self.name_width();
            self.print(f, w)
        }
    }

    /// Maps positional command-line arguments to option names.
    #[derive(Debug, Clone, Default)]
    pub struct PositionalOptionsDescription {
        names: Vec<(String, i32)>,
    }

    impl PositionalOptionsDescription {
        /// Assigns the next `max_count` positional arguments to `name`; `-1` takes every remaining
        /// one.
        pub fn add(&mut self, name: &str, max_count: i32) -> &mut Self {
            self.names.push((name.to_string(), max_count));
            self
        }

        /// Returns the option the positional argument at `position` belongs to, if any.
        pub fn name_for_position(&self, mut position: usize) -> Option<&str> {
            for (n, c) in &self.names {
                if *c < 0 {
                    return Some(n);
                }
                if position < *c as usize {
                    return Some(n);
                }
                position -= *c as usize;
            }
            None
        }
    }

    /// The value of one option in a [`VariablesMap`].
    pub struct VariableValue {
        value: Box<dyn Any>,
        defaulted: bool,
        semantic: Option<Rc<OptionDescription>>,
    }

    impl VariableValue {
        /// Whether the value comes from the default.
        pub fn defaulted(&self) -> bool {
            self.defaulted
        }

        /// Returns the value as a `T`, or `None` if it has another type.
        pub fn as_<T: 'static>(&self) -> Option<&T> {
            self.value.downcast_ref::<T>()
        }

        /// The value as `Any`.
        pub fn value(&self) -> &dyn Any {
            &*self.value
        }
    }

    /// The parsed values, by long option name.
    #[derive(Default)]
    pub struct VariablesMap {
        values: BTreeMap<String, VariableValue>,
        required: Vec<String>,
    }

    impl VariablesMap {
        /// An empty map.
        pub fn new() -> Self {
            Self::default()
        }

        /// Returns the value of `name`, if any.
        pub fn find(&self, name: &str) -> Option<&VariableValue> {
            self.values.get(name)
        }

        /// Returns 1 if `name` has a value (given or defaulted), 0 otherwise.
        pub fn count(&self, name: &str) -> usize {
            self.values.contains_key(name) as usize
        }

        /// Returns the value of `name` as a `T`, if present with that type.
        pub fn get<T: 'static>(&self, name: &str) -> Option<&T> {
            self.values.get(name).and_then(|v| v.as_::<T>())
        }
    }

    /// One parsed occurrence of an option, with its tokens.
    pub struct ParsedOption {
        desc: Rc<OptionDescription>,
        tokens: Vec<String>,
    }

    /// The result of a parser, to pass to [`store`].
    pub struct ParsedOptions {
        options: Vec<ParsedOption>,
        all: Vec<Rc<OptionDescription>>,
    }

    /// Parser of command-line arguments, configured with options and positional options.
    pub struct CommandLineParser<'a> {
        args: Vec<String>,
        desc: Option<&'a OptionsDescription>,
        pos: Option<&'a PositionalOptionsDescription>,
    }

    /// Returns a parser of `args`, which must not include the program name.
    pub fn command_line_parser(args: Vec<String>) -> CommandLineParser<'static> {
        CommandLineParser { args, desc: None, pos: None }
    }

    impl<'a> CommandLineParser<'a> {
        /// Sets the options to recognise.
        pub fn options<'b>(self, desc: &'b OptionsDescription) -> CommandLineParser<'b>
        where
            'a: 'b,
        {
            CommandLineParser { args: self.args, desc: Some(desc), pos: self.pos }
        }

        /// Sets the mapping of positional arguments.
        pub fn positional<'b>(self, pos: &'b PositionalOptionsDescription) -> CommandLineParser<'b>
        where
            'a: 'b,
        {
            CommandLineParser { args: self.args, desc: self.desc, pos: Some(pos) }
        }

        /// Parses the arguments.
        ///
        /// Accepts `--name`, `--name=value`, `-c`, `-cvalue`, separate value tokens, `--` to end
        /// options, and positional arguments.
        ///
        /// # Errors
        /// Fails on an unknown option, a missing or unexpected argument, or too many positional
        /// arguments.
        pub fn run(self) -> Result<ParsedOptions, Error> {
            let empty = OptionsDescription::default();
            let desc = self.desc.unwrap_or(&empty);
            let mut out = Vec::new();
            let mut positional_index = 0usize;
            let mut i = 0;
            let args = &self.args;
            let mut only_positional = false;
            while i < args.len() {
                let a = &args[i];
                i += 1;
                if !only_positional && a == "--" {
                    only_positional = true;
                    continue;
                }
                let (opt, inline_value) = if !only_positional && a.starts_with("--") {
                    let body = &a[2..];
                    let (name, val) = match body.split_once('=') {
                        Some((n, v)) => (n, Some(v.to_string())),
                        None => (body, None),
                    };
                    let o = desc
                        .find_long(name)
                        .ok_or_else(|| Error::new(format!("unrecognised option '--{}'", name)))?;
                    (o, val)
                } else if !only_positional && a.starts_with('-') && a.len() > 1 {
                    let mut chars = a[1..].chars();
                    let c = chars.next().unwrap_or('-');
                    let rest: String = chars.collect();
                    let o = desc
                        .find_short(c)
                        .ok_or_else(|| Error::new(format!("unrecognised option '-{}'", c)))?;
                    (o, if rest.is_empty() { None } else { Some(rest) })
                } else {
                    let name = self.pos.and_then(|p| p.name_for_position(positional_index))
                        .ok_or_else(|| Error::new("too many positional options have been specified on the command line"))?;
                    positional_index += 1;
                    let o = desc
                        .find_long(name)
                        .ok_or_else(|| Error::new(format!("unrecognised option '{}'", name)))?;
                    out.push(ParsedOption { desc: o, tokens: vec![a.clone()] });
                    continue;
                };
                let mut tokens = Vec::new();
                match opt.semantic() {
                    None => {
                        if inline_value.is_some() {
                            return Err(Error::new(format!(
                                "option '{}' does not take any arguments",
                                opt.long_name
                            )));
                        }
                    }
                    Some(s) => {
                        if let Some(v) = inline_value {
                            tokens.push(v);
                        } else if s.implicit_value().is_none()
                            || (i < args.len() && !args[i].starts_with('-'))
                        {
                            if i < args.len()
                                && (!args[i].starts_with('-') || s.implicit_value().is_none())
                            {
                                tokens.push(args[i].clone());
                                i += 1;
                            } else if s.implicit_value().is_none() {
                                return Err(Error::new(format!(
                                    "the required argument for option '--{}' is missing",
                                    opt.long_name
                                )));
                            }
                        }
                        if s.is_multitoken() {
                            while i < args.len() && !args[i].starts_with('-') {
                                tokens.push(args[i].clone());
                                i += 1;
                            }
                        }
                    }
                }
                out.push(ParsedOption { desc: opt, tokens });
            }
            Ok(ParsedOptions { options: out, all: desc.all_options() })
        }
    }

    /// Parses a configuration file: `name = value` lines, `[section]` lines that prefix the names
    /// that follow with `section.`, and `#` comments.
    ///
    /// # Errors
    /// Fails on a read error, a line without `=`, or an unknown option.
    pub fn parse_config_file<R: std::io::BufRead>(
        is: R,
        desc: &OptionsDescription,
    ) -> Result<ParsedOptions, Error> {
        let mut out = Vec::new();
        let mut prefix = String::new();
        for line in is.lines() {
            let line = line.map_err(|e| Error::new(e.to_string()))?;
            let line = match line.find('#') {
                Some(p) => &line[..p],
                None => &line[..],
            };
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if line.starts_with('[') && line.ends_with(']') {
                prefix = format!("{}.", line[1..line.len() - 1].trim());
                continue;
            }
            let (name, value) = line
                .split_once('=')
                .ok_or_else(|| Error::new(format!("invalid line in config file: {}", line)))?;
            let name = format!("{}{}", prefix, name.trim());
            let o = desc
                .find_long(&name)
                .ok_or_else(|| Error::new(format!("unrecognised option '{}'", name)))?;
            out.push(ParsedOption { desc: o, tokens: vec![value.trim().to_string()] });
        }
        Ok(ParsedOptions { options: out, all: desc.all_options() })
    }

    /// Stores parsed values into `vm`: values already in `vm` (and not defaulted) win, later
    /// occurrences in `parsed` accumulate or fail per their semantic, and defaults fill the rest.
    ///
    /// # Errors
    /// Fails if a value cannot be parsed or validated; the message names the option.
    pub fn store(parsed: ParsedOptions, vm: &mut VariablesMap) -> Result<(), Error> {
        let mut fresh: BTreeMap<String, (Box<dyn Any>, Rc<OptionDescription>)> = BTreeMap::new();
        for p in parsed.options {
            let name = p.desc.long_name.clone();
            if vm.values.get(&name).is_some_and(|v| !v.defaulted) {
                continue;
            }
            let value: Box<dyn Any> = match p.desc.semantic() {
                None => Box::new(()),
                Some(s) => {
                    let prev = fresh.remove(&name).map(|(v, _)| v);
                    s.parse(prev, &p.tokens)
                        .map_err(|e| Error::new(format!("{} for option '--{}'", e.what(), name)))?
                }
            };
            fresh.insert(name, (value, p.desc));
        }
        for (name, (value, desc)) in fresh {
            vm.values.insert(name, VariableValue { value, defaulted: false, semantic: Some(desc) });
        }
        for d in parsed.all {
            if let Some(s) = d.semantic() {
                if !vm.values.contains_key(&d.long_name)
                    && let Some((v, _)) = s.default_value()
                {
                    vm.values.insert(
                        d.long_name.clone(),
                        VariableValue { value: v, defaulted: true, semantic: Some(d.clone()) },
                    );
                }
                if s.is_required() && !vm.required.contains(&d.long_name) {
                    vm.required.push(d.long_name.clone());
                }
            }
        }
        Ok(())
    }

    /// Checks that every required option was given, then runs the notifiers with the final values.
    ///
    /// # Errors
    /// Fails if a required option is missing.
    pub fn notify(vm: &mut VariablesMap) -> Result<(), Error> {
        for r in &vm.required {
            if !vm.values.get(r).is_some_and(|v| !v.defaulted) {
                return Err(Error::new(format!("the option '--{}' is required but missing", r)));
            }
        }
        for v in vm.values.values() {
            if let Some(d) = &v.semantic
                && let Some(s) = d.semantic()
            {
                s.notify(&*v.value);
            }
        }
        Ok(())
    }
}

/// Returns the option-value name of an allocation failure kind.
fn to_string_alloc_failure_kind(val: AllocFailureKind) -> &'static str {
    match val {
        AllocFailureKind::None => "none",
        AllocFailureKind::Critical => "critical",
        AllocFailureKind::All => "all",
    }
}

/// Returns the option-value name of a log level.
fn to_string_log_level(val: LogLevel) -> &'static str {
    match val {
        LogLevel::Error => "error",
        LogLevel::Warn => "warn",
        LogLevel::Info => "info",
        LogLevel::Debug => "debug",
        LogLevel::Trace => "trace",
    }
}

/// Returns the option-value name of a timestamp style.
fn to_string_logger_timestamp_style(val: LoggerTimestampStyle) -> &'static str {
    match val {
        LoggerTimestampStyle::None => "none",
        LoggerTimestampStyle::Boot => "boot",
        LoggerTimestampStyle::Real => "real",
    }
}

/// Returns the option-value name of a log output stream.
fn to_string_logger_ostream_type(val: LoggerOstreamType) -> &'static str {
    match val {
        LoggerOstreamType::None => "none",
        LoggerOstreamType::Cout => "stdout",
        LoggerOstreamType::Cerr => "stderr",
    }
}

/// Enums that options store by name.
trait EnumOption: Copy + 'static {
    /// Returns the name of the value.
    fn to_str(self) -> &'static str;
    /// Parses a name; fails with a runtime error naming the enum for an unknown one.
    fn from_str_opt(val: &str) -> Result<Self, ExceptionPtr>;
}

impl EnumOption for AllocFailureKind {
    fn to_str(self) -> &'static str {
        to_string_alloc_failure_kind(self)
    }

    fn from_str_opt(val: &str) -> Result<Self, ExceptionPtr> {
        match val {
            "none" => Ok(AllocFailureKind::None),
            "critical" => Ok(AllocFailureKind::Critical),
            "all" => Ok(AllocFailureKind::All),
            _ => Err(make_exception_ptr(RuntimeError::new(format!(
                "Invalid value for enum memory::alloc_failure_kind: {}",
                val
            )))),
        }
    }
}

impl EnumOption for LogLevel {
    fn to_str(self) -> &'static str {
        to_string_log_level(self)
    }

    fn from_str_opt(val: &str) -> Result<Self, ExceptionPtr> {
        match val {
            "error" => Ok(LogLevel::Error),
            "warn" => Ok(LogLevel::Warn),
            "info" => Ok(LogLevel::Info),
            "debug" => Ok(LogLevel::Debug),
            "trace" => Ok(LogLevel::Trace),
            _ => Err(make_exception_ptr(RuntimeError::new(format!(
                "Invalid value for enum log_level: {}",
                val
            )))),
        }
    }
}

impl EnumOption for LoggerTimestampStyle {
    fn to_str(self) -> &'static str {
        to_string_logger_timestamp_style(self)
    }

    fn from_str_opt(val: &str) -> Result<Self, ExceptionPtr> {
        match val {
            "none" => Ok(LoggerTimestampStyle::None),
            "boot" => Ok(LoggerTimestampStyle::Boot),
            "real" => Ok(LoggerTimestampStyle::Real),
            _ => Err(make_exception_ptr(RuntimeError::new(format!(
                "Invalid value for enum logger_timestamp_style: {}",
                val
            )))),
        }
    }
}

impl EnumOption for LoggerOstreamType {
    fn to_str(self) -> &'static str {
        to_string_logger_ostream_type(self)
    }

    fn from_str_opt(val: &str) -> Result<Self, ExceptionPtr> {
        match val {
            "none" => Ok(LoggerOstreamType::None),
            "stdout" => Ok(LoggerOstreamType::Cout),
            "stderr" => Ok(LoggerOstreamType::Cerr),
            _ => Err(make_exception_ptr(RuntimeError::new(format!(
                "Invalid value for enum logger_ostream_type: {}",
                val
            )))),
        }
    }
}

/// Adds option `name` with a single value of type `T` and its optional default.
fn describe_value<T: bpo::ParseToken + Clone + fmt::Display>(
    opts: &mut bpo::OptionsDescription,
    name: &str,
    description: &str,
    default_value: Option<&T>,
) {
    let mut v = bpo::value::<T>();
    if let Some(d) = default_value {
        v = v.default_value(d.clone());
    }
    opts.add_options().opt(name, v, description);
}

/// Adds an enum option, stored by name, with its optional default.
fn describe_enum_value<E: EnumOption>(
    opts: &mut bpo::OptionsDescription,
    d: &ValueMetadata,
    default_value: Option<&E>,
) {
    let mut v = bpo::value::<String>();
    if let Some(e) = default_value {
        v = v.default_value(e.to_str().to_string());
    }
    opts.add_options().opt(&d.name, v, &d.description);
}

/// Copies the explicitly given value of `current_name` into `val` if it has type `T`; returns
/// whether it did.
fn extract_value<T: Clone + 'static>(
    values: &bpo::VariablesMap,
    current_name: &str,
    val: &mut T,
) -> bool {
    match values.find(current_name) {
        Some(v) if !v.defaulted() => match v.as_::<T>() {
            Some(x) => {
                *val = x.clone();
                true
            }
            None => false,
        },
        _ => false,
    }
}

/// Parses the explicitly given name of `current_name` into `val`; returns whether it did.
///
/// # Errors
/// Fails if the name is not a value of the enum.
fn extract_enum_value<E: EnumOption>(
    values: &bpo::VariablesMap,
    current_name: &str,
    val: &mut E,
) -> Result<bool, ExceptionPtr> {
    match values.find(current_name) {
        Some(v) if !v.defaulted() => match v.as_::<String>() {
            Some(s) => {
                *val = E::from_str_opt(s)?;
                Ok(true)
            }
            None => Ok(false),
        },
        _ => Ok(false),
    }
}

/// One option group being described by [`OptionsDescriptionBuildingVisitor`].
pub struct GroupMetadata {
    /// The group's name, also the caption of its help section.
    pub name: String,
    /// The options of the group described so far.
    pub description: bpo::OptionsDescription,
    /// Whether the group is used; unused groups are left out.
    pub used: bool,
    /// Number of used values seen in the group; empty groups are left out.
    pub values: usize,
}

/// The option being described by [`OptionsDescriptionBuildingVisitor`].
#[derive(Debug, Clone)]
pub struct ValueMetadata {
    /// The option name.
    pub name: String,
    /// Its help text.
    pub description: String,
}

/// Builds the parser's description of every used option of an option tree.
///
/// Pass it to the root group's `describe`, then take the result with
/// [`get_options_description`](Self::get_options_description). Groups that are unused or have no
/// used value are left out. `smp` gets the short name `-c` and `memory` the short name `-m`.
#[derive(Default)]
pub struct OptionsDescriptionBuildingVisitor {
    _groups: Vec<GroupMetadata>,
    _current_metadata: Option<ValueMetadata>,
}

impl OptionsDescriptionBuildingVisitor {
    /// Returns a visitor with no group yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the description of the innermost open group.
    ///
    /// # Panics
    /// Panics if no group is open (a value visited outside any group).
    fn top(&mut self) -> &mut bpo::OptionsDescription {
        match self._groups.last_mut() {
            Some(g) => &mut g.description,
            // Invariant: values are only visited inside a group.
            None => panic!("option value visited outside of any group"),
        }
    }

    /// Returns the metadata of the value being visited (empty if none).
    fn metadata(&self) -> ValueMetadata {
        self._current_metadata
            .clone()
            .unwrap_or(ValueMetadata { name: String::new(), description: String::new() })
    }

    /// Adds the current value as option `name`, with its default when there is one.
    fn describe_maybe_default<T: bpo::ParseToken + Clone + fmt::Display>(
        &mut self,
        name: &str,
        default_value: Option<&T>,
    ) {
        let md = self.metadata();
        describe_value(self.top(), name, &md.description, default_value);
    }

    /// Consumes the visitor and returns the description of the root group.
    pub fn get_options_description(mut self) -> bpo::OptionsDescription {
        self._groups.pop().map(|g| g.description).unwrap_or_default()
    }
}

impl OptionsDescriptor for OptionsDescriptionBuildingVisitor {
    fn visit_group_start(&mut self, name: &str, used: bool) -> bool {
        self._groups.push(GroupMetadata {
            name: name.to_string(),
            description: bpo::OptionsDescription::new(name),
            used,
            values: 0,
        });
        used
    }

    fn visit_group_end(&mut self) {
        if self._groups.len() == 1 {
            return;
        }
        let Some(grp) = self._groups.pop() else {
            return;
        };
        if grp.used && grp.values != 0 {
            self.top().add(grp.description);
        }
    }

    fn visit_value_metadata(&mut self, name: &str, description: &str, used: bool) -> bool {
        if !used {
            return false;
        }
        if let Some(g) = self._groups.last_mut() {
            g.values += 1;
        }
        self._current_metadata =
            Some(ValueMetadata { name: name.to_string(), description: description.to_string() });
        true
    }

    fn visit_value_switch(&mut self) {
        let md = self.metadata();
        self.top().add_options().flag(&md.name, &md.description);
    }

    fn visit_value_bool(&mut self, default_value: Option<&bool>) {
        let name = self.metadata().name;
        self.describe_maybe_default(&name, default_value);
    }

    fn visit_value_int(&mut self, default_value: Option<&i32>) {
        let name = self.metadata().name;
        self.describe_maybe_default(&name, default_value);
    }

    fn visit_value_unsigned(&mut self, default_value: Option<&u32>) {
        let mut name = self.metadata().name;
        if name == "smp" {
            name = "smp,c".to_string();
        }
        self.describe_maybe_default(&name, default_value);
    }

    fn visit_value_float(&mut self, default_value: Option<&f32>) {
        let name = self.metadata().name;
        self.describe_maybe_default(&name, default_value);
    }

    fn visit_value_double(&mut self, default_value: Option<&f64>) {
        let name = self.metadata().name;
        self.describe_maybe_default(&name, default_value);
    }

    fn visit_value_string(&mut self, default_value: Option<&String>) {
        let mut name = self.metadata().name;
        if name == "memory" {
            name = "memory,m".to_string();
        }
        self.describe_maybe_default(&name, default_value);
    }

    fn visit_value_unsigned_set(&mut self, _default_value: Option<&BTreeSet<u32>>) {
        let name = self.metadata().name;
        self.describe_maybe_default::<String>(&name, None);
    }

    fn visit_value_alloc_failure_kind(&mut self, default_value: Option<&AllocFailureKind>) {
        let md = self.metadata();
        describe_enum_value(self.top(), &md, default_value);
    }

    fn visit_value_log_level(&mut self, default_value: Option<&LogLevel>) {
        let md = self.metadata();
        describe_enum_value(self.top(), &md, default_value);
    }

    fn visit_value_logger_timestamp_style(&mut self, default_value: Option<&LoggerTimestampStyle>) {
        let md = self.metadata();
        describe_enum_value(self.top(), &md, default_value);
    }

    fn visit_value_logger_ostream_type(&mut self, default_value: Option<&LoggerOstreamType>) {
        let md = self.metadata();
        describe_enum_value(self.top(), &md, default_value);
    }

    fn visit_value_log_level_map(&mut self, _default_value: Option<&HashMap<Sstring, LogLevel>>) {
        let md = self.metadata();
        self.top().add_options().opt(&md.name, bpo::vector_value::<String>(), &md.description);
    }

    fn visit_selection_value(&mut self, candidates: &[String], selected_candidate: Option<&usize>) {
        let name = self.metadata().name;
        let default = selected_candidate.and_then(|&i| candidates.get(i).cloned());
        self.describe_maybe_default::<String>(&name, default.as_ref());
    }
}

/// Copies the parsed values of a [`bpo::VariablesMap`] into an option tree.
///
/// Pass it to the root group's `mutate`. Only values given explicitly (not defaulted) are
/// written. Enum values are parsed by name, CPU sets in `cpuset(7)` list format, and log level
/// maps as `name=level` associations; an invalid value fails the walk.
pub struct VariablesMapExtractingVisitor<'a> {
    _values: &'a bpo::VariablesMap,
    _current_name: Option<String>,
}

impl<'a> VariablesMapExtractingVisitor<'a> {
    /// Returns a visitor reading from `values`.
    pub fn new(values: &'a bpo::VariablesMap) -> Self {
        Self { _values: values, _current_name: None }
    }

    /// Returns the name of the value being visited (empty if none).
    fn name(&self) -> &str {
        self._current_name.as_deref().unwrap_or("")
    }
}

impl OptionsMutator for VariablesMapExtractingVisitor<'_> {
    fn visit_group_start(&mut self, _name: &str, used: bool) -> bool {
        used
    }

    fn visit_group_end(&mut self) {}

    fn visit_value_metadata(&mut self, name: &str, used: bool) -> bool {
        if used {
            self._current_name = Some(name.to_string());
            true
        } else {
            self._current_name = None;
            false
        }
    }

    fn visit_value_switch(&mut self) -> Result<bool, ExceptionPtr> {
        Ok(self._values.count(self.name()) != 0)
    }

    fn visit_value_bool(&mut self, val: &mut bool) -> Result<bool, ExceptionPtr> {
        Ok(extract_value(self._values, self.name(), val))
    }

    fn visit_value_int(&mut self, val: &mut i32) -> Result<bool, ExceptionPtr> {
        Ok(extract_value(self._values, self.name(), val))
    }

    fn visit_value_unsigned(&mut self, val: &mut u32) -> Result<bool, ExceptionPtr> {
        Ok(extract_value(self._values, self.name(), val))
    }

    fn visit_value_float(&mut self, val: &mut f32) -> Result<bool, ExceptionPtr> {
        Ok(extract_value(self._values, self.name(), val))
    }

    fn visit_value_double(&mut self, val: &mut f64) -> Result<bool, ExceptionPtr> {
        Ok(extract_value(self._values, self.name(), val))
    }

    fn visit_value_string(&mut self, val: &mut String) -> Result<bool, ExceptionPtr> {
        Ok(extract_value(self._values, self.name(), val))
    }

    fn visit_value_unsigned_set(&mut self, val: &mut BTreeSet<u32>) -> Result<bool, ExceptionPtr> {
        let mut raw_val = String::new();
        if !extract_value(self._values, self.name(), &mut raw_val) {
            return Ok(false);
        }
        if let Some(parsed_cpu_set) = resource::parse_cpuset(raw_val.clone())? {
            *val = parsed_cpu_set;
            return Ok(true);
        }
        Err(ExceptionPtr::new(crate::engine::core::future::InvalidArgument::new(format!(
            "invalid value for option {}: failed to parse cpuset: {}",
            self.name(),
            raw_val
        ))))
    }

    fn visit_value_log_level(&mut self, val: &mut LogLevel) -> Result<bool, ExceptionPtr> {
        extract_enum_value(self._values, self.name(), val)
    }

    fn visit_value_logger_timestamp_style(
        &mut self,
        val: &mut LoggerTimestampStyle,
    ) -> Result<bool, ExceptionPtr> {
        extract_enum_value(self._values, self.name(), val)
    }

    fn visit_value_logger_ostream_type(
        &mut self,
        val: &mut LoggerOstreamType,
    ) -> Result<bool, ExceptionPtr> {
        extract_enum_value(self._values, self.name(), val)
    }

    fn visit_value_alloc_failure_kind(
        &mut self,
        val: &mut AllocFailureKind,
    ) -> Result<bool, ExceptionPtr> {
        extract_enum_value(self._values, self.name(), val)
    }

    fn visit_value_log_level_map(
        &mut self,
        val: &mut HashMap<Sstring, LogLevel>,
    ) -> Result<bool, ExceptionPtr> {
        let mut raw_val: Vec<String> = Vec::new();
        if !extract_value(self._values, self.name(), &mut raw_val) {
            return Ok(false);
        }
        for e in &raw_val {
            let mut err = None;
            // Stop at the first invalid level: later associations are not applied.
            log_cli::parse_map_associations(e, |k: String, v: String| {
                if err.is_some() {
                    return;
                }
                match log_cli::parse_log_level(&Sstring::from(v.as_str())) {
                    Ok(l) => {
                        val.insert(Sstring::from(k), l);
                    }
                    Err(x) => err = Some(x),
                }
            })?;
            if let Some(x) = err {
                return Err(x);
            }
        }
        Ok(!val.is_empty())
    }

    fn visit_selection_value(
        &mut self,
        candidates: &[String],
        selected_candidate: &mut usize,
    ) -> Result<bool, ExceptionPtr> {
        let mut candidate_name = String::new();
        if !extract_value(self._values, self.name(), &mut candidate_name) {
            return Ok(false);
        }
        match candidates.iter().position(|c| *c == candidate_name) {
            Some(i) => {
                *selected_candidate = i;
                Ok(true)
            }
            None => {
                Err(ExceptionPtr::new(crate::engine::core::future::InvalidArgument::new(format!(
                    "invalid value for option {}: selected candidate doesn't exist: {}",
                    self.name(),
                    candidate_name
                ))))
            }
        }
    }
}
