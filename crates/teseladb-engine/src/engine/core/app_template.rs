//! The standard skeleton of an engine application.
//!
//! [`AppTemplate`] parses the command line and the configuration files with the options of every
//! subsystem (reactor, SMP, metrics, collectd, logging) plus the application's own, starts one
//! reactor per shard, runs the application's function on shard 0 once the reactors have started,
//! and returns the process exit code when the reactors stop. [`Config`] is the simple way to
//! configure it; [`EngineOptions`] exposes every option group.
//!
//! The configuration files `~/.config/engine/engine.conf` and `~/.config/engine/io.conf` are read
//! by default; [`AppTemplate::set_configuration_reader`] replaces that reader.

use std::io::Write as _;
use std::pin::Pin;

use crate::engine::core::alien;
use crate::engine::core::future::{
    ExceptionPtr, Future, Promise, futurize_invoke, make_exception_future, make_ready_future,
};
use crate::engine::core::metrics;
use crate::engine::core::program_options::bpo;
use crate::engine::core::program_options::{
    OptionsDescriptionBuildingVisitor, VariablesMapExtractingVisitor,
};
use crate::engine::core::reactor::{Reactor, ReactorOptions, engine, internal::at_exit};
use crate::engine::core::scollectd;
use crate::engine::core::shared_ptr::make_lw_shared;
use crate::engine::core::smp::Smp;
use crate::engine::core::smp_options::SmpOptions;
use crate::engine::core::sstring::Sstring;
use crate::engine::util::log::apply_logging_settings;
use crate::engine::util::log_cli;
use crate::engine::util::noncopyable_function::NoncopyableFunction;
use crate::engine::util::program_options::OptionGroup;
use std::sync::Arc;

/// Standard output as a `fmt::Write`, for printers that take a formatter.
struct StdoutWriter<'a>(std::io::StdoutLock<'a>);

impl std::fmt::Write for StdoutWriter<'_> {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        self.0.write_all(s.as_bytes()).map_err(|_| std::fmt::Error)
    }
}

/// The configuration of an application: the simple form of [`EngineOptions`].
#[derive(Debug, Clone)]
pub struct Config {
    /// The name of the application.
    ///
    /// Used in the `--help` output to tell the command-line options registered by the
    /// application from those of the engine and its subsystems.
    pub name: Sstring,
    /// The description of the application.
    ///
    /// Printed at the top of the `--help` output. Lines should be hard-wrapped at 80 characters.
    pub description: Sstring,
    /// Default task quota: how long the reactor runs tasks before it polls for I/O again.
    pub default_task_quota: std::time::Duration,
    /// Handle `SIGINT`/`SIGTERM` by stopping the reactor.
    ///
    /// When true, the engine installs handlers for `SIGINT` and `SIGTERM` that stop the reactor,
    /// which then runs the callbacks registered with `at_exit`.
    ///
    /// When false, no handlers are installed and the default behaviour (terminate the program) is
    /// kept; the application can install its own with the reactor's `handle_signal`.
    pub auto_handle_sigint_sigterm: bool,
    /// Default number of linux-aio I/O control blocks reserved for networking, which bounds the
    /// number of sockets a shard can handle.
    pub max_networking_aio_io_control_blocks: u32,
    /// The amount of memory that should not be used by the engine allocator,
    /// additional to the amount of memory already reserved for the OS.
    /// This can be used when the application allocates some of its memory using the
    /// engine allocator, and some using the system allocator, in particular when it
    /// maps memory with `mmap(2)` and `MAP_ANONYMOUS`, which the engine allocator does not
    /// intercept.
    pub reserve_additional_memory_per_shard: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            name: "App".into(),
            description: "".into(),
            default_task_quota: std::time::Duration::from_micros(500),
            auto_handle_sigint_sigterm: true,
            max_networking_aio_io_control_blocks: 10000,
            reserve_additional_memory_per_shard: 0,
        }
    }
}

