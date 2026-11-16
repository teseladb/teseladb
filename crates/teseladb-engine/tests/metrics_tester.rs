//! A metrics server for the Prometheus exposition tests: it reads a YAML file of metric
//! families and values (with `teseladb::engine::core::reactor::yaml`), registers them and
//! serves them over HTTP until stopped. A program with its own `main()` (`harness = false`),
//! driven by `tests/data/prometheus_test.py`.

// This file calls unsafe engine, libc or FFI interfaces directly; every `unsafe` block
// states why it is sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

#[path = "common/stop_signal.rs"]
mod stop_signal;

use std::io::Write;

use stop_signal::StopSignal;
use teseladb::engine::core::app_template::AppTemplate;
use teseladb::engine::core::future::{ExceptionPtr, Future, RuntimeError, make_exception_future};
use teseladb::engine::core::internal::estimated_histogram::TimeEstimatedHistogram;
use teseladb::engine::core::metrics::impl_::MetricDefinitionImpl;
use teseladb::engine::core::metrics::{self as sm, Description, LabelInstance};
use teseladb::engine::core::metrics_registration::MetricGroups;
use teseladb::engine::core::program_options::bpo;
use teseladb::engine::core::prometheus;
use teseladb::engine::core::reactor::yaml;
use teseladb::engine::core::relabel_config::metrics::{
    MetricFamilyConfig, RelabelAction, RelabelConfig, RelabelConfigRegex,
};
use teseladb::engine::core::smp::Smp;
use teseladb::engine::core::sstring::Sstring;
use teseladb::engine::core::thread::async_;
use teseladb::engine::http::httpd::httpd::HttpServerControl;
use teseladb::engine::net::inet_address::InetAddress;
use teseladb::engine::net::socket_defs::SocketAddress;
use teseladb::engine::util::closeable::{DeferredStop, Stoppable};

/// The serializer tag (unused).
#[allow(dead_code)]
struct Serializer;

/// One metric of the configuration: its name, type, values and labels.
#[derive(Default)]
struct MetricDef {
    name: Sstring,
    type_: Sstring,
    values: Vec<f64>,
    labels: Vec<LabelInstance>,
}

/// The configuration file: the metric definitions and the metric family configurations.
#[derive(Default)]
struct Config {
    metrics: Vec<MetricDef>,
    metric_family_config: Vec<MetricFamilyConfig>,
}

fn as_f64_vec(node: &yaml::Node) -> Result<Vec<f64>, ExceptionPtr> {
    node.as_sequence()?
        .iter()
        .map(|n| {
            n.as_string()?.trim().parse::<f64>().map_err(|_| {
                ExceptionPtr::new(RuntimeError("yaml: error: bad conversion".to_string()))
            })
        })
        .collect()
}

/// A YAML map of strings: the keys come out sorted.
fn as_string_map(
    node: &yaml::Node,
) -> Result<std::collections::BTreeMap<String, String>, ExceptionPtr> {
    match node {
        yaml::Node::Map(m) => m.iter().map(|(k, v)| Ok((k.clone(), v.as_string()?))).collect(),
        _ => Err(ExceptionPtr::new(RuntimeError("yaml: error: bad conversion".to_string()))),
    }
}

/// Decodes a metric definition, from the fields the node defines; returns `Ok(true)`.
fn decode_metric_def(node: &yaml::Node, cfg: &mut MetricDef) -> Result<bool, ExceptionPtr> {
    if node.get("name").is_defined() {
        cfg.name = Sstring::from(node.get("name").as_string()?);
    }
    if node.get("type").is_defined() {
        cfg.type_ = Sstring::from(node.get("type").as_string()?);
    }
    if node.get("values").is_defined() {
        cfg.values = as_f64_vec(node.get("values"))?;
    }
    if node.get("labels").is_defined() {
        let labels = as_string_map(node.get("labels"))?;
        for (key, value) in &labels {
            cfg.labels.push(LabelInstance::new(key, value));
        }
    }
    Ok(true)
}

/// Decodes a metric family configuration, from the fields the node defines; returns `Ok(true)`.
fn decode_metric_family_config(
    node: &yaml::Node,
    cfg: &mut MetricFamilyConfig,
) -> Result<bool, ExceptionPtr> {
    if node.get("name").is_defined() {
        cfg.name = node.get("name").as_string()?;
    }
    if node.get("regex_name").is_defined() {
        cfg.regex_name = RelabelConfigRegex::new(&node.get("regex_name").as_string()?)?;
    }
    if node.get("aggregate_labels").is_defined() {
        cfg.aggregate_labels = node.get("aggregate_labels").as_string_vec()?;
    }
    Ok(true)
}

/// Decodes the whole configuration, from the fields the node defines; returns `Ok(true)`.
fn decode_config(node: &yaml::Node, cfg: &mut Config) -> Result<bool, ExceptionPtr> {
    if node.get("metrics").is_defined() {
        cfg.metrics = node
            .get("metrics")
            .as_sequence()?
            .iter()
            .map(|n| {
                let mut m = MetricDef::default();
                decode_metric_def(n, &mut m)?;
                Ok(m)
            })
            .collect::<Result<Vec<_>, ExceptionPtr>>()?;
    }
    if node.get("metric_family_config").is_defined() {
        cfg.metric_family_config = node
            .get("metric_family_config")
            .as_sequence()?
            .iter()
            .map(|n| {
                let mut m = MetricFamilyConfig::default();
                decode_metric_family_config(n, &mut m)?;
                Ok(m)
            })
            .collect::<Result<Vec<_>, ExceptionPtr>>()?;
    }
    Ok(true)
}

