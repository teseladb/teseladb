//! URL matchers: the pieces a [`MatchRule`](crate::engine::http::matchrules::MatchRule)
//! chains to match a request path.
//!
//! A [`Matcher`] checks the URL from a start offset and returns the end of what it
//! matched, or [`NPOS`] when it does not match. [`StrMatcher`] matches a fixed string;
//! [`ParamMatcher`] captures one path segment (or the rest of the URL) into the request
//! parameters. Matching runs on the shard that serves the request and allocates only to
//! store captured parameters.

use crate::engine::core::sstring::Sstring;
use crate::engine::http::common::httpd::Parameters;

/// The "no position" value a [`Matcher`] returns when the URL does not match.
pub const NPOS: usize = usize::MAX;

/// One part of a URL match rule: checks whether a URL matches a criterion at a position.
pub trait Matcher {
    /// Checks whether `url` matches from position `ind`, and returns the end of the
    /// matched part, or [`NPOS`] if it does not match.
    ///
    /// An implementation may add to `param` the parameters it matches, and only when it
    /// matches.
    fn match_(&self, url: &Sstring, ind: usize, param: &mut Parameters) -> usize;
}

/// A [`Matcher`] of a path parameter: stores the matched part of the URL in the
/// parameters under its name.
///
/// Any non-empty part matches. With the rule `/file/{path}`, this matcher handles the
/// `/{path}` part: for the URL `/file/etc/hosts` it receives `/etc/hosts`, and matches
/// `/etc/hosts` when it takes the entire path, or `/etc` when it does not. An empty part
/// matches only when the matcher takes the entire path.
pub struct ParamMatcher {
    _name: Sstring,
    _entire_path: bool,
}

impl ParamMatcher {
    /// Creates a matcher of the parameter `name`, the key of its value in the parameters.
    ///
    /// With `entire_path`, the parameter takes the rest of the URL; otherwise it ends at
    /// the next `/`.
    pub fn new(name: &Sstring, entire_path: bool) -> Self {
        ParamMatcher { _name: name.clone(), _entire_path: entire_path }
    }
}

/// A [`Matcher`] of a fixed string, which must be followed by `/` or the end of the URL.
///
/// With the rule `/file/{path}`, this matcher handles the `/file` part.
pub struct StrMatcher {
    _cmp: Sstring,
    _len: u32,
}

impl StrMatcher {
    /// Creates a matcher of the string `cmp`.
    pub fn new(cmp: &Sstring) -> Self {
        StrMatcher { _cmp: cmp.clone(), _len: cmp.size() as u32 }
    }
}

/// Returns the end of the URL parameter that starts at position `ind` of `url`.
///
/// With `entire_path` the parameter takes the rest of the URL; otherwise it ends at the
/// next `/` after `ind` (or at the end of the URL).
#[inline]
fn find_end_param(url: &Sstring, ind: usize, entire_path: bool) -> usize {
    let pos = if entire_path {
        url.size()
    } else {
        url.as_bytes()
            .get(ind + 1..)
            .and_then(|s| s.iter().position(|&c| c == b'/'))
            .map_or(NPOS, |p| p + ind + 1)
    };
    if pos == NPOS {
        return url.size();
    }
    pos
}

impl Matcher for ParamMatcher {
    fn match_(&self, url: &Sstring, ind: usize, param: &mut Parameters) -> usize {
        let last = find_end_param(url, ind, self._entire_path);
        if last == ind {
            /*
             * empty parameter allows only for the case of entire_path
             */
            if self._entire_path {
                param.set(&self._name, &Sstring::new());
                return ind;
            }
            return NPOS;
        }
        // `ind` and `last` are positions inside `url`, so `substr` does not throw.
        let Ok(value) = url.substr(ind, last - ind) else {
            return NPOS;
        };
        param.set(&self._name, &value);
        last
    }
}

impl Matcher for StrMatcher {
    fn match_(&self, url: &Sstring, ind: usize, _param: &mut Parameters) -> usize {
        let len = self._len as usize;
        let u = url.as_bytes();
        // The URL holds the string at `ind`, followed by the end or a slash.
        if u.len() >= len + ind
            && u[ind..].starts_with(self._cmp.as_bytes())
            && (u.len() == len + ind || u[len + ind] == b'/')
        {
            return len + ind;
        }
        NPOS
    }
}