/// All the option groups of an engine application, under one root group named `"engine"`.
///
/// Build it with [`EngineOptions::new`], adjust defaults, and hand it to
/// [`AppTemplate::with_options`], which links the groups in place.
pub struct EngineOptions {
    /// The root option group ("engine").
    pub group: OptionGroup,
    /// The name of the application.
    ///
    /// Used in the `--help` output to tell the command-line options registered by the
    /// application from those of the engine and its subsystems.
    pub name: Sstring,
    /// The description of the application.
    ///
    /// Printed at the top of the `--help` output. Lines should be hard-wrapped at 80 characters.
    pub description: Sstring,
    /// Handle `SIGINT`/`SIGTERM` by stopping the reactor.
    ///
    /// When true, the engine installs handlers for `SIGINT` and `SIGTERM` that stop the reactor,
    /// which then runs the callbacks registered with `at_exit`.
    ///
    /// When false, no handlers are installed and the default behaviour (terminate the program) is
    /// kept; the application can install its own with the reactor's `handle_signal`.
    pub auto_handle_sigint_sigterm: bool,
    /// Configuration options for the reactor.
    pub reactor_opts: ReactorOptions,
    /// Configuration for the metrics sub-system.
    pub metrics_opts: metrics::Options,
    /// Configuration options of the multicore layer.
    pub smp_opts: SmpOptions,
    /// Configuration for the collectd metrics exporter.
    pub scollectd_opts: scollectd::Options,
    /// Configuration for the logging sub-system.
    pub log_opts: log_cli::Options,
}

impl EngineOptions {
    /// The default options, not yet linked: [`AppTemplate::with_options`] places them and links
    /// every subsystem's group under "engine" with [`EngineOptions::link`].
    pub fn new() -> Self {
        Self {
            group: OptionGroup::new("engine"),
            name: "App".into(),
            description: "".into(),
            auto_handle_sigint_sigterm: true,
            reactor_opts: ReactorOptions::new(),
            metrics_opts: metrics::Options::new(),
            smp_opts: SmpOptions::new(),
            scollectd_opts: scollectd::Options::new(),
            log_opts: log_cli::Options::new(),
        }
    }

    /// Links the root group (it has no parent) and each subsystem's group under it, in field
    /// order. The options must not move afterwards: the groups are intrusive lists.
    pub fn link(self: Pin<&mut Self>) {
        crate::link_option_group!(
            self,
            None,
            group,
            [reactor_opts],
            [metrics_opts],
            [smp_opts],
            [scollectd_opts],
            [log_opts]
        );
    }
}

impl Default for EngineOptions {
    fn default() -> Self {
        Self::new()
    }
}

/// A function that reads configuration files into the parsed option map.
///
/// It runs after the command line has been parsed; values already present take precedence.
pub type ConfigurationReader = Box<dyn FnMut(&mut bpo::VariablesMap) -> Result<(), bpo::Error>>;

/// A positional option of the application.
pub struct PositionalOption {
    /// The option name.
    pub name: &'static str,
    /// How its tokens are parsed.
    pub value_semantic: Box<dyn bpo::ValueSemantic>,
    /// Its help text.
    pub help: &'static str,
    /// How many tokens it takes (-1: all the remaining ones).
    pub max_count: i32,
}

/// Builds the full options from the simple configuration.
fn engine_options_from_config(cfg: Config) -> EngineOptions {
    let mut opts = EngineOptions::new();
    opts.name = cfg.name;
    opts.description = cfg.description;
    opts.auto_handle_sigint_sigterm = cfg.auto_handle_sigint_sigterm;
    opts.reactor_opts
        .task_quota_ms
        .set_default_value(cfg.default_task_quota.as_secs_f64() * 1000.0);
    opts.reactor_opts
        .max_networking_io_control_blocks
        .set_default_value(cfg.max_networking_aio_io_control_blocks);
    opts.smp_opts.reserve_additional_memory_per_shard = cfg.reserve_additional_memory_per_shard;
    opts
}

/// Registers the engine's own loggers. Each one is a static created on first use, where the
/// original's are global objects registered before `main`: without this, the logging options
/// (`--logger-log-level engine=debug`) and `--help-loggers` would not know a logger that has
/// not logged yet.
fn register_engine_loggers() {
    use std::sync::LazyLock;
    let _ = crate::engine::core::reactor::engine_logger();
    LazyLock::force(&crate::engine::core::memory::ENGINE_MEMORY_LOGGER);
    LazyLock::force(&crate::engine::core::io_queue::IO_LOG);
    LazyLock::force(&scollectd::LOGGER);
    #[cfg(not(no_exception_intercept))]
    LazyLock::force(&crate::engine::core::exception_hacks::EXCEPTION_LOGGER);
    LazyLock::force(&crate::engine::util::alloc_failure_injector::memory::LOG);
    LazyLock::force(&crate::engine::http::httpd::HLOGGER);
    LazyLock::force(&crate::engine::http::client::HTTP_LOG);
    LazyLock::force(&crate::engine::websocket::common::websocket::WEBSOCKET_LOGGER);
    #[cfg(feature = "dns")]
    let _ = crate::engine::net::dns::dns_log();
}

