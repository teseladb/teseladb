//! AWS IAM authentication (013 FR-025): AWS Signature Version 4 and the server's side of the
//! conversation of the drivers' authentication specification, without I/O. The client proves who it
//! is by signing an STS `GetCallerIdentity` request bound to the server's nonce; the server sends
//! that request to the STS (`awsSTSUrl`) and the ARN of the answer names the user of `$external`.
//! Checks measured on the reference enterprise server 9.0.4: the first message is exactly `{r:
//! BinData of 32 bytes, p: 'n' or 'y'}`, the second exactly `{a, d[, t]}` with strings, and the
//! signed headers include the server nonce and the channel binding flag; the signature, the date
//! and the host are the STS's to check.

use std::fmt;

use teseladb_bson::{Document, RawDoc, Value};
use teseladb_crypto::HashAlgorithm;
use teseladb_x509::pem::base64_encode;

/// `awsSTSUrl` by default.
pub const DEFAULT_STS_URL: &str = "https://sts.amazonaws.com";
/// The body of `GetCallerIdentity`.
pub const STS_BODY: &str = "Action=GetCallerIdentity&Version=2011-06-15";
/// Its type.
pub const STS_CONTENT_TYPE: &str = "application/x-www-form-urlencoded";
/// Bytes of the client's nonce and of the server's part.
pub const NONCE_LEN: usize = 32;
/// The signing algorithm.
const ALGORITHM: &str = "AWS4-HMAC-SHA256";
/// `gs2-cb-flag` values a client may send: `n` and `y` (`p` is refused, measured).
const FLAGS: [u8; 2] = *b"ny";
/// Headers the client must have signed (measured: without them the reference server refuses before
/// the STS).
const REQUIRED_SIGNED: [&str; 2] =
    [teseladb_names::AWS_SIGNED_SERVER_NONCE, teseladb_names::AWS_SIGNED_CHANNEL_BINDING];

/// Why a step of AWS IAM authentication failed; the client only learns `Authentication failed.`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AwsError(pub String);

impl fmt::Display for AwsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for AwsError {}

fn err<T>(m: impl Into<String>) -> Result<T, AwsError> {
    Err(AwsError(m.into()))
}

/// A request to sign with Signature Version 4.
#[derive(Clone, Copy, Debug)]
pub struct SigV4<'a> {
    /// HTTP method.
    pub method: &'a str,
    /// Path, already URI-encoded.
    pub path: &'a str,
    /// Query string, already URI-encoded (`a=1&b=2`), in any order.
    pub query: &'a str,
    /// Headers to sign, with their values.
    pub headers: &'a [(&'a str, &'a str)],
    /// The body.
    pub body: &'a [u8],
    /// Region.
    pub region: &'a str,
    /// Service.
    pub service: &'a str,
    /// `X-Amz-Date`: `YYYYMMDDTHHMMSSZ`.
    pub date: &'a str,
    /// Access key ID.
    pub key_id: &'a str,
    /// Secret access key.
    pub secret: &'a [u8],
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The `Authorization` header of `r` (AWS Signature Version 4).
pub fn authorization(r: &SigV4<'_>) -> String {
    let mut headers: Vec<(String, String)> = r
        .headers
        .iter()
        .map(|(n, v)| (n.to_ascii_lowercase(), v.split_whitespace().collect::<Vec<_>>().join(" ")))
        .collect();
    headers.sort();
    let mut query: Vec<&str> = r.query.split('&').filter(|p| !p.is_empty()).collect();
    query.sort_unstable();
    let signed = headers.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>().join(";");
    let canonical_headers: String = headers.iter().map(|(n, v)| format!("{n}:{v}\n")).collect();
    let sha = HashAlgorithm::Sha256;
    let canonical = format!(
        "{}\n{}\n{}\n{canonical_headers}\n{signed}\n{}",
        r.method,
        r.path,
        query.join("&"),
        hex(sha.hash(r.body).as_ref())
    );
    let day = r.date.get(..8).unwrap_or(r.date);
    let scope = format!("{day}/{}/{}/aws4_request", r.region, r.service);
    let to_sign = format!(
        "{ALGORITHM}\n{}\n{scope}\n{}",
        r.date,
        hex(sha.hash(canonical.as_bytes()).as_ref())
    );
    let mut key = [b"AWS4".as_slice(), r.secret].concat();
    for part in [day, r.region, r.service, "aws4_request"] {
        let next = sha.hmac(&key, part.as_bytes()).as_ref().to_vec();
        key.iter_mut().for_each(|b| *b = 0);
        key = next;
    }
    let signature = hex(sha.hmac(&key, to_sign.as_bytes()).as_ref());
    key.iter_mut().for_each(|b| *b = 0);
    format!(
        "{ALGORITHM} Credential={}/{scope}, SignedHeaders={signed}, Signature={signature}",
        r.key_id
    )
}

