//! `teseladb start --config <file.yaml>` (001 FR-022): the keys of the `refnode` file and the
//! `teseladb.*` section, the command line over the file, and unknown keys as usage errors.

use std::io;
use std::path::{Path, PathBuf};

use teseladb_server::cli::{Command, StartOpts, parse_with};
use teseladb_server::config::yaml_args;
use teseladb_server::log::{Format, Level};

const FULL: &str = "\
storage:
  dbPath: /var/lib/teseladb
net:
  bindIp: localhost
  port: 27018
systemLog:
  destination: file
  path: /var/log/teseladb.log
  logAppend: true
  verbosity: 1
teseladb:
  cores: 4
  memoryLimit: 6G
  peerListen: 127.0.0.1:7100
  adminListen: 127.0.0.1:7200
  join:
    - 127.0.0.1:7101
    - 127.0.0.1:7102
  insecure: true
  logFormat: json
";

/// Parses `args` with `file` as the content of any configuration file.
fn with_file(args: &str, file: &str) -> Result<StartOpts, String> {
    let read = |_: &Path| -> io::Result<Vec<u8>> { Ok(file.as_bytes().to_vec()) };
    let args: Vec<String> = args.split_whitespace().map(String::from).collect();
    match parse_with(&args, &read) {
        Ok(Command::Start(o)) => Ok(*o),
        Ok(other) => panic!("{other:?}"),
        Err(e) => Err(e.to_string()),
    }
}

#[test]
fn every_key_of_the_file() {
    let o = with_file("start --config c.yaml", FULL).unwrap();
    assert_eq!(o.dbpath, PathBuf::from("/var/lib/teseladb"));
    assert_eq!((o.bind_ip.as_str(), o.port), ("localhost", 27018));
    assert_eq!((o.logpath, o.logappend), (Some(PathBuf::from("/var/log/teseladb.log")), true));
    assert_eq!((o.log_level, o.log_format), (Level::Debug, Format::Json));
    assert_eq!(o.cores, Some(4));
    assert_eq!(o.memory_limit, Some(6 << 30));
    assert_eq!(o.peer_listen.to_string(), "127.0.0.1:7100");
    assert_eq!(o.admin_listen.to_string(), "127.0.0.1:7200");
    assert_eq!(o.join, ["127.0.0.1:7101", "127.0.0.1:7102"]);
    assert!(o.insecure && !o.insecure_allow_remote);
    assert_eq!(o.config, Some(PathBuf::from("c.yaml")));
}

#[test]
fn the_command_line_wins_over_the_file() {
    let o =
        with_file("start --port 1234 --config c.yaml --join 127.0.0.1:9 --log-format text", FULL)
            .unwrap();
    assert_eq!(o.port, 1234);
    assert_eq!(o.join, ["127.0.0.1:9"]);
    assert_eq!(o.log_format, Format::Text);
    assert_eq!(o.dbpath, PathBuf::from("/var/lib/teseladb"), "the rest comes from the file");
    // `--insecure` may come from either; without it the node starts in secure mode.
    let o =
        with_file("start --insecure --config c.yaml", "teseladb:\n  join: 127.0.0.1:5\n").unwrap();
    assert_eq!(o.join, ["127.0.0.1:5"]);
    let secure = with_file("start --config c.yaml", "net:\n  port: 1\n").unwrap();
    assert!(!secure.insecure);
    assert_eq!(secure.port, 1);
    // Quiet, verbosity 0 and null values add nothing.
    let o = with_file(
        "start --config c.yaml",
        "teseladb:\n  insecure: true\n  join: ~\nsystemLog:\n  quiet: true\n  verbosity: 0\n",
    )
    .unwrap();
    assert_eq!((o.log_level, o.join.len()), (Level::Warn, 0));
}

#[test]
fn bad_files_are_usage_errors_that_name_the_file() {
    for (file, what) in [
        ("net:\n  bindIpAll: true\n", "--config c.yaml: unknown key `net.bindIpAll`"),
        ("replication:\n  replSetName: rs0\n", "unknown key `replication.replSetName`"),
        ("net: 5\n", "unknown key `net`"),
        ("teseladb:\n  cores: [1, 2]\n", "`teseladb.cores` takes one value"),
        (
            "teseladb:\n  join: [[a]]\n",
            "--config c.yaml: YAML outside the supported subset at line 2",
        ),
        ("systemLog:\n  destination: syslog\n", "`systemLog.destination`: only `file`"),
        (
            "systemLog:\n  destination: file\n",
            "`systemLog.destination: file` needs `systemLog.path`",
        ),
        ("systemLog:\n  verbosity: x\n", "`systemLog.verbosity`: a number from 0 to 5"),
        ("teseladb:\n  insecure: true\n  cores: 0\n", "--cores: `0` (1 to 256)"),
        ("- a\n", "--config c.yaml: the file is not a mapping"),
    ] {
        let e = with_file("start --config c.yaml", file).unwrap_err();
        assert!(e.contains(what), "{file:?}: {e}");
    }
    let read = |_: &Path| -> io::Result<Vec<u8>> { Err(io::Error::other("no such file")) };
    let args: Vec<String> = ["start", "--config", "gone.yaml"].map(String::from).to_vec();
    let e = parse_with(&args, &read).unwrap_err();
    assert_eq!(e.to_string(), "--config gone.yaml: no such file");
}

