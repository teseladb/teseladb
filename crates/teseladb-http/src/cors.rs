//! Cross-origin requests (019 FR-004): off unless `httpOptions.cors.allowedOrigins` lists
//! origins. A request whose `Origin` is listed gets `Access-Control-Allow-Origin` with that
//! origin; a preflight (`OPTIONS` with `Access-Control-Request-Method`) from a listed origin gets
//! the methods and fields the API takes. Any other origin gets no CORS field, and the browser
//! refuses the answer (Fetch standard, section 3.2).

/// The methods of the REST API.
pub const ALLOWED_METHODS: &str = "GET, POST, PUT, PATCH, DELETE";
/// The request fields the REST API reads.
pub const ALLOWED_HEADERS: &str = "Authorization, Content-Type, Accept, Idempotency-Key, \
     TeselaDB-After-Cluster-Time, Last-Event-ID, Mcp-Session-Id, Mcp-Protocol-Version";
/// The response fields a page may read.
pub const EXPOSED_HEADERS: &str = "TeselaDB-Operation-Time, Retry-After, Mcp-Session-Id";
/// How long a browser keeps a preflight's answer, in seconds.
pub const MAX_AGE_SECONDS: u32 = 600;

/// The origins allowed; none by default.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Cors {
    allowed: Vec<String>,
}

impl Cors {
    /// CORS for `origins` (`scheme://host[:port]`, compared without regard to case).
    pub fn new(origins: impl IntoIterator<Item = String>) -> Cors {
        Cors { allowed: origins.into_iter().map(|o| o.to_ascii_lowercase()).collect() }
    }

    /// Whether any origin is allowed.
    pub fn is_enabled(&self) -> bool {
        !self.allowed.is_empty()
    }

    fn allows(&self, origin: &str) -> bool {
        self.allowed.iter().any(|a| a.eq_ignore_ascii_case(origin))
    }

    /// Whether a request of `method` with fields `headers` is a preflight.
    pub fn is_preflight(method: &str, headers: &[(String, String)]) -> bool {
        method == "OPTIONS"
            && headers.iter().any(|(n, _)| n.eq_ignore_ascii_case("access-control-request-method"))
    }

    /// The CORS fields of the answer to a request from `origin` (its `Origin` field, if any);
    /// `preflight` for an `OPTIONS` preflight.
    pub fn response_headers(&self, origin: Option<&str>, preflight: bool) -> Vec<(String, String)> {
        let Some(origin) = origin.filter(|o| self.allows(o)) else { return Vec::new() };
        let field = |n: &str, v: &str| (n.to_owned(), v.to_owned());
        let mut out = vec![field("Access-Control-Allow-Origin", origin), field("Vary", "Origin")];
        match preflight {
            true => {
                out.push(field("Access-Control-Allow-Methods", ALLOWED_METHODS));
                out.push(field("Access-Control-Allow-Headers", ALLOWED_HEADERS));
                out.push(field("Access-Control-Max-Age", &MAX_AGE_SECONDS.to_string()));
            }
            false => out.push(field("Access-Control-Expose-Headers", EXPOSED_HEADERS)),
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(n: &str, v: &str) -> (String, String) {
        (n.to_owned(), v.to_owned())
    }

    #[test]
    fn off_by_default() {
        let c = Cors::default();
        assert!(!c.is_enabled());
        assert!(c.response_headers(Some("https://a.example"), false).is_empty());
    }

    #[test]
    fn a_listed_origin_gets_its_fields_and_others_none() {
        let c = Cors::new(["https://App.example".to_owned()]);
        assert!(c.is_enabled());
        let simple = c.response_headers(Some("https://app.example"), false);
        assert_eq!(simple[0], h("Access-Control-Allow-Origin", "https://app.example"));
        assert!(simple.contains(&h("Access-Control-Expose-Headers", EXPOSED_HEADERS)));
        let pre = c.response_headers(Some("https://app.example"), true);
        assert!(pre.contains(&h("Access-Control-Allow-Methods", ALLOWED_METHODS)));
        assert!(pre.contains(&h("Access-Control-Max-Age", "600")));
        assert!(c.response_headers(Some("https://evil.example"), true).is_empty());
        assert!(c.response_headers(None, false).is_empty());
    }

    #[test]
    fn a_preflight_is_an_options_with_a_requested_method() {
        let asked = [h("Access-Control-Request-Method", "POST")];
        assert!(Cors::is_preflight("OPTIONS", &asked));
        assert!(!Cors::is_preflight("OPTIONS", &[]));
        assert!(!Cors::is_preflight("POST", &asked));
    }
}