/// The host of an `awsSTSUrl`, which goes to the client as `h`.
pub fn sts_host(url: &str) -> Result<String, AwsError> {
    let Some(rest) = url.strip_prefix("https://") else {
        return err("STS URL must start with https://");
    };
    let authority = rest.split('/').next().unwrap_or_default();
    let host = authority.rsplit_once(':').map_or(authority, |(h, _)| h);
    if host.is_empty() {
        return err("STS URL without a host");
    }
    Ok(host.to_owned())
}

fn document(payload: &[u8]) -> Result<Document, AwsError> {
    RawDoc::from_bytes(payload).map(Document::from).map_err(|e| AwsError(format!("{e:?}")))
}

/// The server's side of one conversation, between the first message and the STS's answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Server {
    nonce: Vec<u8>,
    flag: u8,
}

/// The request to send to the STS.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StsRequest {
    /// Headers besides `Host`, `Content-Type` and `Content-Length`, which the HTTP client sets.
    pub headers: Vec<(String, String)>,
    /// [`STS_BODY`].
    pub body: &'static str,
}

impl Server {
    /// Reads the client's first message and answers with the server nonce (the client's and
    /// `random`) and the STS host `host`.
    pub fn start(
        payload: &[u8],
        random: [u8; NONCE_LEN],
        host: &str,
    ) -> Result<(Server, Vec<u8>), AwsError> {
        let doc = document(payload)?;
        let (mut nonce, mut flag) = (None, None);
        for (k, v) in doc.iter() {
            match (k, v) {
                ("r", Value::Binary { bytes, .. }) if bytes.len() == NONCE_LEN => {
                    nonce = Some(bytes.clone());
                }
                ("r", Value::Binary { .. }) => return err("Nonce must be 32 bytes"),
                ("p", Value::Int32(p)) => match u8::try_from(*p).ok().filter(|f| FLAGS.contains(f))
                {
                    Some(f) => flag = Some(f),
                    None => return err("bad gs2-cb-flag"),
                },
                ("r" | "p", _) => return err(format!("BSON field 'sasl.{k}' is the wrong type")),
                (other, _) => {
                    return err(format!("BSON field 'sasl.{other}' is an unknown field."));
                }
            }
        }
        let missing = |f: &str| AwsError(format!("BSON field 'sasl.{f}' is missing"));
        let mut nonce = nonce.ok_or_else(|| missing("r"))?;
        let flag = flag.ok_or_else(|| missing("p"))?;
        nonce.extend_from_slice(&random);
        let reply = Document::new()
            .with("s", Value::Binary { subtype: 0, bytes: nonce.clone() })
            .with("h", host);
        let reply = reply.to_bytes().map_err(|e| AwsError(format!("{e:?}")))?;
        Ok((Server { nonce, flag }, reply))
    }

