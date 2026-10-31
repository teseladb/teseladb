//! LDAP for PLAIN logins (013 T036, FR-025): the BER of RFC 4511, the filters of RFC 4515 with
//! its examples, RefDb's queries and `userToDNMapping`, and the conversation of a login against
//! a server written here from RFC 4511: direct bind, search with and without the query user, and
//! each failure.

use teseladb_auth::ldap::{
    self, AuthzTemplate, INVALID_CREDENTIALS, LdapError, Login, Mapping, Message, Op, Step, Target,
    escape_dn_value,
};
use teseladb_auth::ldap_ber::{self, BerError, tag};
use teseladb_auth::ldap_filter::{self, Query, Scope};

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[test]
fn ber_lengths_and_integers() {
    assert_eq!(hex(&ldap_ber::element(4, &[7; 3])), "0403070707");
    let long = ldap_ber::element(4, &[0; 200]);
    assert_eq!(hex(&long[..3]), "0481c8");
    let longer = ldap_ber::element(4, &[0; 300]);
    assert_eq!(hex(&longer[..4]), "0482012c");
    for (v, want) in
        [(0, "00"), (127, "7f"), (128, "0080"), (-1, "ff"), (-129, "ff7f"), (256, "0100")]
    {
        assert_eq!(hex(&ldap_ber::int_content(v)), want, "{v}");
        assert_eq!(ldap_ber::int_value(&ldap_ber::int_content(v)), Ok(v));
    }
    // Four-byte lengths of short values, as Active Directory writes them, are read.
    assert_eq!(
        ldap_ber::read(&[4, 0x84, 0, 0, 0, 1, 9, 5]).unwrap(),
        Some((4, &[9][..], &[5][..]))
    );
    assert_eq!(ldap_ber::read(&[4, 0x84, 0, 0]).unwrap(), None);
    assert_eq!(ldap_ber::read(&[4, 3, 1]).unwrap(), None);
    assert_eq!(ldap_ber::read(&[]).unwrap(), None);
    assert_eq!(ldap_ber::read(&[4]).unwrap(), None);
    assert_eq!(ldap_ber::read(&[0x1f, 0]), Err(BerError("multi-byte tag")));
    assert_eq!(ldap_ber::read(&[4, 0x80]), Err(BerError("indefinite length")));
    assert_eq!(ldap_ber::read(&[4, 0x85, 0, 0, 0, 0, 1]), Err(BerError("length too large")));
    assert_eq!(ldap_ber::int_value(&[]), Err(BerError("integer length")));
    assert_eq!(ldap_ber::expect(&[5, 0], 4), Err(BerError("unexpected tag")));
    assert_eq!(ldap_ber::read_whole(&[4, 2, 1]), Err(BerError("truncated element")));
    assert!(!BerError("x").to_string().is_empty());
}

