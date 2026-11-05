//! `teseladb admin tenant create|list|drop` (007 FR-025, contract `admin-cli.md`): the routes of
//! the administration port under [`TENANTS`], what a node answers on them, and the command line's
//! part. A tenant goes only when it has no database and no user left.

use teseladb_meta::command::Group0Command;
use teseladb_meta::group0::Group0Error;
use teseladb_bson::{Document, Value};

use crate::admin::{AdminBackend, AdminError, Answer, NodeAdmin, json};
use crate::admin_http::{HttpRequest, HttpResponse, Method};
use crate::catalog_ops::{CatalogAnswer, CatalogReq};
use crate::logic::NodeMsg;

/// `GET`: the tenants; `POST /admin/tenants/<name>` creates one, and `/admin/tenants/<name>/drop`
/// drops it.
pub const TENANTS: &str = "/admin/tenants";
/// Last part of the path that drops a tenant.
pub const DROP: &str = "drop";

/// What `teseladb admin tenant` does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TenantAction {
    /// `tenant create <name>`.
    Create(String),
    /// `tenant list`.
    List,
    /// `tenant drop <name>`.
    Drop(String),
    /// `tenant limits <name> [--<limit> <value>]...`: shows them, or changes the ones given.
    Limits {
        /// The tenant.
        name: String,
        /// The options given, by name without `--`.
        set: Vec<(String, String)>,
    },
    /// `tenant access <name> [--allow <cidr>]... [--clear]`: shows the list, or replaces it.
    Access {
        /// The tenant.
        name: String,
        /// The sources `--allow` gives.
        allow: Vec<String>,
        /// `--clear`: the list empties.
        clear: bool,
    },
    /// `tenant suspend <name>`.
    Suspend(String),
    /// `tenant resume <name>`.
    Resume(String),
    /// `tenant usage <name>`.
    Usage(String),
}

/// The route of the port for `path`, if it is one of the tenants'.
pub fn route(path: &str, req: &HttpRequest, b: &dyn AdminBackend) -> Option<HttpResponse> {
    if path == TENANTS {
        return Some(match req.method {
            Method::Get => json(b.tenants()),
            Method::Post => HttpResponse::text(405, ""),
        });
    }
    let rest = path.strip_prefix(TENANTS)?.strip_prefix('/')?;
    let post = req.method == Method::Post;
    Some(match rest.split_once('/') {
        None if post => json(b.create_tenant(rest)),
        Some((name, DROP)) if post => json(b.drop_tenant(name)),
        None | Some((_, DROP)) => HttpResponse::text(405, ""),
        Some((name, op)) => {
            let body = match crate::admin::body_doc(req) {
                Ok(body) => body,
                Err(e) => return Some(json(Err(e))),
            };
            match crate::admin_tenant_state::op_of(req.method, op, body) {
                Some(Ok(op)) => json(b.tenant_state(name, op)),
                Some(Err(status)) => HttpResponse::text(status, ""),
                None => HttpResponse::text(404, ""),
            }
        }
    })
}

/// The options of `tenant limits`, as the command line names them.
pub const LIMIT_OPTIONS: [&str; 13] = [
    "storage-bytes",
    "max-connections",
    "ops-per-second",
    "max-api-keys",
    "max-time-ms",
    "max-concurrent-ops",
    "max-write-bytes-per-second",
    "max-databases",
    "max-collections",
    "max-search-indexes",
    "http",
    "mcp",
    "service-user",
];
/// The options of `tenant access`.
pub const ACCESS_OPTIONS: [&str; 2] = ["allow", "clear"];

