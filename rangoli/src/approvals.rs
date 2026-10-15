//! Human approval for agent writes. A token made with "ask me first" (`TokenScope::needs_approval`)
//! doesn't change data: each create, update or delete it sends, over the JSON API or MCP, is
//! validated and stored as a pending change, and the caller gets `202 {"pending": true}`.
//! Staff see the queue in the admin under **Pending changes** and approve or reject them there;
//! an approved change runs through the JSON API as the user who asked, with every permission
//! checked again, and its History entry names who approved it.

use crate::auth::{CurrentUser, TokenScope, User};
use crate::orm::Model;
use crate::{DateTime, Error, Result};
use std::sync::OnceLock;

#[derive(rangoli_macros::Choices, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApprovalStatus {
    Pending,
    Approved,
    Rejected,
    /// Approved, but the change no longer applied (the row is gone, a value is now invalid).
    Failed,
}

/// One write waiting for a person, as stored in `rangoli_pending_change`.
#[derive(rangoli_macros::Model, Clone, Debug)]
#[model(table = "rangoli_pending_change", display = "summary", index(status, created_at))]
pub struct PendingChange {
    pub id: Option<i64>,
    /// The user the agent acts for; the change runs as them once approved.
    #[field(fk = User, cascade)]
    pub user_id: i64,
    /// The token's name ("Claude on my laptop"), so reviewers know which agent asked.
    #[field(max_length = 100)]
    pub token: String,
    /// "Add a blog post", "Change blog post 7".
    #[field(max_length = 200)]
    pub summary: String,
    #[field(max_length = 100)]
    pub table_name: String,
    /// create, update (PATCH), replace (PUT) or delete.
    #[field(max_length = 10)]
    pub action: String,
    pub object_id: Option<i64>,
    /// The validated request body, as JSON.
    #[field(text)]
    pub payload: String,
    /// How it arrived: "MCP" or "the API".
    #[field(max_length = 20)]
    pub via: String,
    #[field(choices, max_length = 16)]
    pub status: ApprovalStatus,
    #[field(auto_now_add)]
    pub created_at: DateTime,
    pub decided_by: Option<i64>,
    pub decided_at: Option<DateTime>,
    /// What applying it returned: the saved row, or why it failed.
    #[field(text)]
    pub result: Option<String>,
}

/// The JSON API router approved changes replay through; `App::router` sets it.
static API: OnceLock<axum::Router> = OnceLock::new();

pub(crate) fn set_api(router: axum::Router) {
    let _ = API.set(router);
}

fn summary(action: &str, table: &str, id: Option<i64>) -> String {
    let noun = table.replace('_', " ");
    match (action, id) {
        ("create", _) => format!("Add a {noun}"),
        ("delete", Some(id)) => format!("Delete {noun} {id}"),
        (_, Some(id)) => format!("Change {noun} {id}"),
        _ => format!("Change a {noun}"),
    }
}

/// Store a write for approval; returns its id.
pub(crate) async fn queue(
    user: &User,
    scope: &TokenScope,
    via: Option<&str>,
    table: &str,
    action: &str,
    object_id: Option<i64>,
    payload: Option<&serde_json::Value>,
) -> Result<i64> {
    let mut row = PendingChange {
        id: None,
        user_id: user.id.ok_or(Error::NotFound)?,
        token: scope.token.chars().take(100).collect(),
        summary: summary(action, table, object_id),
        table_name: table.into(),
        action: action.into(),
        object_id,
        payload: payload.map_or_else(String::new, |p| p.to_string()),
        via: via.unwrap_or("the API").chars().take(20).collect(),
        status: ApprovalStatus::Pending,
        created_at: DateTime::now(),
        decided_by: None,
        decided_at: None,
        result: None,
    };
    row.save().await?;
    Ok(row.id.unwrap_or_default())
}

/// The admin user running the current action (set around admin actions).
pub(crate) fn approver() -> Option<User> {
    crate::admin::acting_user()
}

/// Apply pending changes as their requesters, through the JSON API. Already decided ones are skipped.
pub async fn approve(changes: Vec<PendingChange>) -> Result<()> {
    use tower::ServiceExt;
    let by = approver().ok_or_else(|| Error::Config("approving needs a signed-in admin".into()))?;
    let api = API.get().ok_or_else(|| Error::Config("no JSON API is mounted".into()))?;
    for mut change in changes.into_iter().filter(|c| c.status == ApprovalStatus::Pending) {
        let (method, path) = match (change.action.as_str(), change.object_id) {
            ("create", _) => ("POST", format!("/api/{}/", change.table_name)),
            ("update", Some(id)) => ("PATCH", format!("/api/{}/{id}", change.table_name)),
            ("replace", Some(id)) => ("PUT", format!("/api/{}/{id}", change.table_name)),
            ("delete", Some(id)) => ("DELETE", format!("/api/{}/{id}", change.table_name)),
            _ => return Err(Error::Config(format!("pending change {:?} is malformed", change.id))),
        };
        // The requester's rights are checked again now: a deactivated user's changes fail.
        let user = User::objects().filter(User::ID.eq(change.user_id) & User::IS_ACTIVE.eq(true)).first().await?;
        let mut req = axum::http::Request::builder().method(method).uri(path).header("content-type", "application/json");
        if let Some(ext) = req.extensions_mut() {
            ext.insert(CurrentUser(user));
            ext.insert(crate::api::Via(format!("{}, approved by {}", change.via, by.username)));
        }
        let body = if change.payload.is_empty() { axum::body::Body::empty() } else { axum::body::Body::from(change.payload.clone()) };
        let req = req.body(body).map_err(|e| Error::Config(e.to_string()))?;
        let res = match api.clone().oneshot(req).await {
            Ok(res) => res,
            Err(never) => match never {},
        };
        let ok = res.status().is_success();
        let bytes = axum::body::to_bytes(res.into_body(), 1024 * 1024).await.unwrap_or_default();
        change.status = if ok { ApprovalStatus::Approved } else { ApprovalStatus::Failed };
        change.result = Some(String::from_utf8_lossy(&bytes).chars().take(10_000).collect());
        change.decided_by = by.id;
        change.decided_at = Some(DateTime::now());
        change.save().await?;
    }
    Ok(())
}

