//! The audit options of `teseladb start` added by 013 T042 (FR-031): `--auditRuntimeConfiguration`,
//! `--auditSchema` and `--setParameter auditAuthorizationSuccess`, their names in the
//! configuration file and in `getCmdLineOpts`, and RefDb Enterprise 9.0.4's errors (measured on
//! 31-10-2026).

use teseladb_auth::audit::Schema;
use teseladb_server::cli::{Command, parse, parse_with};

fn args(s: &str) -> Vec<String> {
    s.split_whitespace().map(String::from).collect()
}

fn bad(s: &str) -> String {
    parse(&args(s)).expect_err(s).to_string()
}

fn start(s: &str) -> teseladb_server::cli::StartOpts {
    match parse(&args(s)).unwrap() {
        Command::Start(o) => *o,
        other => panic!("{other:?}"),
    }
}

#[test]
fn runtime_configuration_schema_and_authorization_success() {
    let o = start(
        "start --auditDestination console --auditRuntimeConfiguration true --auditSchema OCSF",
    );
    let config = o.audit.config().unwrap().unwrap();
    assert!(config.runtime && config.schema == Schema::Ocsf && !config.authorization_success);
    let o = start("start --auditDestination console --setParameter auditAuthorizationSuccess=1");
    assert!(o.audit.config().unwrap().unwrap().authorization_success);
    assert_eq!(o.audit.authorization_success, Some(true));
    for (value, on) in [("true", true), ("false", false), ("0", false)] {
        let o = start(&format!("start --setParameter auditAuthorizationSuccess={value}"));
        assert_eq!(o.audit.authorization_success, Some(on), "{value}");
    }
    assert_eq!(start("start").audit.authorization_success, None);
    assert!(
        !start("start --auditDestination console --auditRuntimeConfiguration false")
            .audit
            .config()
            .unwrap()
            .unwrap()
            .runtime
    );
    // RefDb's errors.
    assert_eq!(
        bad("start --setParameter auditAuthorizationSuccess=yes"),
        "Bad value for parameter \"auditAuthorizationSuccess\": auditAuthorizationSuccess expects bool, got 'yes'"
    );
    assert_eq!(
        bad("start --auditDestination console --auditRuntimeConfiguration true --auditFilter {}"),
        "auditLog.filter must not be configured when runtime audit configuration is enabled"
    );
    assert_eq!(
        bad(
            "start --auditDestination console --auditRuntimeConfiguration true --setParameter auditAuthorizationSuccess=true"
        ),
        "setParameter.auditAuthorizationSuccess must not be configured when runtime audit configuration is enabled"
    );
    assert_eq!(
        bad("start --auditRuntimeConfiguration true"),
        "auditLog.destination is required when auditLog.runtimeConfiguration is specified"
    );
    assert_eq!(
        bad("start --auditDestination console --auditSchema ocsf"),
        "auditSchema must be one of 'ref' or 'OCSF'"
    );
    // The encrypted file: a key file, and zstd only with it (013 FR-031).
    let o = start(
        "start --auditDestination file --auditFormat JSON --auditPath /tmp/a.json --auditLocalKeyFile /k --auditCompressionMode zstd",
    );
    assert_eq!(o.audit.local_key_file.as_deref(), Some("/k"));
    assert_eq!(o.audit.compression_mode.as_deref(), Some("zstd"));
    assert_eq!(
        bad(
            "start --auditDestination file --auditFormat JSON --auditPath /tmp/a.json --auditCompressionMode zstd"
        ),
        "auditLog.compressionMode is only allowed if audit log encryption is enabled"
    );
    assert!(
        bad("start --auditDestination console --auditRuntimeConfiguration maybe")
            .contains("auditRuntimeConfiguration")
    );
}

#[test]
fn the_configuration_file_and_get_cmd_line_opts_name_them() {
    let yaml = "auditLog:\n  destination: console\n  runtimeConfiguration: true\n  schema: OCSF\n  localAuditKeyFile: /k\n";
    let Ok(Command::Start(o)) =
        parse_with(&args("start --config /c.yaml"), &|_| Ok(yaml.as_bytes().to_vec()))
    else {
        panic!()
    };
    let config = o.audit.config().unwrap().unwrap();
    assert!(config.runtime && config.schema == Schema::Ocsf);
    assert_eq!(config.local_key_file.as_deref(), Some("/k"));
    let mut shown = *o;
    shown.given.push(("auditRuntimeConfiguration".into(), "true".into()));
    shown.given.push(("auditSchema".into(), "OCSF".into()));
    shown.given.push(("auditLocalKeyFile".into(), "/k".into()));
    shown.given.push(("auditCompressionMode".into(), "zstd".into()));
    let text = format!("{:?}", teseladb_server::parsed_options(&shown));
    assert!(text.contains("runtimeConfiguration") && text.contains("schema"), "{text}");
    assert!(text.contains("localAuditKeyFile") && text.contains("compressionMode"), "{text}");
}
