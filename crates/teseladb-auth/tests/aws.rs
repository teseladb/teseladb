//! REFDB-AWS (013 T036, FR-025): AWS Signature Version 4 against AWS's published examples
//! (the `aws-signing-test-suite` of `awslabs/aws-c-auth`, cases `get-vanilla`, `post-vanilla`,
//! `post-x-www-form-urlencoded` and `get-vanilla-query-order-key-case`, and the IAM example of
//! the SigV4 documentation), and the server's side of the conversation as RefDb Enterprise
//! 9.0.4 answers it (measured with a test STS behind `awsSTSUrl`).

use teseladb_auth::aws::{self, AwsError, Server, SigV4};
use teseladb_bson::{Document, Value};

const KEY_ID: &str = "AKIDEXAMPLE";
const SECRET: &[u8] = b"wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";
const DATE: &str = "20150830T123600Z";

fn signature(auth: &str) -> &str {
    auth.rsplit("Signature=").next().unwrap()
}

#[test]
fn signatures_of_the_aws_test_suite() {
    let base = |method, query, headers, body| SigV4 {
        method,
        path: "/",
        query,
        headers,
        body,
        region: "us-east-1",
        service: "service",
        date: DATE,
        key_id: KEY_ID,
        secret: SECRET,
    };
    let host = [("Host", "example.amazonaws.com"), ("X-Amz-Date", DATE)];
    let a = aws::authorization(&base("GET", "", &host, b""));
    assert_eq!(signature(&a), "5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31");
    assert!(a.starts_with(
        "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, SignedHeaders=host;x-amz-date, Signature="
    ));
    let a = aws::authorization(&base("POST", "", &host, b""));
    assert_eq!(signature(&a), "5da7c1a2acd57cee7505fc6676e4e544621c30862966e37dddb68e92efbe5d6b");
    let a = aws::authorization(&base("GET", "Param2=value2&Param1=value1", &host, b""));
    assert_eq!(signature(&a), "b97d918cfa904a5beff61c982a1b6f458b799221646efd99d3219ec94cdf2500");
    let form = [
        ("Content-Type", "application/x-www-form-urlencoded"),
        ("Host", "example.amazonaws.com"),
        ("Content-Length", "13"),
        (
            "X-Amz-Content-Sha256",
            "9095672bbd1f56dfc5b65f3e153adc8731a4a654192329106275f4c7b24d0b6e",
        ),
        ("X-Amz-Date", DATE),
    ];
    let a = aws::authorization(&base("POST", "", &form, b"Param1=value1"));
    assert_eq!(signature(&a), "d3875051da38690788ef43de4db0d8f280229d82040bfac253562e56c3f20e0b");
    // The IAM `ListUsers` example of the SigV4 documentation.
    let iam = [
        ("Content-Type", "application/x-www-form-urlencoded; charset=utf-8"),
        ("Host", "iam.amazonaws.com"),
        ("X-Amz-Date", DATE),
    ];
    let a = aws::authorization(&SigV4 {
        query: "Action=ListUsers&Version=2010-05-08",
        headers: &iam,
        service: "iam",
        ..base("GET", "", &[], b"")
    });
    assert_eq!(
        a,
        "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/iam/aws4_request, SignedHeaders=content-type;host;x-amz-date, Signature=5d672d79c15b13162d9279b0855cfba6789a8edb4c82c400e06b5924a6f2b5d7"
    );
}

fn bson(d: &Document) -> Vec<u8> {
    d.to_bytes().unwrap()
}

fn first(nonce: &[u8], flag: i32) -> Vec<u8> {
    bson(
        &Document::new()
            .with("r", Value::Binary { subtype: 0, bytes: nonce.to_vec() })
            .with("p", flag),
    )
}

const HOST: &str = "sts.amazonaws.com";

