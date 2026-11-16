//! Registration of JSON API paths in the routing table.
//!
//! A [`PathDescription`] describes one API operation: its method, base path, path
//! parameters and mandatory query parameters. [`PathDescription::set`] installs a handler
//! for it in [`Routes`] (a plain path entry, or a
//! [`MatchRule`] when the path has parameters) and remembers a cookie so
//! [`PathDescription::unset`] can remove exactly that handler later. Registration happens
//! on each shard that serves the API.

use std::cell::Cell;

use crate::engine::core::future::ExceptionPtr;
use crate::engine::core::sstring::Sstring;
use crate::engine::http::common::httpd::OperationType;
use crate::engine::http::function_handlers::{
    FunctionHandler, FutureJsonFunction, JsonRequestFunction,
};
use crate::engine::http::handlers::HandlerBase;
use crate::engine::http::matchrules::MatchRule;
use crate::engine::http::routes::{Routes, RuleCookie};

/// An operation of a path: an HTTP method and its nickname.
///
/// A path can have several operations, one per method.
#[derive(Clone, Debug)]
pub struct JsonOperation {
    /// The http method.
    pub method: OperationType,
    /// The nickname.
    pub nickname: Sstring,
}

impl Default for JsonOperation {
    /// A `GET` operation without a nickname.
    fn default() -> Self {
        JsonOperation { method: OperationType::Get, nickname: Sstring::new() }
    }
}

impl JsonOperation {
    /// Creates the operation of the HTTP method `method` with the nickname `nickname`.
    #[inline]
    pub fn new(method: OperationType, nickname: &Sstring) -> Self {
        JsonOperation { method, nickname: nickname.clone() }
    }
}

//
/// The kind of a url component.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum UrlComponentType {
    /// a normal path parameter (starts with / and end with / or end of path)
    #[default]
    Param,
    /// a parameter that contains all the path entil its end
    ParamUntilEndOfPath,
    /// a fixed string inside the path, must be a full match and does not count
    /// as a parameter
    FixedString,
}

/// path_part is either a parameter or a fixed string
#[derive(Clone, Debug, Default)]
pub struct PathPart {
    /// The parameter name or the fixed string.
    pub name: Sstring,
    /// The kind of the part.
    pub type_: UrlComponentType,
}

/// The description of a path of the API: its base path, its operation and its URL
/// components.
///
/// It maps a nickname to an operation, so an operation (path and method) can be defined
/// by its nickname. Each URL component is a path parameter, a path parameter that takes
/// the rest of the path, or a fixed string. The descriptions come from the Swagger JSON
/// definition of the API, through code generated at build time.
#[derive(Debug, Default)]
pub struct PathDescription {
    /// The url components after the base path.
    pub params: Vec<PathPart>,
    /// The base path.
    pub path: Sstring,
    /// The operation.
    pub operations: JsonOperation,
    /// The cookie of the rule installed by [`PathDescription::set`], kept in a `Cell` so
    /// `set` can record it through `&self`.
    pub _cookie: Cell<RuleCookie>,
    /// The mandatory query parameters.
    pub mandatory_queryparams: Vec<Sstring>,
}

impl PathDescription {
    /// Creates the description of the URL path `path` for the HTTP method `method`, with
    /// the nickname `nickname`.
    ///
    /// `path_parameters` lists the path parameters as (name, takes the rest of the path)
    /// pairs, and `mandatory_params` the names of the mandatory query parameters.
    pub fn new(
        path: &Sstring,
        method: OperationType,
        nickname: &Sstring,
        path_parameters: &[(Sstring, bool)],
        mandatory_params: &[Sstring],
    ) -> Self {
        let mut d = PathDescription {
            path: path.clone(),
            operations: JsonOperation::new(method, nickname),
            ..Default::default()
        };
        for man in mandatory_params {
            d.pushmandatory_param(man);
        }
        for (param, all_path) in path_parameters {
            d.pushparam(param, *all_path);
        }
        d
    }