#[test]
fn filters_of_rfc_4515() {
    let cases = [
        ("(cn=Babs Jensen)", "a3110402636e040b42616273204a656e73656e"),
        ("(!(cn=Tim Howes))", "a211a30f0402636e040954696d20486f776573"),
        ("(o=univ*of*mich*)", "a41604016f3011800475e6976810266f6681046d696368"),
        ("(seeAlso=)", "a30b04077365654116c736f0400"),
        ("(objectClass=*)", "870b6f626a656374436c617373"),
        ("(cn:caseExactMatch:=Fred Flintstone)", "a9238110636173654578616374"),
    ];
    for (text, _) in &cases[..4] {
        assert!(ldap_filter::encode(text).is_ok(), "{text}");
    }
    assert_eq!(hex(&ldap_filter::encode(cases[0].0).unwrap()), cases[0].1);
    assert_eq!(hex(&ldap_filter::encode(cases[1].0).unwrap()), cases[1].1);
    assert_eq!(hex(&ldap_filter::encode(cases[4].0).unwrap()), cases[4].1);
    // `&` and `|`, escapes, the comparisons and the extensible forms.
    let f =
        ldap_filter::encode("(&(|(uid=a\\2ab)(mail=*@x))(age>=18)(age<=65)(sn~=smith))").unwrap();
    assert_eq!(f[0], 0xa0);
    assert!(hex(&f).contains(&hex(b"a*b")));
    assert_eq!(
        teseladb_auth::shell_json::quote_keys("{a.b: 'x', $c_1: 2}"),
        r#"{"a.b": "x", "$c_1": 2}"#
    );
    for ok in [
        "(cn;lang-en=x)",
        "(cn:dn:=x)",
        "(:caseExactMatch:=x)",
        "(cn:dn:2.4.6.8.10:=x)",
        "(cn=*a)",
        "(cn=a*)",
    ] {
        assert!(ldap_filter::encode(ok).is_ok(), "{ok}");
    }
    for bad in [
        "cn=x",
        "(cn=x",
        "(cn=x))",
        "(&)",
        "(cn)",
        "(c n=x)",
        "(cn=\\zz)",
        "(cn=\\4)",
        "(:=x)",
        "(cn:a:b:=x)",
        "(cn:1.2:dn:=x)",
        "(c n:dn:=x)",
        "(a<=x\\)",
        "(!cn=x)",
    ] {
        assert!(ldap_filter::encode(bad).is_err(), "{bad}");
    }
    let deep = format!("{}(a=b){}", "(!".repeat(40), ")".repeat(40));
    assert_eq!(ldap_filter::encode(&deep), Err(BerError("filter too deep")));
    assert_eq!(ldap_filter::escape_value("a*(b)\\\0"), "a\\2a\\28b\\29\\5c\\00");
}

#[test]
fn queries_of_refdbs_configuration() {
    let q = Query::parse("ou=Users,dc=example,dc=com??one?(uid=jane)").unwrap();
    assert_eq!(q.base, "ou=Users,dc=example,dc=com");
    assert_eq!((q.scope, q.filter.as_str(), q.attributes.len()), (Scope::One, "(uid=jane)", 0));
    let q = Query::parse("dc=example,dc=com?memberOf,cn?sub").unwrap();
    assert_eq!((q.scope, q.filter.as_str()), (Scope::Sub, "(objectClass=*)"));
    assert_eq!(q.attributes, ["memberOf", "cn"]);
    let q = Query::parse("cn=a%20b").unwrap();
    assert_eq!((q.base.as_str(), q.scope), ("cn=a b", Scope::Base));
    assert_eq!(Query::parse("x??base").unwrap().scope.code(), 0);
    for bad in ["a?b?c?d?e", "a??everywhere", "a??sub?(x", "%zz", "a?%4", "%ff"] {
        assert!(Query::parse(bad).is_err(), "{bad}");
    }
}

fn mapping(rules: &[(&str, &str, bool)]) -> Mapping {
    let mut m = Mapping::default();
    for (p, t, q) in rules {
        m.push(p, t, *q).unwrap();
    }
    m
}

#[test]
fn user_to_dn_mapping() {
    assert_eq!(Mapping::default().map("cn=x").unwrap(), Some(Target::Dn("cn=x".into())));
    let m = mapping(&[
        ("(.+)@ENGINEERING", "cn={0},ou=engineering,dc=example,dc=com", false),
        ("(.+)@(.+)", "ou={1},dc=example,dc=com??one?(uid={0})", true),
    ]);
    assert_eq!(
        m.map("jane@ENGINEERING").unwrap(),
        Some(Target::Dn("cn=jane,ou=engineering,dc=example,dc=com".into()))
    );
    let Some(Target::Query(q)) = m.map("j*ne@sales").unwrap() else { panic!() };
    assert_eq!(
        (q.base.as_str(), q.filter.as_str()),
        ("ou=sales,dc=example,dc=com", "(uid=j\\2ane)")
    );
    assert_eq!(m.map("no-at-sign").unwrap(), None);
    let mut bad = Mapping::default();
    assert!(matches!(bad.push("(", "x", false), Err(LdapError::Config(_))));
    assert!(matches!(bad.push("x", "a??nowhere", true), Err(LdapError::Config(_))));
    // A group that does not take part fills nothing; a filled query that breaks is refused.
    let m = mapping(&[("(a)|(b)", "cn={0}{1}", false), ("(.*)", "x??one?({0}=1)", true)]);
    assert_eq!(m.map("b").unwrap(), Some(Target::Dn("cn=b".into())));
    assert!(matches!(m.map("c d"), Err(LdapError::Config(_))));
}

