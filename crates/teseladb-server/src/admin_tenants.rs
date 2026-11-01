//! `teseladb admin tenant create|list|drop` (007 FR-025, contract `admin-cli.md`): the routes of
//! the administration port under [`TENANTS`], what a node answers on them, and the command line's
//! part. A tenant goes only when it has no database and no user left.

use teseladb_bson::{Document, Value};
use teseladb_meta::command::Group0Command;
use teseladb_meta::group0::Group0Error;

use crate::admin::{AdminBackend, AdminError, Answer, NodeAdmin, json};
use crate::catalog_ops::{CatalogAnswer, CatalogReq};
use crate::logic::NodeMsg;
use crate::metrics_http::{HttpRequest, HttpResponse, Method};

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
    if req.method != Method::Post {
        return Some(HttpResponse::text(405, ""));
    }
    Some(match rest.split_once('/') {
        None => json(b.create_tenant(rest)),
        Some((name, DROP)) => json(b.drop_tenant(name)),
        Some(_) => HttpResponse::text(404, ""),
    })
}

/// The words of `teseladb admin` that name a tenant subcommand.
pub fn parse(words: &[&str]) -> Option<TenantAction> {
    match words {
        ["tenant", "create", name] => Some(TenantAction::Create((*name).to_owned())),
        ["tenant", "list"] => Some(TenantAction::List),
        ["tenant", "drop", name] => Some(TenantAction::Drop((*name).to_owned())),
        _ => None,
    }
}

/// The request `teseladb admin` sends for `a`: method and target.
pub fn request(a: &TenantAction) -> (Method, String) {
    match a {
        TenantAction::Create(n) => {
            (Method::Post, format!("{TENANTS}/{}", crate::admin::component(n)))
        }
        TenantAction::List => (Method::Get, TENANTS.to_owned()),
        TenantAction::Drop(n) => {
            (Method::Post, format!("{TENANTS}/{}/{DROP}", crate::admin::component(n)))
        }
    }
}

/// The answer to `a` as text: a line per tenant, or what changed.
pub fn text(a: &TenantAction, doc: Option<&Document>) -> String {
    let show = |v: Option<&Value>| v.map_or_else(String::new, teseladb_wire::commands::display);
    match a {
        TenantAction::Create(n) => {
            format!("tenant {n}: {}\n", show(doc.and_then(|d| d.get("_id"))))
        }
        TenantAction::Drop(n) => format!("tenant {n} dropped\n"),
        TenantAction::List => {
            let Some(Value::Array(all)) = doc.and_then(|d| d.get("tenants")) else {
                return String::new();
            };
            all.iter()
                .filter_map(|t| match t {
                    Value::Document(t) => Some(format!(
                        "{}\t{}\n",
                        show(t.get("_id")),
                        match t.get("name") {
                            Some(Value::String(n)) => n.clone(),
                            other => show(other),
                        }
                    )),
                    _ => None,
                })
                .collect()
        }
    }
}

/// The error of a tenant change Group 0 refused.
fn refusal(name: &str, e: Group0Error) -> AdminError {
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
        other => AdminError::unavailable(format!("Group 0 refused the change: {other:?}")),
    }
}

/// The tenants of node `a`'s copy of Group 0.
fn listed(a: &NodeAdmin) -> Result<Vec<(u32, String)>, AdminError> {
    match a.ask(0, |tx| NodeMsg::Catalog(CatalogReq::Tenants, tx)) {
        Some(CatalogAnswer::Tenants(all)) => Ok(all),
        _ => Err(AdminError::unavailable("Group 0 did not answer")),
    }
}

/// Proposes tenant change `cmd` of tenant `name` through node `a`.
fn change(a: &NodeAdmin, name: &str, cmd: Group0Command) -> Result<(), AdminError> {
    match a.ask(0, |tx| NodeMsg::Catalog(CatalogReq::Security(cmd), tx)) {
        Some(CatalogAnswer::Done(_)) => Ok(()),
        Some(CatalogAnswer::Refused(e)) => Err(refusal(name, e)),
        _ => Err(AdminError::unavailable("Group 0 did not answer")),
    }
}

/// `{tenants: [{_id, name}]}`.
pub fn list(a: &NodeAdmin) -> Answer {
    let all = listed(a)?
        .into_iter()
        .map(|(id, n)| Value::Document(Document::new().with("_id", i64::from(id)).with("name", n)))
        .collect();
    Ok(Document::new().with("tenants", Value::Array(all)).with("ok", 1.0))
}

/// Creates tenant `name`: `{_id}`, its number.
pub fn create(a: &NodeAdmin, name: &str) -> Answer {
    change(a, name, Group0Command::CreateTenant { name: name.to_owned() })?;
    // The answer comes once this node's copy applied the change.
    let id = listed(a)?
        .into_iter()
        .find(|(_, n)| n == name)
        .map(|(id, _)| id)
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
        assert_eq!(parse(&["tenant", "list"]), Some(TenantAction::List));
        assert_eq!(parse(&["tenant", "create"]), None);
        let (m, t) = request(&TenantAction::Drop("a b".into()));
        assert_eq!((m, t.as_str()), (Method::Post, "/admin/tenants/a%20b/drop"));
        assert_eq!(text(&TenantAction::List, None), "");
        let odd = Document::new().with("tenants", Value::Array(vec![Value::Int32(1)]));
        assert_eq!(text(&TenantAction::List, Some(&odd)), "");
        let e = refusal("x", Group0Error::InvalidTenantName);
        assert_eq!(e.message, "'x' cannot name a tenant: 1 to 64 bytes without a zero byte");
        assert_eq!(refusal("x", Group0Error::NoSuchGroup).status, 503);
    }
}