fn started(flag: i32) -> (Server, Vec<u8>) {
    let (server, reply) = Server::start(&first(&[1; 32], flag), [2; 32], HOST).unwrap();
    let doc = Document::from(teseladb_bson::RawDoc::from_bytes(&reply).unwrap());
    let Some(Value::Binary { bytes, .. }) = doc.get("s") else { panic!("{doc:?}") };
    assert_eq!(bytes.len(), 64);
    assert_eq!((&bytes[..32], &bytes[32..]), (&[1u8; 32][..], &[2u8; 32][..]));
    assert_eq!(doc.get("h"), Some(&Value::from(HOST)));
    (server, bytes.clone())
}

/// What a driver sends second, signed for `server_nonce`, with `drop` left out of the headers.
fn second(server_nonce: &[u8], token: Option<&str>, drop: &str) -> Document {
    let nonce = teseladb_x509::pem::base64_encode(server_nonce);
    let date = "20261030T215406Z";
    let mut headers = vec![
        ("Content-Length", "43"),
        ("Content-Type", "application/x-www-form-urlencoded"),
        ("Host", HOST),
        ("X-Amz-Date", date),
        (teseladb_names::AWS_HEADER_CHANNEL_BINDING, "n"),
        (teseladb_names::AWS_HEADER_SERVER_NONCE, nonce.as_str()),
    ];
    if let Some(t) = token {
        headers.push(("X-Amz-Security-Token", t));
    }
    headers.retain(|(n, _)| !n.eq_ignore_ascii_case(drop));
    let a = aws::authorization(&SigV4 {
        method: "POST",
        path: "/",
        query: "",
        headers: &headers,
        body: aws::STS_BODY.as_bytes(),
        region: "us-east-1",
        service: "sts",
        date,
        key_id: "AKIDUSER",
        secret: b"secret",
    });
    let mut d = Document::new().with("a", a).with("d", date);
    if let Some(t) = token {
        d.push("t", t);
    }
    d
}

#[test]
fn the_conversation_builds_the_sts_request() {
    let (server, nonce) = started(i32::from(b'n'));
    let msg = second(&nonce, None, "");
    let req = server.request(&bson(&msg)).unwrap();
    let header = |n: &str| req.headers.iter().find(|(k, _)| k == n).map(|(_, v)| v.as_str());
    assert_eq!(header("Authorization").map(Value::from).as_ref(), msg.get("a"));
    assert_eq!(header("X-Amz-Date"), Some("20261030T215406Z"));
    assert_eq!(
        header(teseladb_names::AWS_HEADER_SERVER_NONCE),
        Some(teseladb_x509::pem::base64_encode(&nonce).as_str())
    );
    assert_eq!(header(teseladb_names::AWS_HEADER_CHANNEL_BINDING), Some("n"));
    assert_eq!(header("X-Amz-Security-Token"), None);
    assert_eq!(req.body, aws::STS_BODY);
    // A session token, and the flag `y`.
    let (server, nonce) = started(i32::from(b'y'));
    let req = server.request(&bson(&second(&nonce, Some("tok"), ""))).unwrap();
    assert!(req.headers.contains(&("X-Amz-Security-Token".to_owned(), "tok".to_owned())));
    assert!(
        req.headers
            .contains(&(teseladb_names::AWS_HEADER_CHANNEL_BINDING.to_owned(), "y".to_owned()))
    );
}

