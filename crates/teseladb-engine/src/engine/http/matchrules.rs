//! Parameterised routing rules.
//!
//! A [`MatchRule`] owns a handler and a chain of [`Matcher`]s; a URL matches the rule when
//! every matcher matches in turn and the chain consumes the whole URL. Captured path
//! parameters are written to the request [`Parameters`] only on a full match. The routing
//! table tries its rules in order on the shard that serves the request.

use crate::engine::core::sstring::Sstring;
use crate::engine::http::common::httpd::Parameters;
use crate::engine::http::handlers::HandlerBase;
use crate::engine::http::matcher::{Matcher, NPOS, ParamMatcher, StrMatcher};

/// A URL match rule: a sequence of [`Matcher`]s, which may match parameters, and the
/// handler to use when the whole sequence matches.
///
/// The routes call [`MatchRule::get`] with a URL; the rule returns its handler if the
/// URL matches, filling the parameters along the way.
pub struct MatchRule {
    _match_list: Vec<Box<dyn Matcher>>,
    _handler: *mut dyn HandlerBase,
}

impl MatchRule {
    /// Creates a rule, with no matchers yet, that returns `handler` when it matches; the
    /// rule owns the handler.
    pub fn new(handler: Box<dyn HandlerBase>) -> Self {
        MatchRule { _match_list: Vec::new(), _handler: Box::into_raw(handler) }
    }

    /// Returns the rule's handler if `url` matches the whole rule, or `None` if it does
    /// not.
    ///
    /// The matchers run in order, each from where the previous one ended, and the match
    /// must cover the URL (a trailing `/` may remain). Matched parameters are stored in
    /// `params`. A rule without matchers matches any URL.
    pub fn get(&self, url: &Sstring, params: &mut Parameters) -> Option<*mut dyn HandlerBase> {
        let mut ind: usize = 0;
        if self._match_list.is_empty() {
            return Some(self._handler);
        }
        for m in &self._match_list {
            ind = m.match_(url, ind, params);
            if ind == NPOS {
                return None;
            }
        }
        if ind + 1 >= url.size() { Some(self._handler) } else { None }
    }

    /// Appends the matcher `match_` to the rule, and returns `self`.
    pub fn add_matcher(&mut self, match_: Box<dyn Matcher>) -> &mut Self {
        self._match_list.push(match_);
        self
    }

    /// Appends a matcher of the fixed string `str` ([`StrMatcher`]), and returns `self`.
    pub fn add_str(&mut self, str: &Sstring) -> &mut Self {
        self.add_matcher(Box::new(StrMatcher::new(str)));
        self
    }

    /// Appends a matcher of the parameter `str` ([`ParamMatcher`]), and returns `self`.
    ///
    /// With `fullpath`, the parameter takes the rest of the URL.
    pub fn add_param(&mut self, str: &Sstring, fullpath: bool) -> &mut Self {
        self.add_matcher(Box::new(ParamMatcher::new(str, fullpath)));
        self
    }
}

impl Drop for MatchRule {
    fn drop(&mut self) {
        // SAFETY: the handler was leaked from a Box in new() and is owned by the rule.
        unsafe { drop(Box::from_raw(self._handler)) };
    }
}