/// What a pending change would do, as an HTML table for its admin page: the row now, and the
/// row after, from a dry run of the change as its requester (so defaults, row rules and
/// validation are real, and a change that would fail says why), changed fields highlighted.
pub async fn preview_html(change: PendingChange) -> Result<String> {
    use tower::ServiceExt;
    let Some(api) = API.get() else { return Ok(String::new()) };
    let user = User::objects().filter(User::ID.eq(change.user_id) & User::IS_ACTIVE.eq(true)).first().await?;
    let call = |method: &'static str, path: String, body: Option<String>| {
        let user = user.clone();
        let api = api.clone();
        async move {
            let mut req = axum::http::Request::builder().method(method).uri(path).header("content-type", "application/json");
            if let Some(ext) = req.extensions_mut() {
                ext.insert(CurrentUser(user));
            }
            let req = req.body(body.map_or_else(axum::body::Body::empty, axum::body::Body::from)).map_err(|e| Error::Config(e.to_string()))?;
            let res = match api.oneshot(req).await {
                Ok(r) => r,
                Err(never) => match never {},
            };
            let ok = res.status().is_success();
            let bytes = axum::body::to_bytes(res.into_body(), 1024 * 1024).await.unwrap_or_default();
            Ok::<_, Error>((ok, serde_json::from_slice::<serde_json::Value>(&bytes).unwrap_or_default()))
        }
    };
    let t = &change.table_name;
    let now = match change.object_id {
        Some(id) => call("GET", format!("/api/{t}/{id}"), None).await?.1,
        None => serde_json::Value::Null,
    };
    let (method, path) = match (change.action.as_str(), change.object_id) {
        ("create", _) => ("POST", format!("/api/{t}/?dry_run=1")),
        ("update", Some(id)) => ("PATCH", format!("/api/{t}/{id}?dry_run=1")),
        ("replace", Some(id)) => ("PUT", format!("/api/{t}/{id}?dry_run=1")),
        ("delete", Some(id)) => ("DELETE", format!("/api/{t}/{id}?dry_run=1")),
        _ => return Ok("<p>This change is malformed.</p>".into()),
    };
    let body = (!change.payload.is_empty()).then(|| change.payload.clone());
    let (ok, report) = call(method, path, body).await?;
    let esc = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;");
    let show = |v: Option<&serde_json::Value>| match v {
        None | Some(serde_json::Value::Null) => "<em>empty</em>".to_string(),
        Some(serde_json::Value::String(s)) => esc(s),
        Some(other) => esc(&other.to_string()),
    };
    if !ok {
        return Ok(format!("<p class=\"field-error\">Approving this would fail right now: <code>{}</code></p>", esc(&report.to_string())));
    }
    let after = report.get("would_be").cloned().unwrap_or(serde_json::Value::Null);
    let deleting = report.get("would_delete").is_some();
    let mut keys: Vec<String> = now.as_object().into_iter().flatten().chain(after.as_object().into_iter().flatten()).map(|(k, _)| k.clone()).collect();
    keys.sort();
    keys.dedup();
    let rows: String = keys
        .iter()
        .filter(|k| k.as_str() != "id")
        .map(|k| {
            let (a, b) = (now.get(k), if deleting { None } else { after.get(k) });
            let changed = a != b;
            format!("<tr{}><th scope=\"row\">{}</th><td>{}</td><td>{}</td></tr>", if changed { " class=\"changed\"" } else { "" }, esc(k), show(a), show(b))
        })
        .collect();
    let head = if deleting { "This deletes the row." } else if change.object_id.is_none() { "This adds a row." } else { "This changes the row." };
    Ok(format!("<p>{head} Changed fields are highlighted.</p><table class=\"diff\"><thead><tr><th scope=\"col\">Field</th><th scope=\"col\">Now</th><th scope=\"col\">After</th></tr></thead><tbody>{rows}</tbody></table>"))
}

/// Turn pending changes down. Already decided ones are skipped.
pub async fn reject(changes: Vec<PendingChange>) -> Result<()> {
    let by = approver().ok_or_else(|| Error::Config("rejecting needs a signed-in admin".into()))?;
    for mut change in changes.into_iter().filter(|c| c.status == ApprovalStatus::Pending) {
        change.status = ApprovalStatus::Rejected;
        change.decided_by = by.id;
        change.decided_at = Some(DateTime::now());
        change.save().await?;
    }
    Ok(())
}