#[test]
fn bad_messages_fail_before_the_sts() {
    let bin = |n: usize| Value::Binary { subtype: 0, bytes: vec![1; n] };
    for (label, payload) in [
        ("not BSON", b"xyz".to_vec()),
        ("31 bytes", first(&[1; 31], 110)),
        ("no r", bson(&Document::new().with("p", 110))),
        ("r a string", bson(&Document::new().with("r", "abc").with("p", 110))),
        ("no p", bson(&Document::new().with("r", bin(32)))),
        ("p is p", first(&[1; 32], i32::from(b'p'))),
        ("p is x", first(&[1; 32], i32::from(b'x'))),
        ("p a string", bson(&Document::new().with("r", bin(32)).with("p", "n"))),
        ("extra", bson(&Document::new().with("r", bin(32)).with("p", 110).with("z", 1))),
    ] {
        assert!(Server::start(&payload, [0; 32], HOST).is_err(), "{label}");
    }
    let (server, nonce) = started(110);
    let good = second(&nonce, None, "");
    let a = good.get("a").cloned().unwrap();
    let d = good.get("d").cloned().unwrap();
    for (label, msg) in [
        ("no a", bson(&Document::new().with("d", d.clone()))),
        ("no d", bson(&Document::new().with("a", a.clone()))),
        ("a not a string", bson(&Document::new().with("a", 1).with("d", d.clone()))),
        (
            "t not a string",
            bson(&Document::new().with("a", a.clone()).with("d", d.clone()).with("t", 1)),
        ),
        ("extra", bson(&Document::new().with("a", a.clone()).with("d", d.clone()).with("z", "1"))),
        ("a garbage", bson(&Document::new().with("a", "garbage").with("d", d.clone()))),
        ("no nonce header", bson(&second(&nonce, None, teseladb_names::AWS_HEADER_SERVER_NONCE))),
        ("no flag header", bson(&second(&nonce, None, teseladb_names::AWS_HEADER_CHANNEL_BINDING))),
        ("not BSON", b"x".to_vec()),
    ] {
        assert!(server.request(&msg).is_err(), "{label}");
    }
    // What the STS checks is left to it: another host or date still goes.
    assert!(server.request(&bson(&second(&nonce, None, "Host"))).is_ok());
    assert!(matches!(server.request(b"x"), Err(AwsError(_))));
    // Authorization headers with a part too many, a short credential scope or no signature.
    let Value::String(a) = a else { panic!("{a:?}") };
    let (head, signature) = a.rsplit_once("Signature=").unwrap();
    for bad in
        [format!("{a}, Extra=1"), a.replacen("/us-east-1", "", 1), format!("{head}Signature=")]
    {
        assert!(!signature.is_empty());
        let msg = bson(&Document::new().with("a", bad.as_str()).with("d", d.clone()));
        let e = server.request(&msg).unwrap_err();
        assert_eq!(e.to_string(), "bad Authorization header");
    }
}

#[test]
fn the_sts_answer_names_the_user() {
    let ok = |arn: &str| {
        format!(
            "<GetCallerIdentityResponse xmlns=\"https://sts.amazonaws.com/doc/2011-06-15/\"><GetCallerIdentityResult><Arn>{arn}</Arn><UserId>AIDA</UserId><Account>123456789012</Account></GetCallerIdentityResult><ResponseMetadata><RequestId>r</RequestId></ResponseMetadata></GetCallerIdentityResponse>"
        )
    };
    let user = "arn:aws:iam::123456789012:user/alice";
    assert_eq!(aws::caller(200, ok(user).as_bytes()).unwrap(), user);
    // An assumed role is the role with any session, as RefDb names it.
    let role = "arn:aws:sts::123456789012:assumed-role/reader/session1";
    assert_eq!(
        aws::caller(200, ok(role).as_bytes()).unwrap(),
        "arn:aws:sts::123456789012:assumed-role/reader/*"
    );
    assert!(aws::caller(403, b"<ErrorResponse/>").is_err());
    assert!(aws::caller(200, b"<GetCallerIdentityResponse/>").is_err());
    assert!(aws::caller(200, ok("").as_bytes()).is_err());
    assert!(aws::caller(200, &[0xff, 0xfe]).is_err());
}

#[test]
fn the_sts_url_gives_the_host() {
    assert_eq!(aws::sts_host(aws::DEFAULT_STS_URL), Ok("sts.amazonaws.com".to_owned()));
    assert_eq!(aws::sts_host("https://ld13-sts:8443"), Ok("ld13-sts".to_owned()));
    assert_eq!(aws::sts_host("https://h/path"), Ok("h".to_owned()));
    assert_eq!(aws::sts_host("http://h"), Err(AwsError("STS URL must start with https://".into())));
    assert!(aws::sts_host("https://").is_err());
}
