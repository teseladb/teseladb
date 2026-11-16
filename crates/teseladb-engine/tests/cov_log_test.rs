//! Tests of the loggers outside a reactor: levels and their names, the message format with
//! and without colour and timestamps, rate limiting, runtime format strings and their errors,
//! log writers, the registry, the logging settings and their command-line options, and the
//! formatting of exceptions.
#![cfg(target_os = "linux")]

use std::fmt::Write as _;
use std::io::Write;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use teseladb::engine::core::future::{ExceptionPtr, NestedException, RuntimeError};
use teseladb::engine::core::lowres_clock::LowresClock;
use teseladb::engine::core::program_options::bpo;
use teseladb::engine::core::sstring::Sstring;
use teseladb::engine::util::log::log_cli::{
    extract_settings, extract_settings_from_vars, get_options_description, parse_log_level,
    parse_map_associations, print_available_loggers,
};
use teseladb::engine::util::log::{
    DisplayExceptionPtr, FormatInfo, LambdaLogWriter, LogLevel, Logger, LoggerFor,
    LoggerOstreamType, LoggerTimestampStyle, LoggingSettings, Ostream, RateLimit,
    apply_logging_settings, format_exception, format_system_error, global_logger_registry,
    level_name, pretty_type_name, validate_logger_ostream_type, validate_logger_timestamp_style,
};
use teseladb::engine::util::log_cli::{Options, parse_logger_levels};
use teseladb::engine::util::log_impl::internal::format_to;
use teseladb::engine::util::program_options::StringMap;
use teseladb::{log_debug, log_error, log_info, log_trace, log_warn};

/// The loggers' output is process-wide: the tests that capture it run one at a time.
static OUTPUT: Mutex<()> = Mutex::new(());

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Capture {
    fn take(&self) -> String {
        String::from_utf8(std::mem::take(&mut *self.0.lock().unwrap())).unwrap()
    }
}

/// Sends the log output to a capture, without colour or timestamps, until dropped.
struct Captured {
    out: Capture,
    _lock: MutexGuard<'static, ()>,
}

impl Captured {
    fn new() -> Self {
        let lock = OUTPUT.lock().unwrap_or_else(|e| e.into_inner());
        let c = Captured { out: Capture::default(), _lock: lock };
        c.apply(&settings(LoggerTimestampStyle::None, false));
        c
    }

    /// Applies `s`, which selects an output stream, and sends the output back to the capture.
    fn apply(&self, s: &LoggingSettings) {
        apply_logging_settings(s).unwrap();
        Logger::set_ostream(Ostream::Custom(Box::new(self.out.clone())));
    }
}

impl Drop for Captured {
    fn drop(&mut self) {
        Logger::set_ostream(Ostream::Cerr);
        Logger::set_ostream_enabled(true);
        Logger::set_syslog_enabled(false);
    }
}

fn settings(ts: LoggerTimestampStyle, color: bool) -> LoggingSettings {
    LoggingSettings {
        logger_levels: Default::default(),
        default_level: LogLevel::Info,
        stdout_enabled: true,
        syslog_enabled: false,
        with_color: color,
        stdout_timestamp_style: ts,
        logger_ostream: LoggerOstreamType::Cerr,
    }
}

#[test]
fn levels_have_names_and_parse_back() {
    for (level, name) in [
        (LogLevel::Error, "error"),
        (LogLevel::Warn, "warn"),
        (LogLevel::Info, "info"),
        (LogLevel::Debug, "debug"),
        (LogLevel::Trace, "trace"),
    ] {
        assert_eq!(level.to_string(), name);
        assert_eq!(level_name(level).as_str(), name);
        assert_eq!(name.parse::<LogLevel>(), Ok(level));
        assert_eq!(parse_log_level(&name.into()).unwrap(), level);
    }
    assert!("verbose".parse::<LogLevel>().is_err());
    let err = parse_log_level(&"verbose".into()).unwrap_err();
    assert!(err.to_string().contains("Unknown log level 'verbose'"), "{err}");
}