/// The words and options of `teseladb admin` that name a tenant subcommand; an error for an option
/// that does not go with it.
pub fn parse(words: &[&str], opts: &[(String, String)]) -> Option<Result<TenantAction, String>> {
    let allow = |allowed: &[&str]| match opts.iter().find(|(k, _)| !allowed.contains(&k.as_str())) {
        Some((k, _)) => Err(format!("--{k} does not go with this subcommand")),
        None => Ok(()),
    };
    let name = |n: &&str| (*n).to_owned();
    Some(match words {
        ["tenant", "create", n] => allow(&[]).map(|()| TenantAction::Create(name(n))),
        ["tenant", "list"] => allow(&[]).map(|()| TenantAction::List),
        ["tenant", "drop", n] => allow(&[]).map(|()| TenantAction::Drop(name(n))),
        ["tenant", "suspend", n] => allow(&[]).map(|()| TenantAction::Suspend(name(n))),
        ["tenant", "resume", n] => allow(&[]).map(|()| TenantAction::Resume(name(n))),
        ["tenant", "usage", n] => allow(&[]).map(|()| TenantAction::Usage(name(n))),
        // A value an option does not take is a usage error, before anything is sent.
        ["tenant", "limits", n] => allow(&LIMIT_OPTIONS)
            .and_then(|()| limits_body(opts))
            .map(|_| TenantAction::Limits { name: name(n), set: opts.to_vec() }),
        ["tenant", "access", n] => allow(&ACCESS_OPTIONS).map(|()| TenantAction::Access {
            name: name(n),
            allow: opts.iter().filter(|(k, _)| k == "allow").map(|(_, v)| v.clone()).collect(),
            clear: opts.iter().any(|(k, _)| k == "clear"),
        }),
        _ => return None,
    })
}

/// The body of `tenant limits` for the options `set`.
fn limits_body(set: &[(String, String)]) -> Result<Document, String> {
    use crate::admin_tenant_state::field as f;
    let mut d = Document::new();
    for (k, v) in set {
        let number = || v.parse::<i64>().map_err(|_| format!("--{k}: not a whole number: {v}"));
        let on = || match v.as_str() {
            "on" => Ok(true),
            "off" => Ok(false),
            _ => Err(format!("--{k}: on or off, not {v}")),
        };
        let (field, value) = match k.as_str() {
            "http" => (f::HTTP_ENABLED, Value::Boolean(on()?)),
            "mcp" => (f::MCP_ENABLED, Value::Boolean(on()?)),
            "service-user" => {
                (f::SERVICE_USER, Value::Document(Document::new().with("user", v.as_str())))
            }
            "storage-bytes" => (f::STORAGE_BYTES, Value::Int64(number()?)),
            "max-connections" => (f::MAX_CONNECTIONS, Value::Int64(number()?)),
            "ops-per-second" => (f::OPS_PER_SECOND, Value::Int64(number()?)),
            "max-api-keys" => (f::MAX_API_KEYS, Value::Int64(number()?)),
            "max-time-ms" => (f::MAX_TIME_MS, Value::Int64(number()?)),
            "max-concurrent-ops" => (f::MAX_CONCURRENT_OPS, Value::Int64(number()?)),
            "max-write-bytes-per-second" => {
                (f::MAX_WRITE_BYTES_PER_SECOND, Value::Int64(number()?))
            }
            "max-databases" => (f::MAX_DATABASES, Value::Int64(number()?)),
            "max-collections" => (f::MAX_COLLECTIONS, Value::Int64(number()?)),
            _ => (f::MAX_SEARCH_INDEXES, Value::Int64(number()?)),
        };
        d.push(field, value);
    }
    Ok(d)
}

/// The request `teseladb admin` sends for `a`: method, target and body.
pub fn request(a: &TenantAction) -> Result<(Method, String, String), String> {
    use crate::admin_tenant_state::{ACCESS, LIMITS, RESUME, SUSPEND, USAGE, field};
    let at = |n: &str, op: &str| format!("{TENANTS}/{}/{op}", crate::admin::component(n));
    let encode = |d: &Document| {
        teseladb_rest::media::encode(d, teseladb_rest::media::Format::Relaxed)
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .map_err(|e| format!("{e:?}"))
    };
    Ok(match a {
        TenantAction::Create(n) => {
            (Method::Post, format!("{TENANTS}/{}", crate::admin::component(n)), String::new())
        }
        TenantAction::List => (Method::Get, TENANTS.to_owned(), String::new()),
        TenantAction::Drop(n) => (Method::Post, at(n, DROP), String::new()),
        TenantAction::Suspend(n) => (Method::Post, at(n, SUSPEND), String::new()),
        TenantAction::Resume(n) => (Method::Post, at(n, RESUME), String::new()),
        TenantAction::Usage(n) => (Method::Get, at(n, USAGE), String::new()),
        TenantAction::Limits { name, set } if set.is_empty() => {
            (Method::Get, at(name, LIMITS), String::new())
        }
        TenantAction::Limits { name, set } => {
            (Method::Post, at(name, LIMITS), encode(&limits_body(set)?)?)
        }
        TenantAction::Access { name, allow, clear: false } if allow.is_empty() => {
            (Method::Get, at(name, ACCESS), String::new())
        }
        TenantAction::Access { name, allow, .. } => {
            let all = allow.iter().map(|c| Value::String(c.clone())).collect();
            let body = Document::new().with(field::ALLOWED_SOURCES, Value::Array(all));
            (Method::Post, at(name, ACCESS), encode(&body)?)
        }
    })
}

