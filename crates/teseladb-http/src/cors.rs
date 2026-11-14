//! Cross-origin requests: off unless the consumer lists origins. A request whose `Origin` is listed
//! gets `Access-Control-Allow-Origin` with that origin; a preflight (`OPTIONS` with
//! `Access-Control-Request-Method`) from a listed origin gets the methods and fields the consumer
//! takes. Any other origin gets no CORS field, and the browser refuses the answer (Fetch standard,
//! section 3.2).

/// The methods a preflight allows unless the consumer gives others.
pub const DEFAULT_METHODS: &str = "GET, POST, PUT, PATCH, DELETE";
/// The request fields a preflight allows unless the consumer gives others.
pub const DEFAULT_ALLOWED_HEADERS: &str = "Authorization, Content-Type, Accept";
/// How long a browser keeps a preflight's answer unless the consumer says otherwise, in seconds.
pub const DEFAULT_MAX_AGE_SECONDS: u32 = 600;

/// The origins allowed (none by default) and what their requests may use.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cors {
    allowed: Vec<String>,
    methods: String,
    allowed_headers: String,
    exposed_headers: String,
    max_age_seconds: u32,
}

impl Default for Cors {
    fn default() -> Cors {
        Cors::new(Vec::new())
    }
}

impl Cors {
    /// CORS for `origins` (`scheme://host[:port]`, compared without regard to case), with the
    /// default methods and request fields and no exposed response field.
    pub fn new(origins: impl IntoIterator<Item = String>) -> Cors {
        Cors {
            allowed: origins.into_iter().map(|o| o.to_ascii_lowercase()).collect(),
            methods: DEFAULT_METHODS.to_owned(),
            allowed_headers: DEFAULT_ALLOWED_HEADERS.to_owned(),
            exposed_headers: String::new(),
            max_age_seconds: DEFAULT_MAX_AGE_SECONDS,
        }
    }

    /// The same CORS allowing the methods `methods` (a comma-separated list).
    #[must_use]
    pub fn with_methods(mut self, methods: &str) -> Cors {
        methods.clone_into(&mut self.methods);
        self
    }

    /// The same CORS allowing the request fields `headers` (a comma-separated list).
    #[must_use]
    pub fn with_allowed_headers(mut self, headers: &str) -> Cors {
        headers.clone_into(&mut self.allowed_headers);
        self
    }

    /// The same CORS letting a page read the response fields `headers` (a comma-separated list).
    #[must_use]
    pub fn with_exposed_headers(mut self, headers: &str) -> Cors {
        headers.clone_into(&mut self.exposed_headers);
        self
    }

    /// The same CORS with preflight answers kept `seconds` seconds.
    #[must_use]
    pub fn with_max_age(mut self, seconds: u32) -> Cors {
        self.max_age_seconds = seconds;
        self
    }

    /// Whether any origin is allowed.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        !self.allowed.is_empty()
    }

    fn allows(&self, origin: &str) -> bool {
        self.allowed.iter().any(|a| a.eq_ignore_ascii_case(origin))
    }

    /// Whether a request of `method` with fields `headers` is a preflight.
    #[must_use]
    pub fn is_preflight(method: &str, headers: &[(String, String)]) -> bool {
        method == "OPTIONS"
            && headers.iter().any(|(n, _)| n.eq_ignore_ascii_case("access-control-request-method"))
    }

    /// The CORS fields of the answer to a request from `origin` (its `Origin` field, if any);
    /// `preflight` for an `OPTIONS` preflight.
    #[must_use]
    pub fn response_headers(&self, origin: Option<&str>, preflight: bool) -> Vec<(String, String)> {
        let Some(origin) = origin.filter(|o| self.allows(o)) else {
            return Vec::new();
        };
        let field = |n: &str, v: &str| (n.to_owned(), v.to_owned());
        let mut out = vec![field("Access-Control-Allow-Origin", origin), field("Vary", "Origin")];
        if preflight {
            out.push(field("Access-Control-Allow-Methods", &self.methods));
            out.push(field("Access-Control-Allow-Headers", &self.allowed_headers));
            out.push(field("Access-Control-Max-Age", &self.max_age_seconds.to_string()));
        } else if !self.exposed_headers.is_empty() {
            out.push(field("Access-Control-Expose-Headers", &self.exposed_headers));
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
        assert_eq!(
            simple,
            vec![h("Access-Control-Allow-Origin", "https://app.example"), h("Vary", "Origin")]
        );
        let pre = c.response_headers(Some("https://app.example"), true);
        assert!(pre.contains(&h("Access-Control-Allow-Methods", DEFAULT_METHODS)));
        assert!(pre.contains(&h("Access-Control-Allow-Headers", DEFAULT_ALLOWED_HEADERS)));
        assert!(pre.contains(&h("Access-Control-Max-Age", "600")));
        // What the consumer gives replaces the defaults.
        let custom = c
            .clone()
            .with_methods("GET")
            .with_allowed_headers("X-Request")
            .with_exposed_headers("X-Answer")
            .with_max_age(5);
        let simple = custom.response_headers(Some("https://app.example"), false);
        assert!(simple.contains(&h("Access-Control-Expose-Headers", "X-Answer")));
        let pre = custom.response_headers(Some("https://app.example"), true);
        for field in [
            h("Access-Control-Allow-Methods", "GET"),
            h("Access-Control-Allow-Headers", "X-Request"),
            h("Access-Control-Max-Age", "5"),
        ] {
            assert!(pre.contains(&field), "{field:?} in {pre:?}");
        }
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