#[test]
fn option_validators_accept_the_known_words() {
    for (word, style) in [
        ("none", LoggerTimestampStyle::None),
        ("boot", LoggerTimestampStyle::Boot),
        ("real", LoggerTimestampStyle::Real),
    ] {
        let mut v = None;
        validate_logger_timestamp_style(&mut v, &[word.to_string()]).unwrap();
        assert_eq!(v, Some(style));
        assert_eq!(style.to_string(), word);
        // A second occurrence is refused.
        assert!(validate_logger_timestamp_style(&mut v, &[word.to_string()]).is_err());
    }
    let mut v = None;
    assert!(validate_logger_timestamp_style(&mut v, &["monotonic".to_string()]).is_err());

    for (word, t) in [
        ("none", LoggerOstreamType::None),
        ("stdout", LoggerOstreamType::Cout),
        ("stderr", LoggerOstreamType::Cerr),
    ] {
        let mut v = None;
        validate_logger_ostream_type(&mut v, &[word.to_string()]).unwrap();
        assert_eq!(v, Some(t));
        assert_eq!(t.to_string(), word);
    }
    let mut v = None;
    assert!(validate_logger_ostream_type(&mut v, &["file".to_string()]).is_err());
}

#[test]
fn messages_have_level_name_and_text() {
    let cap = Captured::new();
    let log = Logger::new("cov_format").unwrap();
    assert_eq!(log.name().as_str(), "cov_format");
    assert_eq!(log.level(), LogLevel::Info);
    log_info!(log, "hello {}", 42);
    log_warn!(log, "careful");
    log_error!(log, "broken");
    // Below the level: nothing.
    log_debug!(log, "hidden");
    log_trace!(log, "hidden");
    assert_eq!(
        cap.out.take(),
        "INFO   cov_format - hello 42\nWARN   cov_format - careful\nERROR  cov_format - broken\n"
    );

    log.set_level(LogLevel::Trace);
    assert!(log.is_enabled(LogLevel::Trace));
    log.debug(FormatInfo::new("d"), format_args!("d{}", 1));
    log.trace(FormatInfo::new("t"), format_args!("t"));
    log.info0(FormatInfo::new("i0"), format_args!("only on shard zero"));
    log.error(FormatInfo::empty(), format_args!("e"));
    log.warn(FormatInfo::default(), format_args!("w"));
    log.info(FormatInfo::new("i"), format_args!("i"));
    let out = cap.out.take();
    assert_eq!(
        out,
        "DEBUG  cov_format - d1\nTRACE  cov_format - t\nINFO   cov_format - only on shard zero\n\
         ERROR  cov_format - e\nWARN   cov_format - w\nINFO   cov_format - i\n"
    );
    assert!(log.is_shard_zero());

    // Disabled output writes nothing.
    Logger::set_ostream_enabled(false);
    log_error!(log, "dropped");
    Logger::set_ostream_enabled(true);
    assert!(cap.out.take().is_empty());
}

#[test]
fn colour_and_timestamps_decorate_the_line() {
    let cap = Captured::new();
    let log = Logger::new("cov_decor").unwrap();
    cap.apply(&settings(LoggerTimestampStyle::None, true));
    log_error!(log, "red");
    log_warn!(log, "yellow");
    log.set_level(LogLevel::Trace);
    log_debug!(log, "green");
    log_trace!(log, "blue");
    log_info!(log, "white");
    let out = cap.out.take();
    assert!(out.contains("\x1b[31mERROR\x1b[0m  cov_decor - red"), "{out:?}");
    assert!(out.contains("\x1b[33mWARN \x1b[0m"));
    assert!(out.contains("\x1b[32mDEBUG\x1b[0m"));
    assert!(out.contains("\x1b[34mTRACE\x1b[0m"));
    assert!(out.contains("\x1b[37mINFO \x1b[0m"));

    cap.apply(&settings(LoggerTimestampStyle::Real, false));
    log_error!(log, "dated");
    let out = cap.out.take();
    // "ERROR YYYY-MM-DD HH:MM:SS,mmm cov_decor - dated"
    let rest = out.strip_prefix("ERROR ").unwrap();
    assert_eq!(&rest[4..5], "-", "{out:?}");
    assert_eq!(&rest[19..20], ",");
    assert!(rest.ends_with(" cov_decor - dated\n"));

    cap.apply(&settings(LoggerTimestampStyle::Boot, false));
    log_error!(log, "since boot");
    let out = cap.out.take();
    let ts = out.strip_prefix("ERROR ").unwrap().split(' ').find(|s| !s.is_empty()).unwrap();
    assert!(ts.parse::<f64>().is_ok(), "{out:?}");
    assert!(out.ends_with(" cov_decor - since boot\n"));
}

