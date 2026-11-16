//! The smallest application: starts the reactors, logs "Hello world!" and exits.
//!
//! It is also the smoke test of the build: it links the whole runtime (reactor, smp, the C
//! libraries) and starts and stops the reactors.

use std::sync::LazyLock;

use teseladb::engine::core::app_template::{AppTemplate, Config};
use teseladb::engine::core::future::make_ready_future;
use teseladb::engine::util::log::Logger;
use teseladb::log_info;

/// The application's logger, named `app`.
static APPLOG: LazyLock<std::pin::Pin<Box<Logger>>> = LazyLock::new(|| match Logger::new("app") {
    Ok(l) => l,
    // The name is registered once, by this static; a clash is a programming error.
    Err(e) => panic!("logger `app` registered twice: {e}"),
});

pub(crate) fn main() {
    let mut app = AppTemplate::new(Config::default());
    let code = app.run(std::env::args().collect(), || {
        log_info!(APPLOG, "Hello world!");
        make_ready_future(())
    });
    std::process::exit(code);
}