/// A value as a line shows it: a string without its quotes.
fn plain(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => teseladb_wire::commands::display(other),
    }
}

/// What a line of `tenant list` adds after the name: each limit that is set, each switch that is
/// off, the service user and whether the tenant is suspended.
fn state_words(t: &Document) -> String {
    use crate::admin_tenant_state::field as f;
    let mut words = Vec::new();
    for (k, v) in t.iter() {
        match (k, v) {
            (f::HTTP_ENABLED | f::MCP_ENABLED, Value::Boolean(false)) => {
                words.push(format!("{k}=off"));
            }
            (f::SERVICE_USER, Value::Document(u)) => {
                words.push(format!("{k}={}", plain(u.get("user").unwrap_or(&Value::Null))));
            }
            (f::ALLOWED_SOURCES, Value::Array(all)) if !all.is_empty() => {
                let shown: Vec<String> = all.iter().map(plain).collect();
                words.push(format!("{k}={}", shown.join(",")));
            }
            // Relaxed Extended JSON reads a small number back as an int32.
            (_, Value::Int64(n)) if *n > 0 && k != "_id" => words.push(format!("{k}={n}")),
            (_, Value::Int32(n)) if *n > 0 && k != "_id" => words.push(format!("{k}={n}")),
            _ => {}
        }
    }
    if t.get(f::SUSPENDED) == Some(&Value::Boolean(true)) {
        words.push(f::SUSPENDED.to_owned());
    }
    words.iter().map(|w| format!("\t{w}")).collect()
}

/// The answer to `a` as text: a line per tenant, or what changed.
pub fn text(a: &TenantAction, doc: Option<&Document>) -> String {
    let show = |v: Option<&Value>| v.map_or_else(String::new, teseladb_wire::commands::display);
    match a {
        TenantAction::Create(n) => {
            format!("tenant {n}: {}\n", show(doc.and_then(|d| d.get("_id"))))
        }
        TenantAction::Drop(n) => format!("tenant {n} dropped\n"),
        TenantAction::Suspend(n) => format!("tenant {n} suspended\n"),
        TenantAction::Resume(n) => format!("tenant {n} resumed\n"),
        TenantAction::Limits { .. } | TenantAction::Usage(_) => doc.map_or_else(String::new, |d| {
            d.iter()
                .filter(|(k, _)| *k != "ok")
                .map(|(k, v)| format!("{k}\t{}\n", teseladb_wire::commands::display(v)))
                .collect()
        }),
        TenantAction::Access { .. } => {
            match doc.and_then(|d| d.get(crate::admin_tenant_state::field::ALLOWED_SOURCES)) {
                Some(Value::Array(all)) if !all.is_empty() => {
                    all.iter().map(|v| format!("{}\n", plain(v))).collect()
                }
                _ => "any source\n".to_owned(),
            }
        }
        TenantAction::List => {
            let Some(Value::Array(all)) = doc.and_then(|d| d.get("tenants")) else {
                return String::new();
            };
            all.iter()
                .filter_map(|t| match t {
                    Value::Document(t) => Some(format!(
                        "{}\t{}{}\n",
                        show(t.get("_id")),
                        match t.get("name") {
                            Some(Value::String(n)) => n.clone(),
                            other => show(other),
                        },
                        state_words(t)
                    )),
                    _ => None,
                })
                .collect()
        }
    }
}

/// The error of a tenant change Group 0 refused.
pub(crate) fn refusal(name: &str, e: Group0Error) -> AdminError {
    match e {
        Group0Error::TenantExists => AdminError::bad(format!("a tenant named '{name}' exists")),
        Group0Error::TenantNotFound => AdminError::not_found(format!("no tenant named '{name}'")),
        Group0Error::InvalidTenantName => AdminError::bad(format!(
            "'{name}' cannot name a tenant: 1 to {} bytes without a zero byte",
            teseladb_meta::tenants::MAX_NAME
        )),
        Group0Error::NotEmpty => {
            AdminError::bad(format!("tenant '{name}' still has databases or users"))
        }
        Group0Error::InvalidOptions => {
            AdminError::bad("the default tenant has no limits, access list or suspension")
        }
        Group0Error::NotEveryNodeRunsIt => AdminError::unavailable(
            "every node of the cluster has to run this version before tenants get limits",
        ),
        other => AdminError::unavailable(format!("Group 0 refused the change: {other:?}")),
    }
}