#[test]
fn rate_limit_drops_and_counts_messages() {
    let cap = Captured::new();
    let log = Logger::new("cov_rate").unwrap();
    // Outside a reactor the low-resolution clock moves only when updated.
    LowresClock::update();
    let mut rl = RateLimit::new(Duration::from_millis(20));
    for i in 0..5 {
        log.log_rate_limited(LogLevel::Info, &mut rl, FormatInfo::new("m"), format_args!("m{i}"));
    }
    assert_eq!(cap.out.take(), "INFO   cov_rate - m0\n");
    std::thread::sleep(Duration::from_millis(40));
    LowresClock::update();
    // The next message after the interval reports the four dropped ones.
    log.log_rate_limited(LogLevel::Info, &mut rl, FormatInfo::new("x"), format_args!("x"));
    assert_eq!(cap.out.take(), "INFO   cov_rate - (rate limiting dropped 4 similar messages) x\n");
    // A disabled level neither prints nor counts.
    log.log_rate_limited(LogLevel::Debug, &mut rl, FormatInfo::new("d"), format_args!("d"));
    assert!(cap.out.take().is_empty());

    let mut writer = LambdaLogWriter::new(|it| format_to(it, format_args!("written")));
    let mut rl2 = RateLimit::new(Duration::from_millis(20));
    log.log_writer(LogLevel::Warn, &mut rl2, &mut writer, FormatInfo::new("w"));
    assert_eq!(cap.out.take(), "WARN   cov_rate - written\n");
    log.log_writer(LogLevel::Warn, &mut rl2, &mut writer, FormatInfo::new("w"));
    assert!(cap.out.take().is_empty(), "limited");
    std::thread::sleep(Duration::from_millis(40));
    LowresClock::update();
    log.log_writer(LogLevel::Warn, &mut rl2, &mut writer, FormatInfo::new("w"));
    assert_eq!(
        cap.out.take(),
        "WARN   cov_rate - (rate limiting dropped 1 similar messages) written\n"
    );
    // Disabled level: nothing, and the limit is not consumed.
    log.log_writer(LogLevel::Debug, &mut rl2, &mut writer, FormatInfo::new("w"));
    assert!(cap.out.take().is_empty());
}

#[test]
fn runtime_formats_substitute_or_report_errors() {
    let cap = Captured::new();
    let log = Logger::new("cov_runtime").unwrap();
    let a: &dyn std::fmt::Display = &1;
    let b: &dyn std::fmt::Display = &"two";
    log.log_runtime(LogLevel::Info, FormatInfo::new("{} and {}"), &[a, b]);
    log.log_runtime(LogLevel::Info, FormatInfo::new("{1} then {0}"), &[a, b]);
    log.log_runtime(LogLevel::Info, FormatInfo::new("{{literal}} {:}"), &[a]);
    log.error_runtime(FormatInfo::new("no arguments"));
    let out = cap.out.take();
    assert_eq!(
        out,
        "INFO   cov_runtime - 1 and two\nINFO   cov_runtime - two then 1\n\
         INFO   cov_runtime - {literal} 1\nERROR  cov_runtime - no arguments\n"
    );

    for (fmt, what) in [
        ("{", "invalid format string"),
        ("{x}", "invalid format string"),
        ("{:>5}", "unsupported format specifier"),
        ("{} {0}", "cannot switch from automatic to manual argument indexing"),
        ("{0} {}", "cannot switch from manual to automatic argument indexing"),
        ("{2}", "argument not found"),
        ("a } b", "unmatched '}' in format string"),
    ] {
        log.log_runtime(LogLevel::Warn, FormatInfo::new(fmt), &[a]);
        let out = cap.out.take();
        assert!(out.starts_with("ERROR  cov_runtime - "), "{fmt}: {out}");
        assert!(out.contains("failed to log message"), "{out}");
        assert!(out.contains(&format!("fmt='{fmt}'")), "{out}");
        assert!(out.contains(what), "{fmt}: {out}");
    }
    // A disabled level does not even parse the format.
    log.log_runtime(LogLevel::Trace, FormatInfo::new("{"), &[]);
    assert!(cap.out.take().is_empty());
}