/// Runs an engine application: parses the command line and the configuration files, starts the
/// reactors and runs the application's function.
///
/// Field order matters: fields drop in declaration order, and `_smp` must be dropped before
/// `_alien`, which `Smp` points to.
pub struct AppTemplate {
    _conf_reader: ConfReader,
    _configuration: Option<bpo::VariablesMap>,
    _pos_opts: bpo::PositionalOptionsDescription,
    _opts_conf_file: bpo::OptionsDescription,
    _engine_opts: bpo::OptionsDescription,
    _app_opts: bpo::OptionsDescription,
    /// Boxed and pinned: the option groups are linked in place and must not move with the
    /// application.
    _opts: Pin<Box<EngineOptions>>,
    // Reactor destruction is asynchronous, so the last reactor to go drops the SMP instance;
    // hence the atomic reference count.
    _smp: Arc<Smp>,
    // Boxed so that its address, which `Smp` keeps, is stable.
    _alien: Box<alien::Instance>,
}

/// The configuration reader in use: the default one (which reads the files with the options of
/// `_opts_conf_file`) or the one set by the application.
enum ConfReader {
    Default,
    Custom(ConfigurationReader),
}

impl AppTemplate {
    /// Creates an application with `opts`, links its option groups, and registers its alien
    /// instance as the process default if none is set yet.
    ///
    /// Boxed, because the option groups and the alien instance must keep their addresses.
    pub fn with_options(opts: EngineOptions) -> Box<Self> {
        let mut alien = Box::new(alien::Instance { _qs: None });
        let smp = Arc::new(Smp::new(&mut alien));
        let app_caption = format!("{} options", opts.name);
        let mut opts = Box::pin(opts);
        opts.as_mut().link();
        let mut this = Box::new(Self {
            _alien: alien,
            _smp: smp,
            _opts: opts,
            _app_opts: bpo::OptionsDescription::new(&app_caption),
            _engine_opts: bpo::OptionsDescription::default(),
            _opts_conf_file: bpo::OptionsDescription::default(),
            _pos_opts: bpo::PositionalOptionsDescription::default(),
            _configuration: None,
            _conf_reader: ConfReader::Default,
        });
        if alien::internal::default_instance().is_null() {
            let inst: *mut alien::Instance = &mut *this._alien;
            alien::internal::DEFAULT_INSTANCE.store(inst, std::sync::atomic::Ordering::Release);
        }
        this._app_opts.add_options().flag("help,h", "show help message");
        this._app_opts.add_options().flag("help-engine", "show help message about engine options");
        this._app_opts.add_options().flag("help-loggers", "print a list of logger names and exit");

        {
            let mut visitor = OptionsDescriptionBuildingVisitor::new();
            this._opts.group.describe(&mut visitor);
            this._opts_conf_file.add(visitor.get_options_description());
        }

        let conf = this._opts_conf_file.clone();
        this._engine_opts.add(conf);
        this
    }

    /// Creates an application configured by `cfg`; see [`with_options`](Self::with_options).
    pub fn new(cfg: Config) -> Box<Self> {
        Self::with_options(engine_options_from_config(cfg))
    }

    /// Returns the engine options.
    pub fn options(&self) -> &EngineOptions {
        &self._opts
    }

    /// Returns the engine options for modification (they are filled in from the parsed command
    /// line).
    fn options_mut(&mut self) -> &mut EngineOptions {
        // SAFETY: the returned reference only modifies the options in place; nothing moves them
        // out of their pinned box.
        unsafe { self._opts.as_mut().get_unchecked_mut() }
    }