/// The tenants of node `a`'s copy of Group 0, with their states, and whether every node reads the
/// entries that change a state.
pub(crate) fn listed(a: &NodeAdmin) -> Result<(Vec<Listed>, bool), AdminError> {
    match a.ask(0, |tx| NodeMsg::Catalog(CatalogReq::Tenants, tx)) {
        Some(CatalogAnswer::Tenants(all, ready)) => Ok((all, ready)),
        _ => Err(AdminError::unavailable("Group 0 did not answer")),
    }
}

/// A tenant of the list: its number, its name and its state.
pub(crate) type Listed = (u32, String, teseladb_meta::tenants::TenantState);

/// Proposes tenant change `cmd` of tenant `name` through node `a`.
pub(crate) fn change(a: &NodeAdmin, name: &str, cmd: Group0Command) -> Result<(), AdminError> {
    match a.ask(0, |tx| NodeMsg::Catalog(CatalogReq::Security(cmd), tx)) {
        Some(CatalogAnswer::Done(_)) => Ok(()),
        Some(CatalogAnswer::Refused(e)) => Err(refusal(name, e)),
        _ => Err(AdminError::unavailable("Group 0 did not answer")),
    }
}

/// `{tenants: [{_id, name, storageBytes, …, suspended, allowedSources}]}`.
pub fn list(a: &NodeAdmin) -> Answer {
    let all = listed(a)?
        .0
        .into_iter()
        .map(|(id, n, state)| {
            let head = Document::new().with("_id", i64::from(id)).with("name", n);
            let mut d = head;
            for (k, v) in crate::admin_tenant_state::state_doc(&state).iter() {
                d.push(k, v.clone());
            }
            Value::Document(d)
        })
        .collect();
    Ok(Document::new().with("tenants", Value::Array(all)).with("ok", 1.0))
}

/// Creates tenant `name`: `{_id}`, its number.
pub fn create(a: &NodeAdmin, name: &str) -> Answer {
    change(a, name, Group0Command::CreateTenant { name: name.to_owned() })?;
    // The answer comes once this node's copy applied the change.
    let id = listed(a)?
        .0
        .into_iter()
        .find(|(_, n, _)| n == name)
        .map(|(id, _, _)| id)
        .ok_or_else(|| AdminError::unavailable("the tenant is not in this node's copy yet"))?;
    Ok(Document::new().with("_id", i64::from(id)).with("ok", 1.0))
}