#[test]
fn a_panicking_argument_is_reported_as_a_logging_failure() {
    struct Bad;
    impl std::fmt::Display for Bad {
        fn fmt(&self, _: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            panic!("display failed");
        }
    }
    let cap = Captured::new();
    let log = Logger::new("cov_panic").unwrap();
    log_info!(log, "value {}", Bad);
    let out = cap.out.take();
    assert!(out.starts_with("ERROR  cov_panic - "), "{out}");
    assert!(out.contains("failed to log message: fmt='value {}'"), "{out}");
}

#[test]
fn registry_tracks_loggers_by_name() {
    let reg = global_logger_registry();
    let a = Logger::new("cov_reg_a").unwrap();
    assert!(Logger::new("cov_reg_a").is_err(), "a name registers once");
    assert!(reg.get_all_logger_names().contains(&Sstring::from("cov_reg_a")));
    assert_eq!(reg.get_logger_level(&"cov_reg_a".into()), Some(LogLevel::Info));
    reg.set_logger_level(&"cov_reg_a".into(), LogLevel::Debug).unwrap();
    assert_eq!(a.level(), LogLevel::Debug);
    assert!(reg.set_logger_level(&"cov_reg_missing".into(), LogLevel::Debug).is_err());
    assert_eq!(reg.get_logger_level(&"cov_reg_missing".into()), None);

    // A moved logger keeps its name and level; the old box no longer unregisters it.
    let b = Logger::moved_from(a);
    assert_eq!(b.name().as_str(), "cov_reg_a");
    assert_eq!(b.level(), LogLevel::Debug);
    reg.set_logger_level(&"cov_reg_a".into(), LogLevel::Warn).unwrap();
    assert_eq!(b.level(), LogLevel::Warn);
    drop(b);
    assert_eq!(reg.get_logger_level(&"cov_reg_a".into()), None);

    let mut listing = String::new();
    let c = Logger::new("cov_reg_c").unwrap();
    print_available_loggers(&mut listing).unwrap();
    assert!(listing.starts_with("Available loggers:\n"));
    assert!(listing.contains("    cov_reg_c\n"));
    drop(c);

    struct Marker;
    let typed = LoggerFor::<Marker>::new().unwrap();
    assert_eq!(typed.name(), &pretty_type_name::<Marker>());
    assert!(typed.name().as_str().ends_with("Marker"));
}

#[test]
fn logging_settings_set_levels_and_reject_unknown_loggers() {
    let _lock = OUTPUT.lock().unwrap_or_else(|e| e.into_inner());
    let a = Logger::new("cov_settings").unwrap();
    let mut s = settings(LoggerTimestampStyle::None, false);
    s.default_level = LogLevel::Warn;
    s.logger_levels.insert("cov_settings".into(), LogLevel::Trace);
    apply_logging_settings(&s).unwrap();
    assert_eq!(a.level(), LogLevel::Trace);

    s.logger_levels.insert("cov_settings_missing".into(), LogLevel::Trace);
    let err = apply_logging_settings(&s).unwrap_err();
    assert!(err.to_string().contains("Unknown logger 'cov_settings_missing'"), "{err}");

    let mut quiet = settings(LoggerTimestampStyle::None, false);
    quiet.stdout_enabled = false;
    apply_logging_settings(&quiet).unwrap();
    let mut out = settings(LoggerTimestampStyle::None, false);
    out.logger_ostream = LoggerOstreamType::Cout;
    apply_logging_settings(&out).unwrap();
    out.logger_ostream = LoggerOstreamType::None;
    apply_logging_settings(&out).unwrap();
    apply_logging_settings(&settings(LoggerTimestampStyle::None, false)).unwrap();
    global_logger_registry().set_all_loggers_level(LogLevel::Info);
    assert_eq!(a.level(), LogLevel::Info);
}