// The server's side, from RFC 4511.
fn result_op(t: u8, code: i64, msg: &str) -> Vec<u8> {
    let mut c = ldap_ber::element(tag::ENUMERATED, &ldap_ber::int_content(code));
    ldap_ber::put(&mut c, tag::OCTET_STRING, b"");
    ldap_ber::put(&mut c, tag::OCTET_STRING, msg.as_bytes());
    ldap_ber::element(t, &c)
}

fn msg(id: i64, op: &[u8]) -> Vec<u8> {
    let mut c = ldap_ber::element(tag::INTEGER, &ldap_ber::int_content(id));
    c.extend_from_slice(op);
    ldap_ber::element(tag::SEQUENCE, &c)
}

fn entry(id: i64, dn: &str) -> Vec<u8> {
    let mut vals = Vec::new();
    ldap_ber::put(&mut vals, tag::OCTET_STRING, b"jane");
    let mut attr = ldap_ber::element(tag::OCTET_STRING, b"uid");
    ldap_ber::put(&mut attr, tag::SET, &vals);
    let mut c = ldap_ber::element(tag::OCTET_STRING, dn.as_bytes());
    ldap_ber::put(&mut c, tag::SEQUENCE, &ldap_ber::element(tag::SEQUENCE, &attr));
    msg(id, &ldap_ber::element(0x64, &c))
}

/// The request the client sent: its id and operation tag.
fn sent(bytes: &[u8]) -> (i64, u8, Vec<u8>) {
    let (c, _) = ldap_ber::expect(bytes, tag::SEQUENCE).unwrap();
    let (id, rest) = ldap_ber::expect(c, tag::INTEGER).unwrap();
    let (t, op, _) = ldap_ber::read_whole(rest).unwrap();
    (ldap_ber::int_value(id).unwrap(), t, op.to_vec())
}

#[test]
fn a_direct_bind_logs_in_with_the_password() {
    let m = mapping(&[("(.+)", "uid={0},ou=people,dc=example,dc=com", false)]);
    let (mut login, first) = Login::start(&m, "jane", b"pw-jane", None).unwrap();
    let (id, t, op) = sent(&first);
    assert_eq!((id, t), (1, 0x60));
    assert!(hex(&op).ends_with(&hex(b"\x80\x07pw-jane")));
    let reply = msg(1, &result_op(0x61, 0, ""));
    // In two pieces: the first is not enough.
    assert_eq!(login.receive(&reply[..3]).unwrap(), Step::Read);
    let Step::Done { dn, unbind, .. } = login.receive(&reply[3..]).unwrap() else { panic!() };
    assert_eq!(dn, "uid=jane,ou=people,dc=example,dc=com");
    assert_eq!(sent(&unbind).1, 0x42);
}

#[test]
fn a_search_finds_the_dn_then_the_user_binds() {
    let m = mapping(&[("(.+)", "ou=people,dc=example,dc=com??one?(uid={0})", true)]);
    let (mut login, first) = Login::start(&m, "jane", b"pw", Some(("cn=query", b"qpw"))).unwrap();
    assert_eq!(sent(&first).1, 0x60);
    let Step::Send(search) = login.receive(&msg(1, &result_op(0x61, 0, ""))).unwrap() else {
        panic!()
    };
    assert_eq!(sent(&search).1, 0x63);
    let mut answer = entry(2, "uid=jane,ou=people,dc=example,dc=com");
    answer.extend_from_slice(&msg(2, &ldap_ber::element(0x73, &ldap_ber::element(4, b"ldap://x"))));
    answer.extend_from_slice(&msg(2, &result_op(0x65, 0, "")));
    let Step::Send(bind) = login.receive(&answer).unwrap() else { panic!() };
    assert_eq!(sent(&bind).0, 3);
    let Step::Done { dn, .. } = login.receive(&msg(3, &result_op(0x61, 0, ""))).unwrap() else {
        panic!()
    };
    assert_eq!(dn, "uid=jane,ou=people,dc=example,dc=com");
    // Without the query user, the search goes first (anonymous).
    let (_, first) = Login::start(&m, "jane", b"pw", None).unwrap();
    assert_eq!(sent(&first).1, 0x63);
}