/// Drops tenant `name`.
pub fn drop(a: &NodeAdmin, name: &str) -> Answer {
    change(a, name, Group0Command::DropTenant { name: name.to_owned() })?;
    Ok(Document::new().with("ok", 1.0))
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn the_words_and_answers_of_the_command_line() {
        let none: Vec<(String, String)> = Vec::new();
        let opt = |k: &str, v: &str| (k.to_owned(), v.to_owned());
        assert_eq!(parse(&["tenant", "list"], &none), Some(Ok(TenantAction::List)));
        assert_eq!(parse(&["tenant", "create"], &none), None);
        let refused = parse(&["tenant", "list"], &[opt("allow", "x")]);
        assert_eq!(refused, Some(Err("--allow does not go with this subcommand".into())));
        let (m, t, _) = request(&TenantAction::Drop("a b".into())).unwrap();
        assert_eq!((m, t.as_str()), (Method::Post, "/admin/tenants/a%20b/drop"));
        assert_eq!(text(&TenantAction::List, None), "");
        let odd = Document::new().with("tenants", Value::Array(vec![Value::Int32(1)]));
        assert_eq!(text(&TenantAction::List, Some(&odd)), "");
        let e = refusal("x", Group0Error::InvalidTenantName);
        assert_eq!(e.message, "'x' cannot name a tenant: 1 to 64 bytes without a zero byte");
        assert_eq!(refusal("x", Group0Error::NoSuchGroup).status, 503);
        assert_eq!(refusal("x", Group0Error::InvalidOptions).status, 400);
        assert_eq!(refusal("x", Group0Error::NotEveryNodeRunsIt).status, 503);
    }

    #[test]
    fn limits_access_and_suspension_from_the_command_line() {
        let opt = |k: &str, v: &str| (k.to_owned(), v.to_owned());
        let set = vec![
            opt("storage-bytes", "1024"),
            opt("max-connections", "5"),
            opt("ops-per-second", "6"),
            opt("max-api-keys", "7"),
            opt("max-time-ms", "8"),
            opt("max-concurrent-ops", "9"),
            opt("max-write-bytes-per-second", "10"),
            opt("max-databases", "11"),
            opt("max-collections", "12"),
            opt("max-search-indexes", "13"),
            opt("http", "off"),
            opt("mcp", "on"),
            opt("service-user", "svc.x"),
        ];
        let limits = parse(&["tenant", "limits", "acme"], &set).unwrap().unwrap();
        let (m, t, body) = request(&limits).unwrap();
        assert_eq!((m, t.as_str()), (Method::Post, "/admin/tenants/acme/limits"));
        let doc =
            teseladb_rest::media::decode(body.as_bytes(), teseladb_rest::media::Format::Relaxed)
                .unwrap();
        let l = crate::admin_tenant_state::merge(&Default::default(), &doc).unwrap();
        assert_eq!((l.storage_bytes, l.max_search_indexes, l.http_enabled), (1024, 13, false));
        let show = TenantAction::Limits { name: "acme".into(), set: Vec::new() };
        assert_eq!(request(&show).unwrap().0, Method::Get);
        for bad in [opt("max-connections", "x"), opt("http", "maybe")] {
            let a = TenantAction::Limits { name: "acme".into(), set: vec![bad] };
            assert!(request(&a).is_err());
        }
        let refused = parse(&["tenant", "limits", "acme"], &[opt("allow", "x")]).unwrap();
        assert!(refused.is_err());
        let access =
            parse(&["tenant", "access", "acme"], &[opt("allow", "10.0.0.0/8"), opt("clear", "")]);
        let access = access.unwrap().unwrap();
        assert_eq!(request(&access).unwrap().1, "/admin/tenants/acme/access");
        let read = TenantAction::Access { name: "acme".into(), allow: Vec::new(), clear: false };
        assert_eq!(request(&read).unwrap().0, Method::Get);
        let clear = TenantAction::Access { name: "acme".into(), allow: Vec::new(), clear: true };
        assert!(request(&clear).unwrap().2.contains("allowedSources"));
        for (words, path) in [
            (["tenant", "suspend", "a"], "/admin/tenants/a/suspend"),
            (["tenant", "resume", "a"], "/admin/tenants/a/resume"),
        ] {
            let a = parse(&words, &[]).unwrap().unwrap();
            assert_eq!(request(&a).unwrap().1, path);
        }
        assert_eq!(text(&TenantAction::Suspend("a".into()), None), "tenant a suspended\n");
        assert_eq!(text(&TenantAction::Resume("a".into()), None), "tenant a resumed\n");
        assert_eq!(text(&read, None), "any source\n");
        let listed = Document::new().with(
            crate::admin_tenant_state::field::ALLOWED_SOURCES,
            Value::Array(vec![Value::String("10.0.0.0/8".into())]),
        );
        assert_eq!(text(&read, Some(&listed)), "10.0.0.0/8\n");
        let shown = Document::new().with("maxConnections", 5_i64).with("ok", 1.0);
        assert_eq!(text(&show, Some(&shown)), "maxConnections\t5\n");
        assert_eq!(text(&show, None), "");
        let state = teseladb_meta::tenants::TenantState {
            suspended: true,
            allowed_sources: vec!["10.0.0.0/8".into()],
            limits: l,
        };
        let row = Document::new().with("_id", 1_i64).with("name", "acme");
        let mut row = row;
        for (k, v) in crate::admin_tenant_state::state_doc(&state).iter() {
            row.push(k, v.clone());
        }
        let all = Document::new().with("tenants", Value::Array(vec![Value::Document(row)]));
        let line = text(&TenantAction::List, Some(&all));
        assert!(line.starts_with("1\tacme\tstorageBytes=1024\t"), "{line}");
        assert!(
            line.contains("\thttpEnabled=off\t") && line.contains("serviceUser=\"svc.x\"")
                || line.contains("serviceUser=svc.x"),
            "{line}"
        );
        assert!(line.contains("\tallowedSources=") && line.ends_with("\tsuspended\n"), "{line}");
    }
}