    /// Reads the client's second message: the request to send to the STS.
    pub fn request(&self, payload: &[u8]) -> Result<StsRequest, AwsError> {
        let doc = document(payload)?;
        let (mut auth, mut date, mut token) = (None, None, None);
        for (k, v) in doc.iter() {
            match (k, v) {
                ("a", Value::String(s)) => auth = Some(s.clone()),
                ("d", Value::String(s)) => date = Some(s.clone()),
                ("t", Value::String(s)) => token = Some(s.clone()),
                ("a" | "d" | "t", _) => {
                    return err(format!("BSON field 'sasl.{k}' is the wrong type"));
                }
                (other, _) => {
                    return err(format!("BSON field 'sasl.{other}' is an unknown field."));
                }
            }
        }
        let auth = auth.ok_or_else(|| AwsError("BSON field 'sasl.a' is missing".into()))?;
        let date = date.ok_or_else(|| AwsError("BSON field 'sasl.d' is missing".into()))?;
        check_authorization(&auth)?;
        let mut headers = vec![
            ("Authorization".to_owned(), auth),
            ("X-Amz-Date".to_owned(), date),
            (teseladb_names::AWS_HEADER_SERVER_NONCE.to_owned(), base64_encode(&self.nonce)),
            (
                teseladb_names::AWS_HEADER_CHANNEL_BINDING.to_owned(),
                char::from(self.flag).to_string(),
            ),
        ];
        if let Some(t) = token {
            headers.push(("X-Amz-Security-Token".to_owned(), t));
        }
        Ok(StsRequest { headers, body: STS_BODY })
    }
}

/// The parts of a SigV4 `Authorization` header the reference server checks before sending it.
fn check_authorization(auth: &str) -> Result<(), AwsError> {
    let bad = || AwsError("bad Authorization header".into());
    let rest = auth.strip_prefix(ALGORITHM).and_then(|r| r.strip_prefix(' ')).ok_or_else(bad)?;
    let mut parts = rest.split(", ");
    let credential = parts.next().and_then(|p| p.strip_prefix("Credential=")).ok_or_else(bad)?;
    let signed = parts.next().and_then(|p| p.strip_prefix("SignedHeaders=")).ok_or_else(bad)?;
    let signature = parts.next().and_then(|p| p.strip_prefix("Signature=")).ok_or_else(bad)?;
    if parts.next().is_some() || credential.split('/').count() != 5 || signature.is_empty() {
        return Err(bad());
    }
    let names: Vec<&str> = signed.split(';').collect();
    if let Some(m) = REQUIRED_SIGNED.iter().find(|r| !names.contains(r)) {
        return err(format!("{m} is not a signed header"));
    }
    Ok(())
}

/// The text of the first `<tag …>…</tag>` inside `xml`.
fn element<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}");
    let mut from = 0;
    while let Some(at) = xml[from..].find(&open) {
        let after = from + at + open.len();
        let next = xml[after..].chars().next()?;
        if next == '>' || next.is_ascii_whitespace() {
            let start = after + xml[after..].find('>')? + 1;
            let len = xml[start..].find(&format!("</{tag}>"))?;
            return Some(&xml[start..start + len]);
        }
        from = after;
    }
    None
}

/// The user of an STS answer with HTTP status `status`: the `Arn` of
/// `GetCallerIdentityResponse.GetCallerIdentityResult`, an assumed role named as the role with
/// any session (`…:assumed-role/<role>/*`), as the reference server names it.
pub fn caller(status: u16, body: &[u8]) -> Result<String, AwsError> {
    if status != 200 {
        return err(format!("Failed connecting to AWS STS. HTTP Status Code: {status}"));
    }
    let xml = std::str::from_utf8(body).map_err(|_| AwsError("not UTF-8".into()))?;
    let arn = element(xml, "GetCallerIdentityResponse")
        .and_then(|r| element(r, "GetCallerIdentityResult"))
        .and_then(|r| element(r, "Arn"))
        .filter(|a| !a.is_empty())
        .ok_or_else(|| AwsError("Failed to parse GetCallerIdentityResponse".into()))?;
    let (head, rest) = match arn.split_once(":assumed-role/") {
        Some((h, r)) => (h, r),
        None => return Ok(arn.to_owned()),
    };
    let role = rest.split('/').next().unwrap_or_default();
    Ok(format!("{head}:assumed-role/{role}/*"))
}
