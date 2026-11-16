//! Configuring logging from the command line.
//!
//! [`Options`] is the option group the application template registers for logging
//! (`--default-log-level`, `--logger-log-level`, `--log-to-stdout`, ...). The parsing
//! functions, implemented in the logging module, are re-exported here; together they turn
//! the parsed options into logging settings applied on every shard.

use std::collections::HashMap;

use crate::engine::core::future::ExceptionPtr;
use crate::engine::core::sstring::Sstring;
use crate::engine::util::log::{LogLevel, LoggerOstreamType, LoggerTimestampStyle};
use crate::engine::util::program_options;

/// Functions that build logging settings from program options, parse levels and list
/// the registered loggers. Implemented in [`log::log_cli`](crate::engine::util::log::log_cli).
pub use crate::engine::util::log::log_cli::{
    extract_settings, extract_settings_from_vars, get_options_description, parse_log_level,
    parse_map_associations, print_available_loggers,
};

/// Map from logger name to log level.
pub type LogLevelMap = HashMap<Sstring, LogLevel>;

/// Logging configuration.
pub struct Options {
    /// The option group that holds the values below.
    pub group: program_options::OptionGroup,
    /// Default log level for log messages.
    ///
    /// Valid values are trace, debug, info, warn, error. Default: `info`.
    /// See [`LogLevel`].
    pub default_log_level: program_options::Value<LogLevel>,
    /// Map of logger name to log level.
    ///
    /// The format is `NAME0=LEVEL0[:NAME1=LEVEL1:...]`.
    /// Valid logger names can be queried with `--help-loggers`.
    /// This option can be specified multiple times.
    pub logger_log_level: program_options::Value<LogLevelMap>,
    /// Timestamp style for stdout logs. Default: `real`.
    /// See [`LoggerTimestampStyle`].
    pub logger_stdout_timestamps: program_options::Value<LoggerTimestampStyle>,
    /// Send log output to the output stream selected by
    /// [`logger_ostream_type`](Self::logger_ostream_type). Default: `true`.
    pub log_to_stdout: program_options::Value<bool>,
    /// The output stream log output is sent to. Default: `stderr`.
    /// See [`LoggerOstreamType`].
    pub logger_ostream_type: program_options::Value<LoggerOstreamType>,
    /// Send log output to syslog. Default: `false`.
    pub log_to_syslog: program_options::Value<bool>,
    /// Print a colored tag prefix in log messages sent to the output stream.
    /// Default: `true`.
    pub log_with_color: program_options::Value<bool>,
}

impl std::ops::Deref for Options {
    type Target = program_options::OptionGroup;
    fn deref(&self) -> &program_options::OptionGroup {
        &self.group
    }
}

impl std::ops::DerefMut for Options {
    fn deref_mut(&mut self) -> &mut program_options::OptionGroup {
        &mut self.group
    }
}

/// Parses each `logger -> level` association in `levels` and appends the resulting pairs
/// to `out`.
///
/// # Errors
///
/// Returns a runtime error for an invalid log level; pairs before it have already been
/// appended.
pub fn parse_logger_levels<E: Extend<(Sstring, LogLevel)>>(
    levels: &program_options::StringMap,
    out: &mut E,
) -> Result<(), ExceptionPtr> {
    for (k, v) in levels.iter() {
        let lvl = parse_log_level(v)?;
        out.extend(std::iter::once((k.clone(), lvl)));
    }
    Ok(())
}