    /// Creates the description of the URL path `path` for the HTTP method `method`, with
    /// the nickname `nickname`.
    ///
    /// `path_parameters` lists the URL components after the base path (parameters and
    /// fixed strings), and `mandatory_params` the names of the mandatory query
    /// parameters.
    pub fn with_parts(
        path: &Sstring,
        method: OperationType,
        nickname: &Sstring,
        path_parameters: &[PathPart],
        mandatory_params: &[Sstring],
    ) -> Self {
        let mut d = PathDescription {
            path: path.clone(),
            operations: JsonOperation::new(method, nickname),
            ..Default::default()
        };
        for man in mandatory_params {
            d.pushmandatory_param(man);
        }
        d.params.extend_from_slice(path_parameters);
        d
    }

    /// Appends the path parameter `param` to the description, and returns `self`.
    ///
    /// For a URL that should match `/file/{path}`, the constructor is followed by
    /// `pushparam("path", ..)`. The handler identifies the parameter by its name, which
    /// appears at most once in a description.
    ///
    /// With `all_path`, the parameter matches up to the end of the URL. That suits file
    /// paths: with the rule `/file/{path}` and the URL `/file/etc/hosts`, `path` is
    /// `/etc/hosts` and not only `/etc`.
    #[inline]
    pub fn pushparam(&mut self, param: &Sstring, all_path: bool) -> &mut Self {
        self.params.push(PathPart {
            name: param.clone(),
            type_: if all_path {
                UrlComponentType::ParamUntilEndOfPath
            } else {
                UrlComponentType::Param
            },
        });
        self
    }

    /// Appends the fixed string `url` to the path, and returns `self`; this combines fixed
    /// URL parts with path parameters.
    ///
    /// For example, `/mypath/{param1}/morepath/{param2}` is described by a description of
    /// `/mypath` followed by `pushparam("param1", false)`,
    /// `push_static_path_part("/morepath")` and `pushparam("param2", false)`.
    #[inline]
    pub fn push_static_path_part(&mut self, url: &Sstring) -> &mut Self {
        self.params.push(PathPart { name: url.clone(), type_: UrlComponentType::FixedString });
        self
    }

    /// Adds the mandatory query parameter `param`, checked before the handler is called,
    /// and returns `self`.
    #[inline]
    pub fn pushmandatory_param(&mut self, param: &Sstring) -> &mut Self {
        self.mandatory_queryparams.push(param.clone());
        self
    }

    /// Registers `handler` for this path in `_routes`, which takes ownership of it.
    pub fn set(
        &self,
        _routes: &mut Routes,
        mut handler: Box<dyn HandlerBase>,
    ) -> Result<(), ExceptionPtr> {
        for i in &self.mandatory_queryparams {
            handler.mandatory(i);
        }

        if self.params.is_empty() {
            // put() drops the handler when it refuses it.
            _routes.put(self.operations.method, self.path.as_str(), handler)?;
        } else {
            let mut rule = Box::new(MatchRule::new(handler));
            rule.add_str(&self.path);
            for i in &self.params {
                if i.type_ == UrlComponentType::FixedString {
                    rule.add_str(&i.name);
                } else {
                    rule.add_param(&i.name, i.type_ == UrlComponentType::ParamUntilEndOfPath);
                }
            }
            self._cookie.set(_routes.add_cookie(rule, self.operations.method));
        }
        Ok(())
    }

    /// Registers a json request function for this path.
    pub fn set_json_request_function(
        &self,
        _routes: &mut Routes,
        f: JsonRequestFunction,
    ) -> Result<(), ExceptionPtr> {
        self.set(_routes, Box::new(FunctionHandler::from_json_request_function(f)))
    }

    /// Registers a future json function for this path.
    pub fn set_future_json_function(
        &self,
        _routes: &mut Routes,
        f: FutureJsonFunction,
    ) -> Result<(), ExceptionPtr> {
        self.set(_routes, Box::new(FunctionHandler::from_future_json_function(f)))
    }

    /// Removes the handler of this path from `_routes` and deletes it.
    pub fn unset(&self, _routes: &mut Routes) {
        if self.params.is_empty() {
            // The handler the routes give back is dropped here.
            drop(_routes.drop(self.operations.method, self.path.as_str()));
        } else {
            // The rule removed from the routes is dropped here.
            drop(_routes.del_cookie(self._cookie.get(), self.operations.method));
        }
    }
}