#[allow(dead_code)]
fn make_histogram_fun(c: &MetricDef) -> impl Fn() -> TimeEstimatedHistogram {
    let mut histogram = TimeEstimatedHistogram::default();
    for &v in &c.values {
        histogram.add_micro(v as u64);
    }
    move || histogram
}

fn make_metrics_definition(jc: &MetricDef) -> MetricDefinitionImpl {
    if jc.type_ == "histogram" {
        let mut histogram = TimeEstimatedHistogram::default();
        for &v in &jc.values {
            histogram.add_micro(v as u64);
        }
        return sm::make_histogram(
            jc.name.as_str(),
            move || histogram.to_metrics_histogram(),
            Description::new(jc.name.as_str()),
            jc.labels.clone(),
        );
    }
    if jc.type_ == "gauge" {
        let val = jc.values[0];
        return sm::make_gauge(
            jc.name.as_str(),
            move || val,
            Description::new(jc.name.as_str()),
            jc.labels.clone(),
        );
    }
    let val = jc.values[0];
    sm::make_counter(
        jc.name.as_str(),
        move || val,
        Description::new(jc.name.as_str()),
        jc.labels.clone(),
    )
}

/// Stops the Prometheus server on scope exit. A `DeferredStop` holds a `&mut` to what it
/// stops, while the program keeps using the server after creating the guard (to start the
/// exporter and listen); a second `&mut` to the control would alias the first, so the guard holds the
/// leaked control's pointer and calls the library's `Stoppable::stop` through it when it runs.
struct ServerStopper(*mut HttpServerControl);

impl Stoppable for ServerStopper {
    fn stop(&mut self) -> Future<()> {
        // SAFETY: the server control is leaked for the life of the program, and no other
        // reference to it is live when the guard stops it (at the end of the thread body).
        Stoppable::stop(unsafe { &mut *self.0 })
    }
}

pub(crate) fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut app = AppTemplate::new(Default::default());
    app.add_options()
        .opt(
            "listen",
            bpo::value::<Sstring>().default_value(Sstring::from("0.0.0.0")),
            "address to start Prometheus server on",
        )
        .opt("port", bpo::value::<u16>().default_value(9180), "Prometheus port")
        .opt(
            "conf",
            bpo::value::<Sstring>().default_value(Sstring::from("./conf.yaml")),
            "config with jobs and options",
        );

    let prometheus_server: *mut HttpServerControl =
        Box::into_raw(Box::new(HttpServerControl::new()));
    let appp: *mut AppTemplate = &mut *app;
    let status = app.run(args, move || {
        async_(move || -> Result<(), ExceptionPtr> {
            let mut metrics = MetricGroups::default();
            let mut stop_signal = StopSignal::new();
            // SAFETY: the application outlives run(), which waits for this thread.
            let opts = unsafe { (*appp).configuration() };
            let listen: Sstring = opts.get::<Sstring>("listen").cloned().unwrap_or_default();
            let port: u16 = opts.get::<u16>("port").copied().unwrap_or_default();
            let conf: Sstring = opts.get::<Sstring>("conf").cloned().unwrap_or_default();

            let doc = yaml::load_file(conf.as_str())?;
            let mut cfg = Config::default();
            decode_config(&doc, &mut cfg)?;
            for jc in &cfg.metrics {
                metrics.add_group("test_group", vec![make_metrics_definition(jc)])?;
            }
            Smp::invoke_on_all(|| {
                let mut rl: Vec<RelabelConfig> =
                    vec![RelabelConfig::default(), RelabelConfig::default()];
                rl[0].source_labels = vec!["__name__".to_string()];
                rl[0].expr = RelabelConfigRegex::new(".*").expect("regex");
                rl[0].action = RelabelAction::Drop;

                rl[1].source_labels = vec!["private".to_string()];
                rl[1].expr = RelabelConfigRegex::new(".*").expect("regex");
                rl[1].action = RelabelAction::Keep;
                sm::set_relabel_configs(&rl).then(|_: sm::MetricRelabelingResult| {})
            })
            .get()?;

            if !cfg.metric_family_config.is_empty() {
                sm::set_metric_family_configs(&cfg.metric_family_config);
            }

            // SAFETY: the server control is leaked for the life of the program.
            let server = unsafe { &mut *prometheus_server };
            server.start(&Sstring::from("prometheus")).get()?;
            let mut stopper = ServerStopper(prometheus_server);
            let _stop_server = DeferredStop::new(&mut stopper);
            let pctx = prometheus::Config { allow_protobuf: true, ..prometheus::Config::default() };
            prometheus::start(server, pctx).get()?;

            server
                .listen(SocketAddress::from_inet_address(
                    &InetAddress::from_sstring(&listen)?,
                    port,
                ))
                .handle_exception(|ep: ExceptionPtr| make_exception_future::<()>(ep))
                .get()?;

            println!("{}", port);
            let _ = std::io::stdout().flush();

            stop_signal.wait().get()?;
            drop(metrics);
            Ok(())
        })
    });
    std::process::exit(status);
}