    /// The default configuration reader: reads `$HOME/.config/engine/engine.conf` and
    /// `$HOME/.config/engine/io.conf`, when they exist, with the options of `opts_conf_file`.
    ///
    /// # Errors
    /// Fails if a file that exists cannot be parsed or holds an invalid value.
    fn default_configuration_reader(
        opts_conf_file: &bpo::OptionsDescription,
        configuration: &mut bpo::VariablesMap,
    ) -> Result<(), bpo::Error> {
        if let Ok(home) = std::env::var("HOME") {
            if let Ok(ifs) = std::fs::File::open(format!("{}/.config/engine/engine.conf", home)) {
                bpo::store(
                    bpo::parse_config_file(std::io::BufReader::new(ifs), opts_conf_file)?,
                    configuration,
                )?;
            }
            if let Ok(ifs_io) = std::fs::File::open(format!("{}/.config/engine/io.conf", home)) {
                bpo::store(
                    bpo::parse_config_file(std::io::BufReader::new(ifs_io), opts_conf_file)?,
                    configuration,
                )?;
            }
        }
        Ok(())
    }

    /// Replaces the reader of the configuration files.
    pub fn set_configuration_reader(&mut self, conf_reader: ConfigurationReader) {
        self._conf_reader = ConfReader::Custom(conf_reader);
    }

    /// Returns the description of the application's own options, to add to it.
    pub fn get_options_description(&mut self) -> &mut bpo::OptionsDescription {
        &mut self._app_opts
    }

    /// Returns the description of the options read from the configuration files.
    pub fn get_conf_file_options_description(&mut self) -> &mut bpo::OptionsDescription {
        &mut self._opts_conf_file
    }