#[test]
fn command_line_options_become_settings() {
    let desc = get_options_description();
    let text = desc.to_string();
    for name in [
        "default-log-level",
        "logger-log-level",
        "logger-stdout-timestamps",
        "log-to-stdout",
        "logger-ostream-type",
        "log-to-syslog",
        "log-with-color",
    ] {
        assert!(text.contains(name), "{name} missing from {text}");
    }

    let args = [
        "--default-log-level",
        "debug",
        "--logger-log-level",
        "io=trace:http=warn",
        "--logger-stdout-timestamps",
        "boot",
        "--logger-ostream-type",
        "stdout",
        "--log-to-syslog",
        "true",
        "--log-with-color",
        "false",
    ];
    let parsed = bpo::command_line_parser(args.iter().map(|s| s.to_string()).collect())
        .options(&desc)
        .run()
        .unwrap();
    let mut vm = bpo::VariablesMap::new();
    bpo::store(parsed, &mut vm).unwrap();
    bpo::notify(&mut vm).unwrap();
    let s = extract_settings_from_vars(&vm).unwrap();
    assert_eq!(s.default_level, LogLevel::Debug);
    assert_eq!(s.logger_levels.get(&Sstring::from("io")), Some(&LogLevel::Trace));
    assert_eq!(s.logger_levels.get(&Sstring::from("http")), Some(&LogLevel::Warn));
    assert_eq!(s.stdout_timestamp_style, LoggerTimestampStyle::Boot);
    assert_eq!(s.logger_ostream, LoggerOstreamType::Cout);
    assert!(s.syslog_enabled);
    assert!(!s.with_color);
    assert!(s.stdout_enabled);

    // The defaults.
    let opts = Options::new();
    let d = extract_settings(&opts);
    assert_eq!(d.default_level, LogLevel::Info);
    assert!(d.logger_levels.is_empty());
    assert_eq!(d.stdout_timestamp_style, LoggerTimestampStyle::Real);
    assert_eq!(d.logger_ostream, LoggerOstreamType::Cerr);
    assert!(!d.syslog_enabled);
    let defaulted = Options::default();
    assert!(*defaulted.log_to_stdout.get_value());
}

#[test]
fn level_maps_parse_from_associations() {
    let mut pairs = Vec::new();
    parse_map_associations("a=1:b=two", |k, v| pairs.push((k, v))).unwrap();
    assert_eq!(pairs, vec![("a".into(), "1".into()), ("b".into(), "two".into())]);
    parse_map_associations("", |_, _| panic!("no pairs")).unwrap();
    assert!(parse_map_associations("a=1:broken", |_, _| {}).is_err());

    let mut map = StringMap::new();
    map.emplace("io".into(), "debug".into());
    let mut out: Vec<(Sstring, LogLevel)> = Vec::new();
    parse_logger_levels(&map, &mut out).unwrap();
    assert_eq!(out, vec![(Sstring::from("io"), LogLevel::Debug)]);
    map.emplace("http".into(), "loud".into());
    assert!(parse_logger_levels(&map, &mut out).is_err());
}

#[test]
fn exceptions_format_with_their_type_and_message() {
    let e = ExceptionPtr::new(RuntimeError("boom".to_string()));
    let s = DisplayExceptionPtr(Some(&e)).to_string();
    assert!(s.ends_with("RuntimeError (boom)"), "{s}");
    assert_eq!(e.to_string(), s);
    assert_eq!(DisplayExceptionPtr(None).to_string(), "<no exception>");

    let io = ExceptionPtr::new(std::io::Error::from_raw_os_error(2));
    let s = io.to_string();
    assert!(s.contains("(error system:2, "), "{s}");
    let custom = ExceptionPtr::new(std::io::Error::other("custom io"));
    assert!(custom.to_string().contains("(custom io)"), "{custom}");

    let nested = ExceptionPtr::new(NestedException {
        inner: ExceptionPtr::new(RuntimeError("inner".to_string())),
        outer: ExceptionPtr::new(RuntimeError("outer".to_string())),
    });
    let s = nested.to_string();
    assert!(s.contains("(while cleaning up after "), "{s}");
    assert!(s.contains("(inner)") && s.contains("(outer)"), "{s}");

    let mut out = String::new();
    format_exception(&mut out, "T", "what").unwrap();
    assert_eq!(out, "T (what)");
    out.clear();
    format_system_error(&mut out, "E", &"system:5", "io error").unwrap();
    assert_eq!(out, "E (error system:5, io error)");
    let _ = write!(out, "");
}