#[test]
fn yaml_args_are_command_line_options() {
    assert_eq!(
        yaml_args(b"net:\n  port: 27017\nteseladb:\n  insecure: false\n  join: [a, b]\n").unwrap(),
        ["--port=27017", "--insecure=false", "--join=a,b"]
    );
    assert_eq!(yaml_args(b"").unwrap(), Vec::<String>::new());
}

#[test]
fn set_parameter_in_the_file() {
    let file = "setParameter:\n  enableTestCommands: 1\n";
    assert_eq!(yaml_args(file.as_bytes()), Ok(vec!["--setParameter=enableTestCommands=1".into()]));
    assert!(with_file("start --insecure --config f", file).unwrap().test_commands);
}

/// The security file of the battery's `enterprise-auth` and `enterprise-ldap-authz` (013 T036,
/// T037; `tests/compat/tools/external_auth.sh`), the same one RefDb Enterprise reads.
const EXTERNAL_AUTH: &str = r#"security:
  ldap:
    servers: ldap:389
    transportSecurity: none
    bind:
      method: simple
      queryUser: "cn=admin,dc=tesela,dc=test"
      queryPassword: "q-pass-1234"
    userToDNMapping: '[{"match": "(.+)", "substitution": "uid={0},ou=people,dc=tesela,dc=test"}]'
    authz:
      queryTemplate: "ou=groups,dc=tesela,dc=test??sub?(&(objectClass=groupOfNames)(member={USER}))"
setParameter:
  authenticationMechanisms: PLAIN,GSSAPI,REFDB-AWS,REFDB-OIDC,SCRAM-SHA-256
  awsSTSUrl: https://sts:8443
  oidcIdentityProviders: '[{"issuer": "https://idp:8443", "audience": "teseladb", "authNamePrefix": "idp", "clientId": "teseladb-compat", "useAuthorizationClaim": true, "authorizationClaim": "groups", "supportsHumanFlows": false}]'
"#;

#[test]
fn the_security_file_of_the_external_identities() {
    use teseladb_auth::sasl::SaslMechanism;
    let o = with_file("start --config c.yaml", EXTERNAL_AUTH).unwrap();
    assert_eq!(o.ldap.servers.as_deref(), Some("ldap:389"));
    assert_eq!(o.ldap.transport.as_deref(), Some("none"));
    assert_eq!(o.ldap.bind_method.as_deref(), Some("simple"));
    assert_eq!(o.ldap.query_user.as_deref(), Some("cn=admin,dc=tesela,dc=test"));
    assert!(o.ldap.query_password.is_some());
    assert_eq!(
        o.ldap.mapping.as_deref(),
        Some(r#"[{"match": "(.+)", "substitution": "uid={0},ou=people,dc=tesela,dc=test"}]"#)
    );
    assert_eq!(
        o.ldap.authz_query_template.as_deref(),
        Some("ou=groups,dc=tesela,dc=test??sub?(&(objectClass=groupOfNames)(member={USER}))")
    );
    assert_eq!(
        o.mechanisms,
        [
            SaslMechanism::Plain,
            SaslMechanism::Gssapi,
            SaslMechanism::Aws,
            SaslMechanism::Oidc,
            SaslMechanism::ScramSha256
        ]
    );
    assert_eq!(o.aws_sts_url.as_deref(), Some("https://sts:8443"));
    assert!(o.oidc_providers.as_deref().is_some_and(|p| p.contains(r#""authNamePrefix": "idp""#)));
}

#[test]
fn redaction_from_the_file_or_the_command_line() {
    let file = "security:\n  redactClientLogData: true\n";
    assert!(with_file("start --config c.yaml", file).unwrap().redact_client_log_data);
    assert!(with_file("start --redactClientLogData", "").unwrap().redact_client_log_data);
    assert!(!with_file("start", "").unwrap().redact_client_log_data);
}
