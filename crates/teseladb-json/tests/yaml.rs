//! The YAML subset of `refnode` configuration files (001 FR-022): block mappings and sequences,
//! flow sequences of scalars, plain and quoted scalars with the YAML 1.2 core types, and comments;
//! everything else is an error with its line and column.

use teseladb_json::{Number, Value, YamlErrorKind, parse_yaml, to_string};

fn json(yaml: &str) -> String {
    to_string(&parse_yaml(yaml.as_bytes()).unwrap_or_else(|e| panic!("{yaml}: {e}"))).unwrap()
}

fn error(yaml: &str) -> (YamlErrorKind, usize, usize) {
    let e = parse_yaml(yaml.as_bytes()).expect_err(yaml);
    (e.kind, e.line, e.column)
}

#[test]
fn a_refnode_configuration_file() {
    let yaml = "\
# refnode.conf
storage:
  dbPath: /var/lib/teseladb   # data
net:
  bindIp: 127.0.0.1,::1
  port: 27017
systemLog:
  destination: file
  path: \"/var/log/teseladb/teseladb.log\"
  logAppend: true
  verbosity: 2
setParameter:
  enableLocalhostAuthBypass: false
";
    assert_eq!(
        json(yaml),
        r#"{"storage":{"dbPath":"/var/lib/teseladb"},"net":{"bindIp":"127.0.0.1,::1","port":27017},"systemLog":{"destination":"file","path":"/var/log/teseladb/teseladb.log","logAppend":true,"verbosity":2},"setParameter":{"enableLocalhostAuthBypass":false}}"#
    );
}

#[test]
fn scalars_take_the_core_schema_types() {
    let yaml = "\
a: null
b: ~
c:
d: true
e: False
f: -12
g: 0x1F
h: 0o17
i: 1.5e3
j: .inf
k: 18446744073709551615
l: 2026-10-29
m: 'it''s'
n: \"tab\\tquote\\\" \\u00e9 \\\\\"
o: 'a # not a comment'
r: ''' #b''' # comment
p: x#y
q: \"\"
";
    let v = parse_yaml(yaml.as_bytes()).unwrap();
    let get = |k: &str| v.get(k).unwrap().clone();
    assert_eq!((get("a"), get("b"), get("c")), (Value::Null, Value::Null, Value::Null));
    assert_eq!((get("d"), get("e")), (Value::Bool(true), Value::Bool(false)));
    assert_eq!(get("f"), Value::Number(Number::Int(-12)));
    assert_eq!(get("g"), Value::Number(Number::Int(31)));
    assert_eq!(get("h"), Value::Number(Number::Int(15)));
    assert_eq!(get("i"), Value::Number(Number::Float(1500.0)));
    assert_eq!(get("j"), Value::String(".inf".into()), "no infinity in JSON values");
    assert_eq!(get("k"), Value::Number(Number::UInt(u64::MAX)));
    assert_eq!(get("l"), Value::String("2026-10-29".into()));
    assert_eq!(get("m"), Value::String("it's".into()));
    assert_eq!(get("n"), Value::String("tab\tquote\" é \\".into()));
    assert_eq!(get("o"), Value::String("a # not a comment".into()));
    assert_eq!(get("p"), Value::String("x#y".into()));
    assert_eq!(get("q"), Value::String(String::new()));
    assert_eq!(get("r"), Value::String("' #b'".into()), "a quote doubled inside quotes");
}

#[test]
fn sequences_in_block_and_flow_form() {
    let yaml = "\
teseladb:
  join:
    - 10.0.0.1:27100
    - \"10.0.0.2:27100\"
  seeds: [a, 'b c', \"d\", 3]
  empty: []
list:
- x
- y
---
";
    assert_eq!(
        json(&format!("---\n{yaml}").replace("\n---\n", "\n")),
        r#"{"teseladb":{"join":["10.0.0.1:27100","10.0.0.2:27100"],"seeds":["a","b c","d",3],"empty":[]},"list":["x","y"]}"#
    );
    assert_eq!(json("{}"), "{}");
    assert_eq!(json(""), "{}");
    assert_eq!(json("# only a comment\n\n"), "{}");
    assert_eq!(json("\"quoted key\": 1\n'other': 2"), r#"{"quoted key":1,"other":2}"#);
    assert_eq!(json("a:\n  b:\n    c: 1\n  d: 2\ne: 3"), r#"{"a":{"b":{"c":1},"d":2},"e":3}"#);
    assert_eq!(json("a:\r\n  b: 1\r\n"), r#"{"a":{"b":1}}"#);
}

#[test]
fn what_the_subset_does_not_have_is_an_error_with_its_position() {
    use YamlErrorKind::*;
    for (yaml, want) in [
        ("a: 1\na: 2", (DuplicateKey, 2, 1)),
        ("a:\n\tb: 1", (Tab, 2, 1)),
        ("a: 1\n  b: 2", (Indent, 2, 3)),
        ("a:\n    b: 1\n  c: 2", (Indent, 3, 3)),
        ("- a\nb: 1", (Syntax, 2, 1)),
        ("a: &x 1", (Unsupported, 1, 4)),
        ("a: *x", (Unsupported, 1, 4)),
        ("a: !!str 1", (Unsupported, 1, 4)),
        ("a: |\n  text", (Unsupported, 1, 4)),
        ("a: >\n  text", (Unsupported, 1, 4)),
        ("a: {b: 1}", (Unsupported, 1, 4)),
        ("? a\n: 1", (Unsupported, 1, 1)),
        ("a: 1\n---\nb: 2", (Unsupported, 2, 1)),
        ("a: 1\n...", (Unsupported, 2, 1)),
        ("a:\n  - b: 1", (Unsupported, 2, 5)),
        ("a:\n  - - b", (Unsupported, 2, 5)),
        ("a: [b, [c]]", (Unsupported, 1, 8)),
        ("a: [b, c", (Syntax, 1, 9)),
        ("a: [b,, c]", (Syntax, 1, 7)),
        ("a: 'b", (Quote, 1, 4)),
        ("a: \"b", (Quote, 1, 4)),
        ("a: \"\\q\"", (Escape, 1, 5)),
        ("a: \"\\u12\"", (Escape, 1, 5)),
        ("a: 'b' c", (Syntax, 1, 8)),
        ("a", (Syntax, 1, 1)),
        ("a:b", (Syntax, 1, 1)),
        (": 1", (Syntax, 1, 1)),
        ("just text\n", (Syntax, 1, 1)),
    ] {
        assert_eq!(error(yaml), want, "{yaml:?}");
    }
    let e = parse_yaml(b"a: \xff").unwrap_err();
    assert_eq!((e.kind, e.line), (Utf8, 1));
    assert_eq!(e.to_string(), "invalid UTF-8 at line 1, column 4");
}

#[test]
fn limits_of_depth_and_size() {
    let deep: String = (0..40).map(|i| format!("{}k{i}:\n", " ".repeat(i))).collect();
    assert_eq!(error(&deep).0, YamlErrorKind::Depth);
    let big = format!("a: {}", "x".repeat(2 << 20));
    assert_eq!(error(&big).0, YamlErrorKind::Size);
}