#[test]
fn failures_of_a_login() {
    let direct = mapping(&[("(.+)", "uid={0}", false)]);
    let search = mapping(&[("(.+)", "dc=x??sub?(uid={0})", true)]);
    let refused = |r: Result<Step, LdapError>, code| matches!(r, Err(LdapError::Refused(x)) if x.code == code);
    // A wrong password, an empty one, a user no rule maps.
    let (mut l, _) = Login::start(&direct, "jane", b"bad", None).unwrap();
    assert!(refused(
        l.receive(&msg(1, &result_op(0x61, INVALID_CREDENTIALS, "Invalid credentials"))),
        49
    ));
    assert!(matches!(Login::start(&direct, "jane", b"", None), Err(LdapError::Refused(_))));
    let none = mapping(&[("x", "uid=x", false)]);
    assert_eq!(Login::start(&none, "jane", b"p", None).err(), Some(LdapError::NoUser));
    // The query user refused; no entry; two entries; a failed search.
    let (mut l, _) = Login::start(&search, "jane", b"p", Some(("cn=q", b"q"))).unwrap();
    assert!(refused(l.receive(&msg(1, &result_op(0x61, 49, ""))), 49));
    let (mut l, _) = Login::start(&search, "jane", b"p", None).unwrap();
    assert_eq!(l.receive(&msg(1, &result_op(0x65, 0, ""))), Err(LdapError::NoUser));
    let (mut l, _) = Login::start(&search, "jane", b"p", None).unwrap();
    let mut two = entry(1, "uid=a");
    two.extend_from_slice(&entry(1, "uid=b"));
    two.extend_from_slice(&msg(1, &result_op(0x65, 0, "")));
    assert_eq!(l.receive(&two), Err(LdapError::NoUser));
    let (mut l, _) = Login::start(&search, "jane", b"p", None).unwrap();
    assert!(refused(l.receive(&msg(1, &result_op(0x65, 32, "No such object"))), 32));
    // Answers that are not the expected ones.
    let (mut l, _) = Login::start(&direct, "jane", b"p", None).unwrap();
    assert!(matches!(l.receive(&msg(7, &result_op(0x61, 0, ""))), Err(LdapError::Protocol(_))));
    let (mut l, _) = Login::start(&direct, "jane", b"p", None).unwrap();
    assert!(matches!(l.receive(&msg(1, &result_op(0x65, 0, ""))), Err(LdapError::Protocol(_))));
    let (mut l, _) = Login::start(&direct, "jane", b"p", None).unwrap();
    assert!(matches!(l.receive(&[0x04, 0]), Err(LdapError::Protocol(_))));
    for e in [
        LdapError::Config("x".into()),
        LdapError::NoUser,
        LdapError::Refused(ldap::LdapResult {
            code: 1,
            matched_dn: String::new(),
            message: String::new(),
        }),
        LdapError::Protocol("x".into()),
    ] {
        assert!(!e.to_string().is_empty());
    }
}

#[test]
fn server_messages_read_back() {
    let (m, used) = ldap::read_message(&msg(4, &result_op(0x78, 0, ""))).unwrap().unwrap();
    assert_eq!(m, Message { id: 4, op: Op::Other(0x78) });
    assert!(used > 0);
    assert_eq!(ldap::read_message(&[0x30]).unwrap(), None);
    assert!(ldap::read_message(&ldap_ber::element(4, b"")).is_err());
    // An entry whose attribute is not a SEQUENCE, or whose texts are not UTF-8.
    let bad_attr = msg(
        1,
        &ldap_ber::element(
            0x64,
            &[ldap_ber::element(4, b"dn"), ldap_ber::element(0x30, &ldap_ber::element(4, b"x"))]
                .concat(),
        ),
    );
    assert!(ldap::read_message(&bad_attr).is_err());
    let bad_text = msg(
        1,
        &result_op(0x61, 0, "\u{ff}")
            .iter()
            .map(|&b| if b == 0xc3 { 0xff } else { b })
            .collect::<Vec<_>>(),
    );
    assert!(ldap::read_message(&bad_text).is_err());
    let q = Query::parse("dc=x?cn?sub?(uid=a)").unwrap();
    let (_, t, _) = sent(&ldap::search_request(9, &q).unwrap());
    assert_eq!(t, 0x63);
    let broken = Query { filter: "(x".into(), ..q };
    assert!(ldap::search_request(1, &broken).is_err());
}