    /// Starts adding options to the application's options description.
    pub fn add_options(&mut self) -> bpo::EasyInit<'_> {
        self._app_opts.add_options()
    }

    /// Adds positional options: each becomes an application option and takes its place in the
    /// positional order.
    pub fn add_positional_options(&mut self, options: Vec<PositionalOption>) {
        for o in options {
            self._app_opts.add_option(bpo::OptionDescription::new(
                o.name,
                Some(o.value_semantic),
                o.help,
            ));
            self._pos_opts.add(o.name, o.max_count);
        }
    }

    /// Returns the parsed configuration (filled once `run` has parsed the command line; empty
    /// before).
    pub fn configuration(&mut self) -> &mut bpo::VariablesMap {
        self._configuration.get_or_insert_with(bpo::VariablesMap::new)
    }

    /// Returns the alien instance, through which threads that are not shards can send work to
    /// the shards (see [`alien`]).
    pub fn alien(&mut self) -> &mut alien::Instance {
        &mut self._alien
    }

    /// Runs `func` on shard 0 and stops the application when the future it returns resolves;
    /// returns the exit code that future resolved to.
    ///
    /// `args` is the command line, program name first. Returns 0 after `--help`, 2 if the
    /// command line cannot be parsed, and 1 if the options are invalid or the engine cannot start.
    pub fn run_with_exit_code<F>(&mut self, args: Vec<String>, mut func: F) -> i32
    where
        F: FnMut() -> Future<i32> + 'static,
    {
        self.run_deprecated(
            args,
            Box::new(move || {
                let func_done = make_lw_shared(Promise::<()>::new());
                let fd = func_done.clone();
                at_exit(NoncopyableFunction::<fn() -> Future<()>>::new(move || {
                    // SAFETY: the promise is only used on shard 0, by this hook and the `finally`
                    // below, each for the duration of one call; no other reference is live.
                    unsafe { fd.get_mut_unchecked() }.get_future()
                }));
                // The future is not awaited: func's exit code reaches the reactor through
                // Reactor::exit.
                let fd2 = func_done.clone();
                let _ = futurize_invoke(&mut func)
                    .finally(move || {
                        // SAFETY: as above.
                        unsafe { fd2.get_mut_unchecked() }.set_value(());
                    })
                    .then(|exit_code: i32| Reactor::exit(exit_code))
                    .or_terminate();
            }),
        )
    }

    /// Like [`run_with_exit_code`](Self::run_with_exit_code), for a function whose future has no
    /// value: the exit code is 0 when it resolves successfully.
    pub fn run<F>(&mut self, args: Vec<String>, mut func: F) -> i32
    where
        F: FnMut() -> Future<()> + 'static,
    {
        self.run_with_exit_code(args, move || func().then(|()| Ok::<i32, ExceptionPtr>(0)))
    }

    /// Parses the options, starts the reactors, runs `func` on shard 0 once they have started,
    /// and returns the exit code when the reactors stop.
    ///
    /// The application must stop the reactor itself. Exit codes: 0 after `--help`,
    /// `--help-engine` or `--help-loggers`; 2 for a command line that cannot be parsed; 1 for
    /// invalid options or a failed start. A failure during start-up, or an error left by `func`,
    /// is printed and makes the exit code 1.
    pub fn run_deprecated(&mut self, args: Vec<String>, func: Box<dyn FnMut()>) -> i32 {
        let mut all_opts = bpo::OptionsDescription::default();
        all_opts.add(self._app_opts.clone());
        all_opts.add(self._engine_opts.clone());

        let mut configuration = bpo::VariablesMap::new();
        let argv0 = args.first().cloned().unwrap_or_default();
        let parsed = (|| -> Result<(), bpo::Error> {
            bpo::store(
                bpo::command_line_parser(args.iter().skip(1).cloned().collect())
                    .options(&all_opts)
                    .positional(&self._pos_opts)
                    .run()?,
                &mut configuration,
            )?;
            match &mut self._conf_reader {
                ConfReader::Default => {
                    Self::default_configuration_reader(&self._opts_conf_file, &mut configuration)?
                }
                ConfReader::Custom(reader) => reader(&mut configuration)?,
            }
            Ok(())
        })();
        if let Err(e) = parsed {
            print!("error: {}\n\nTry --help.\n", e.what());
            return 2;
        }

        if configuration.count("help") != 0 {
            if !self._opts.description.empty() {
                println!("{}", self._opts.description);
            }
            println!("{}", self._app_opts);
            return 0;
        }
        if configuration.count("help-engine") != 0 {
            println!("{}", self._engine_opts);
            return 0;
        }
        register_engine_loggers();
        if configuration.count("help-loggers") != 0 {
            let _ = log_cli::print_available_loggers(&mut StdoutWriter(std::io::stdout().lock()));
            return 0;
        }

        if let Err(ex) = bpo::notify(&mut configuration) {
            println!("{}", ex.what());
            return 1;
        }

        {
            let mut visitor = VariablesMapExtractingVisitor::new(&configuration);
            if let Err(e) = self.options_mut().group.mutate(&mut visitor) {
                println!("{}", e);
                return 1;
            }
        }

        let opts = self.options_mut();
        opts.reactor_opts._argv0 = argv0;
        opts.reactor_opts._auto_handle_sigint_sigterm = opts.auto_handle_sigint_sigterm;
        let hugepages = opts.smp_opts.hugepages.as_bool();
        if let Some(native_stack) = opts.reactor_opts.network_stack
            .get_selected_candidate_opts_as_mut::<crate::engine::net::native_stack::NativeStackOptions>() {
            native_stack._hugepages = hugepages;
        }

        // Needs to be before `smp::configure()`.
        if let Err(exn) = apply_logging_settings(&log_cli::extract_settings(&self._opts.log_opts)) {
            println!("logging configuration error: {}", exn);
            return 1;
        }

        if let Err(e) = self._smp.configure(&self._opts.smp_opts, &self._opts.reactor_opts) {
            let _ = writeln!(std::io::stderr(), "Could not initialize engine: {}", e);
            return 1;
        }

        self._configuration = Some(configuration);

        // The future is not awaited: the reactor's run loop below keeps func going.
        let opts: *const EngineOptions = &*self._opts;
        let mut func = func;
        // SAFETY: smp::configure() set this thread's reactor; the reference to it ends with
        // the call, which returns a future without running user code.
        let started = unsafe { (*engine()).when_started() };
        let _ = started
            .then(move |()| {
                // SAFETY: the options are boxed in the app_template, which outlives the reactor's
                // run (engine().run() below), and are not modified while it runs; only shared
                // references are formed.
                let opts = unsafe { &*opts };
                metrics::configure(&opts.metrics_opts).then(move |()| -> Future<()> {
                    // collectd uses the metrics configuration, so metrics are configured first.
                    match scollectd::configure(&opts.scollectd_opts) {
                        Ok(()) => make_ready_future(()),
                        // An invalid collectd address fails the start.
                        Err(e) => make_exception_future(e),
                    }
                })
            })
            .then(move |()| {
                func();
            })
            .then_wrapped(|f: Future<()>| {
                if f.failed() {
                    let ep = f.get_exception();
                    if let Some(ex) = ep.get() {
                        println!("program failed with uncaught exception: {}", ex);
                        Reactor::exit(1);
                    }
                }
            });
        // SAFETY: this thread's reactor is set (by smp::configure()) and no reference to it
        // is live; the loop reaches it through the pointer.
        let exit_code = unsafe { Reactor::run(engine()) };
        self._smp.cleanup();
        exit_code
    }
}
