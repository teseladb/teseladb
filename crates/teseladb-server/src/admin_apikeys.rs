//! `teseladb admin apikey create|list|revoke` (019 FR-016, contract `rest-api.md`): the routes of
//! the administration port under [`API_KEYS`], which run the commands of
//! [`teseladb_wire::commands::api_keys`], and the command line's part. `create` prints the key
//! once; `list` shows only its first characters.

use teseladb_bson::{Document, Value};
use teseladb_wire::commands::api_keys::{CREATE_API_KEY, LIST_API_KEYS, REVOKE_API_KEY};

use crate::admin::{AdminBackend, AdminError, body_doc, component, json};
use crate::admin_http::{HttpRequest, HttpResponse, Method};

/// `GET`: the keys (`?user=`); `POST` with `{user, db, roles, expiresAfterDays}` creates one,
/// and `POST /admin/apiKeys/<keyId>/revoke` revokes it.
pub const API_KEYS: &str = "/admin/apiKeys";
/// Last part of the path that revokes a key.
pub const REVOKE: &str = "revoke";

/// What `teseladb admin apikey` does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApiKeyAction {
    /// `apikey create --user <u> --db <db> [--role <role>@<db>]... [--expires-after-days <n>]`.
    Create {
        /// The user.
        user: String,
        /// Its database.
        db: String,
        /// `role@db` of each `--role`; none: the user's roles.
        roles: Vec<String>,
        /// `--expires-after-days`.
        days: Option<i64>,
    },
    /// `apikey list [--user <u>]`.
    List {
        /// Only this user's.
        user: Option<String>,
    },
    /// `apikey revoke <keyId>`.
    Revoke(String),
}

/// The options `apikey` takes.
pub const OPTIONS: [&str; 4] = ["user", "db", "role", "expires-after-days"];

/// The words and options of `teseladb admin` that name an `apikey` subcommand.
pub fn parse(words: &[&str], opts: &[(String, String)]) -> Option<Result<ApiKeyAction, String>> {
    let all = |n: &str| -> Vec<String> {
        opts.iter().filter(|(k, _)| k == n).map(|(_, v)| v.clone()).collect()
    };
    let one = |n: &str| all(n).pop();
    let allow = |allowed: &[&str]| match opts.iter().find(|(k, _)| !allowed.contains(&k.as_str())) {
        Some((k, _)) => Err(format!("--{k} does not go with this subcommand")),
        None => Ok(()),
    };
    Some(match words {
        ["apikey", "create"] => allow(&OPTIONS).and_then(|()| {
            let user = one("user").ok_or("apikey create needs --user")?;
            let db = one("db").ok_or("apikey create needs --db")?;
            let days = match one("expires-after-days") {
                Some(d) => {
                    Some(d.parse::<i64>().map_err(|_| format!("not a number of days: {d}"))?)
                }
                None => None,
            };
            for r in all("role") {
                if !r.contains('@') {
                    return Err(format!("--role takes <role>@<db>, not {r}"));
                }
            }
            Ok(ApiKeyAction::Create { user, db, roles: all("role"), days })
        }),
        ["apikey", "list"] => allow(&["user"]).map(|()| ApiKeyAction::List { user: one("user") }),
        ["apikey", "revoke", id] => allow(&[]).map(|()| ApiKeyAction::Revoke((*id).to_owned())),
        _ => return None,
    })
}

/// `role@db` as `{role, db}`; the database is after the last `@`.
fn role_value(text: &str) -> Value {
    let (role, db) = text.rsplit_once('@').unwrap_or((text, ""));
    Value::Document(Document::new().with("role", role).with("db", db))
}

/// The request `teseladb admin` sends for `a`: method, target and body.
pub fn request(a: &ApiKeyAction) -> Result<(Method, String, String), String> {
    Ok(match a {
        ApiKeyAction::Create { user, db, roles, days } => {
            let mut d = Document::new().with("user", user.as_str()).with("db", db.as_str());
            if !roles.is_empty() {
                d.push("roles", Value::Array(roles.iter().map(|r| role_value(r)).collect()));
            }
            if let Some(n) = days {
                d.push("expiresAfterDays", *n);
            }
            let body = teseladb_rest::media::encode(&d, teseladb_rest::media::Format::Relaxed)
                .map_err(|e| format!("{e:?}"))?;
            (Method::Post, API_KEYS.to_owned(), String::from_utf8_lossy(&body).into_owned())
        }
        ApiKeyAction::List { user: None } => (Method::Get, API_KEYS.to_owned(), String::new()),
        ApiKeyAction::List { user: Some(u) } => {
            (Method::Get, format!("{API_KEYS}?user={}", component(u)), String::new())
        }
        ApiKeyAction::Revoke(id) => {
            (Method::Post, format!("{API_KEYS}/{}/{REVOKE}", component(id)), String::new())
        }
    })
}

/// The route of the port for `path`, if it is one of the API keys'.
pub fn route(path: &str, req: &HttpRequest, b: &dyn AdminBackend) -> Option<HttpResponse> {
    if path == API_KEYS {
        return Some(match req.method {
            Method::Get => {
                let mut cmd = Document::new().with(LIST_API_KEYS, 1);
                if let Some(u) = req.param("user") {
                    cmd.push("user", u);
                }
                json(b.api_keys(&cmd))
            }
            Method::Post => json(create_command(req).and_then(|cmd| b.api_keys(&cmd))),
        });
    }
    let rest = path.strip_prefix(API_KEYS)?.strip_prefix('/')?;
    Some(match (req.method, rest.split_once('/')) {
        (Method::Post, Some((id, REVOKE))) => {
            json(b.api_keys(&Document::new().with(REVOKE_API_KEY, id)))
        }
        (Method::Get, _) => HttpResponse::text(405, ""),
        (Method::Post, _) => HttpResponse::text(404, ""),
    })
}

/// `createApiKey` from the body `{user, db, roles, expiresAfterDays}`.
fn create_command(req: &HttpRequest) -> Result<Document, AdminError> {
    let body = body_doc(req)?.ok_or_else(|| AdminError::bad("the key's user is missing"))?;
    let Some(Value::String(user)) = body.get("user") else {
        return Err(AdminError::bad("user must be a string"));
    };
    let mut cmd = Document::new().with(CREATE_API_KEY, user.as_str());
    for (k, v) in body.iter().filter(|(k, _)| *k != "user") {
        cmd.push(k, v.clone());
    }
    Ok(cmd)
}

/// The answer to `a` as text: the key alone, a line per key, or what changed.
pub fn text(a: &ApiKeyAction, doc: Option<&Document>) -> String {
    let field = |d: &Document, f: &str| match d.get(f) {
        Some(Value::String(s)) => s.clone(),
        other => other.map_or_else(String::new, teseladb_wire::commands::display),
    };
    match a {
        ApiKeyAction::Create { .. } => {
            doc.map_or_else(String::new, |d| format!("{}\n", field(d, "key")))
        }
        ApiKeyAction::Revoke(id) => format!("API key {id} revoked\n"),
        ApiKeyAction::List { .. } => {
            let Some(Value::Array(all)) = doc.and_then(|d| d.get("keys")) else {
                return String::new();
            };
            all.iter()
                .filter_map(|k| match k {
                    Value::Document(k) => Some(format!(
                        "{}\t{}\t{}@{}\t{}\n",
                        field(k, "keyId"),
                        field(k, "prefix"),
                        field(k, "user"),
                        field(k, "db"),
                        field(k, "expiresAt"),
                    )),
                    _ => None,
                })
                .collect()
        }
    }
}