#[test]
fn mappings_from_the_configuration() {
    let m = Mapping::from_config(
        r#"[{match: "(.+)@ENG", substitution: "cn={0},dc=x"}, {'match': '(.+)', ldapQuery: 'dc=x??one?(uid={0})'}]"#,
    )
    .unwrap();
    assert_eq!(m.map("a@ENG").unwrap(), Some(Target::Dn("cn=a,dc=x".into())));
    assert!(matches!(m.map("b").unwrap(), Some(Target::Query(_))));
    for bad in [
        "{match: 'x'}",
        "[1]",
        "[{substitution: 'x'}]",
        "[{match: 'x'}]",
        "[{match: 'x', substitution: 'a', ldapQuery: 'b'}]",
        "[{match: '(', substitution: 'a'}]",
        "not json",
    ] {
        assert!(matches!(Mapping::from_config(bad), Err(LdapError::Config(_))), "{bad}");
    }
}

fn entry_with(id: i64, dn: &str, attr: &str, values: &[&str]) -> Vec<u8> {
    let mut vals = Vec::new();
    for v in values {
        ldap_ber::put(&mut vals, tag::OCTET_STRING, v.as_bytes());
    }
    let mut a = ldap_ber::element(tag::OCTET_STRING, attr.as_bytes());
    ldap_ber::put(&mut a, tag::SET, &vals);
    let mut c = ldap_ber::element(tag::OCTET_STRING, dn.as_bytes());
    ldap_ber::put(&mut c, tag::SEQUENCE, &ldap_ber::element(tag::SEQUENCE, &a));
    msg(id, &ldap_ber::element(0x64, &c))
}

const GROUPS: &str = "ou=groups,dc=example,dc=com??one?(member={USER})";
const READERS: &str = "cn=readers,ou=groups,dc=example,dc=com";
const WRITERS: &str = "cn=writers,ou=groups,dc=example,dc=com";

#[test]
fn the_authorization_template_fills_the_user_in() {
    let t = AuthzTemplate::parse(GROUPS).unwrap();
    let q = t.query("uid=ja(ne)*,ou=people,dc=example,dc=com", "ja(ne)*");
    assert_eq!(q.base, "ou=groups,dc=example,dc=com");
    assert_eq!(q.filter, r"(member=uid=ja\28ne\29\2a,ou=people,dc=example,dc=com)");
    assert_eq!(q.scope, Scope::One);
    let t = AuthzTemplate::parse("uid={PROVIDED_USER},ou=people,dc=x?memberOf?base").unwrap();
    let q = t.query("uid=a", "a,b");
    assert_eq!(
        (q.base.as_str(), q.attributes.clone()),
        (r"uid=a\,b,ou=people,dc=x", vec!["memberOf".to_owned()])
    );
    assert!(matches!(AuthzTemplate::parse("a?b?bad"), Err(LdapError::Config(_))));
    assert!(matches!(AuthzTemplate::parse("a??one?(member={USER}"), Err(LdapError::Config(_))));
    assert_eq!(escape_dn_value(" #a+b=\"c\"<d>;e\\f\0 "), r#"\ #a\+b\=\"c\"\<d\>\;e\\f\00\ "#);
    assert_eq!(escape_dn_value("#x"), r"\#x");
    assert_eq!(escape_dn_value(""), "");
}

/// The groups of a direct bind, searched as the user, and of a search, as the query user.
#[test]
fn a_login_with_a_template_returns_the_groups() {
    let template = AuthzTemplate::parse(GROUPS).unwrap();
    let direct = mapping(&[("(.+)", "uid={0},ou=people,dc=example,dc=com", false)]);
    let (login, _) = Login::start(&direct, "jane", b"pw", None).unwrap();
    let mut login = login.with_groups(&template, "jane");
    let Step::Send(search) = login.receive(&msg(1, &result_op(0x61, 0, ""))).unwrap() else {
        panic!()
    };
    let (id, t, op) = sent(&search);
    assert_eq!((id, t), (2, 0x63));
    assert!(hex(&op).contains(&hex(b"uid=jane,ou=people,dc=example,dc=com")));
    let mut answer = entry(2, READERS);
    answer.extend_from_slice(&entry(2, WRITERS));
    answer.extend_from_slice(&msg(2, &ldap_ber::element(0x73, &ldap_ber::element(4, b"ldap://x"))));
    answer.extend_from_slice(&msg(2, &result_op(0x65, 0, "")));
    let Step::Done { dn, groups, unbind } = login.receive(&answer).unwrap() else { panic!() };
    assert_eq!(dn, "uid=jane,ou=people,dc=example,dc=com");
    assert_eq!(groups, Some(vec![READERS.to_owned(), WRITERS.to_owned()]));
    assert_eq!(sent(&unbind), (3, 0x42, Vec::new()));
    // As the query user, and with the groups in an attribute of the user's entry.
    let by_attr = AuthzTemplate::parse("{USER}?memberOf?base").unwrap();
    let (login, _) = Login::start(&direct, "jane", b"pw", Some(("cn=q", b"qpw"))).unwrap();
    let mut login = login.with_groups(&by_attr, "jane");
    let Step::Send(bind) = login.receive(&msg(1, &result_op(0x61, 0, ""))).unwrap() else {
        panic!()
    };
    let (id, t, op) = sent(&bind);
    assert_eq!((id, t), (2, 0x60));
    assert!(hex(&op).contains(&hex(b"cn=q")));
    let Step::Send(search) = login.receive(&msg(2, &result_op(0x61, 0, ""))).unwrap() else {
        panic!()
    };
    assert_eq!(sent(&search).0, 3);
    let mut answer = entry_with(3, "uid=jane", "memberOf", &[READERS, WRITERS]);
    answer.extend_from_slice(&entry_with(3, "uid=jane", "cn", &["jane"]));
    answer.extend_from_slice(&msg(3, &result_op(0x65, 0, "")));
    let Step::Done { groups, .. } = login.receive(&answer).unwrap() else { panic!() };
    assert_eq!(groups, Some(vec![READERS.to_owned(), WRITERS.to_owned()]));
    // A user in no group logs in without groups.
    let (login, _) = Login::start(&direct, "lone", b"pw", None).unwrap();
    let mut login = login.with_groups(&template, "lone");
    login.receive(&msg(1, &result_op(0x61, 0, ""))).unwrap();
    let Step::Done { groups, .. } = login.receive(&msg(2, &result_op(0x65, 0, ""))).unwrap() else {
        panic!()
    };
    assert_eq!(groups, Some(Vec::new()));
}

#[test]
fn failures_of_the_group_search() {
    let template = AuthzTemplate::parse(GROUPS).unwrap();
    let direct = mapping(&[("(.+)", "uid={0}", false)]);
    let refused = |r: Result<Step, LdapError>, code| matches!(r, Err(LdapError::Refused(x)) if x.code == code);
    // The query user refused, and a search that fails.
    let (l, _) = Login::start(&direct, "jane", b"p", Some(("cn=q", b"q"))).unwrap();
    let mut l = l.with_groups(&template, "jane");
    l.receive(&msg(1, &result_op(0x61, 0, ""))).unwrap();
    assert!(refused(l.receive(&msg(2, &result_op(0x61, 49, ""))), 49));
    let (l, _) = Login::start(&direct, "jane", b"p", None).unwrap();
    let mut l = l.with_groups(&template, "jane");
    l.receive(&msg(1, &result_op(0x61, 0, ""))).unwrap();
    assert!(refused(l.receive(&msg(2, &result_op(0x65, 32, "No such object"))), 32));
    // An answer that is not part of a search.
    let (l, _) = Login::start(&direct, "jane", b"p", None).unwrap();
    let mut l = l.with_groups(&template, "jane");
    l.receive(&msg(1, &result_op(0x61, 0, ""))).unwrap();
    assert!(matches!(l.receive(&msg(2, &result_op(0x61, 0, ""))), Err(LdapError::Protocol(_))));
}

/// The groups of a user that logged in another way (REFDB-X509, 013 T045): no bind of the
/// user; the groups searched as the query user, or anonymously without one; with a mapping that
/// searches, the entry it finds is the user's DN (measured on RefDb Enterprise 9.0.4).
#[test]
fn groups_of_a_user_without_a_password() {
    let template = AuthzTemplate::parse(GROUPS).unwrap();
    let x509 = "CN=jane,OU=people,O=example";
    let direct =
        mapping(&[("CN=(.+),OU=people,O=example", "uid={0},ou=people,dc=example,dc=com", false)]);
    // Without a query user the first request is the search of the groups.
    let (mut login, first) = Login::groups_of(&direct, x509, None, &template).unwrap();
    let (id, t, op) = sent(&first);
    assert_eq!((id, t), (1, 0x63));
    assert!(hex(&op).contains(&hex(b"uid=jane,ou=people,dc=example,dc=com")));
    let mut answer = entry(1, READERS);
    answer.extend_from_slice(&msg(1, &result_op(0x65, 0, "")));
    let Step::Done { dn, groups, unbind } = login.receive(&answer).unwrap() else { panic!() };
    assert_eq!(dn, "uid=jane,ou=people,dc=example,dc=com");
    assert_eq!(groups, Some(vec![READERS.to_owned()]));
    assert_eq!(sent(&unbind).1, 0x42);
    // With a query user, it binds first.
    let (mut login, first) =
        Login::groups_of(&direct, x509, Some(("cn=q", b"qpw")), &template).unwrap();
    let (id, t, op) = sent(&first);
    assert_eq!((id, t), (1, 0x60));
    assert!(hex(&op).contains(&hex(b"cn=q")));
    let Step::Send(search) = login.receive(&msg(1, &result_op(0x61, 0, ""))).unwrap() else {
        panic!()
    };
    assert_eq!(sent(&search).1, 0x63);
    let Step::Done { groups, .. } = login.receive(&msg(2, &result_op(0x65, 0, ""))).unwrap() else {
        panic!()
    };
    assert_eq!(groups, Some(Vec::new()));
    // A mapping that searches: the DN found, then the groups, never a bind of the user.
    let search = mapping(&[(
        "CN=(.+),OU=people,O=example",
        "ou=people,dc=example,dc=com??one?(uid={0})",
        true,
    )]);
    let (mut login, first) = Login::groups_of(&search, x509, None, &template).unwrap();
    assert_eq!(sent(&first).1, 0x63);
    let mut found = entry(1, "uid=jane,ou=people,dc=example,dc=com");
    found.extend_from_slice(&msg(1, &result_op(0x65, 0, "")));
    let Step::Send(groups) = login.receive(&found).unwrap() else { panic!() };
    let (id, t, op) = sent(&groups);
    assert_eq!((id, t), (2, 0x63));
    assert!(hex(&op).contains(&hex(b"uid=jane,ou=people,dc=example,dc=com")));
    let mut answer = entry(2, WRITERS);
    answer.extend_from_slice(&msg(2, &result_op(0x65, 0, "")));
    let Step::Done { groups, .. } = login.receive(&answer).unwrap() else { panic!() };
    assert_eq!(groups, Some(vec![WRITERS.to_owned()]));
    // A name no rule maps has no groups to look for.
    let other = mapping(&[("uid=(.+)", "uid={0}", false)]);
    assert!(matches!(Login::groups_of(&other, x509, None, &template), Err(LdapError::NoUser)));
}
