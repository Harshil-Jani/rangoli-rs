//! A Model Context Protocol server, so AI agents (Claude, Cursor, any MCP client) can work with
//! your data. Every model exposed through `App::api` becomes tools (`blog_post_list`,
//! `blog_post_get`, `blog_post_create`, `blog_post_update`, `blog_post_delete`) at `POST /mcp`,
//! over MCP's Streamable HTTP transport.
//!
//! An agent is just another API client: it connects with an API token
//! (`Authorization: Bearer rgl_…`), each tool call runs through the JSON API with that token's
//! user, and every access rule, `owned_by` scope and validation applies unchanged. Agents see
//! only the tools their user may call, and every change they make lands in the admin History
//! as "… via MCP", next to the admin's own.

use crate::api::{field_schema, Options, Via};
use crate::auth::CurrentUser;
use crate::orm::{FieldType, ModelMeta};
use axum::body::{to_bytes, Body};
use axum::http::{header, Method, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Map, Value as JsonValue};
use std::sync::Arc;
use tower::ServiceExt;

/// Protocol revisions this server speaks, newest first.
const VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];

struct Mcp {
    models: Vec<(&'static ModelMeta, Options)>,
    api: Router,
}

pub(crate) fn router(exposed: Vec<(&'static ModelMeta, Options)>) -> Router {
    if exposed.is_empty() {
        return Router::new();
    }
    let mcp = Arc::new(Mcp { api: crate::api::router(exposed.clone()), models: exposed });
    // GET would open a server-to-client stream; this server has nothing to push, which the
    // transport allows by answering 405.
    Router::new().route("/mcp", post(handle).get(|| async { StatusCode::METHOD_NOT_ALLOWED })).with_state(mcp)
}

fn reply(id: &JsonValue, result: JsonValue) -> Response {
    Json(json!({ "jsonrpc": "2.0", "id": id, "result": result })).into_response()
}

fn rpc_error(id: &JsonValue, code: i64, message: &str) -> Response {
    Json(json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })).into_response()
}

async fn handle(axum::extract::State(mcp): axum::extract::State<Arc<Mcp>>, user: CurrentUser, body: axum::body::Bytes) -> Response {
    let Ok(msg) = serde_json::from_slice::<JsonValue>(&body) else {
        return rpc_error(&JsonValue::Null, -32700, "Parse error");
    };
    let id = msg.get("id").cloned().unwrap_or(JsonValue::Null);
    let Some(method) = msg.get("method").and_then(JsonValue::as_str) else {
        // A response or a batch: neither is something a client sends this server.
        return rpc_error(&id, -32600, "Invalid Request");
    };
    if msg.get("id").is_none() {
        return StatusCode::ACCEPTED.into_response(); // a notification, e.g. notifications/initialized
    }
    let params = msg.get("params").cloned().unwrap_or(JsonValue::Null);
    match method {
        "initialize" => {
            let asked = params["protocolVersion"].as_str().unwrap_or("");
            let version = VERSIONS.iter().find(|v| **v == asked).unwrap_or(&VERSIONS[0]);
            reply(
                &id,
                json!({
                    "protocolVersion": version,
                    "capabilities": { "tools": { "listChanged": false } },
                    "serverInfo": { "name": "rangoli", "version": env!("CARGO_PKG_VERSION") },
                    "instructions": "Tools read and change this app's data as the signed-in user. \
                        List tools take exact-match `filters`, a `search` text and `ordering` (a field, `-field` for descending).",
                }),
            )
        }
        "ping" => reply(&id, json!({})),
        "tools/list" => reply(&id, json!({ "tools": tools(&mcp, &user) })),
        "tools/call" => {
            let name = params["name"].as_str().unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
            match call(&mcp, &user, name, &args).await {
                Some(result) => reply(&id, result),
                None => rpc_error(&id, -32602, &format!("Unknown tool: {name}")),
            }
        }
        _ => rpc_error(&id, -32601, &format!("Method not found: {method}")),
    }
}

/// The five tools of one model, as (operation, needs write access).
const OPS: [(&str, bool); 5] = [("list", false), ("get", false), ("create", true), ("update", true), ("delete", true)];

fn noun(meta: &ModelMeta) -> String {
    let mut words = String::new();
    for (i, c) in meta.name.chars().enumerate() {
        if c.is_uppercase() && i > 0 {
            words.push(' ');
        }
        words.extend(c.to_lowercase());
    }
    words
}

fn tools(mcp: &Mcp, user: &CurrentUser) -> Vec<JsonValue> {
    let mut out = vec![];
    for (meta, opts) in &mcp.models {
        let (n, id) = (noun(meta), json!({ "type": "integer", "description": "The row's id" }));
        let mut fields = Map::new();
        for f in meta.fields.iter().filter(|f| !f.is_auto()) {
            fields.insert(f.name.into(), field_schema(f));
        }
        for rel in meta.m2m {
            fields.insert(
                rel.name.into(),
                json!({ "type": "array", "items": { "type": "integer" }, "description": format!("ids of related {} rows", rel.target) }),
            );
        }
        let required: Vec<&str> = meta
            .fields
            .iter()
            .filter(|f| !f.null && !f.is_auto() && f.ty != FieldType::Bool && f.default.is_none() && !f.password)
            .map(|f| f.name)
            .collect();
        for (op, write) in OPS {
            let allowed = (if write { opts.write } else { opts.read }).allows(user);
            // Owned rows need a signed-in owner, so anonymous writes there always fail: don't offer them.
            if !allowed || (write && opts.owner.is_some() && user.0.is_none()) {
                continue; // only offer what this user may actually do
            }
            let (description, schema) = match op {
                "list" => (
                    format!("List {n} rows, newest last. Returns count and up to `limit` results."),
                    json!({ "type": "object", "properties": {
                        "filters": { "type": "object", "description": "Exact matches, field name to value, e.g. {\"published\": true}", "additionalProperties": true },
                        "search": { "type": "string", "description": "Text to look for in the text fields" },
                        "ordering": { "type": "string", "description": "A field to sort by; prefix with - for descending" },
                        "limit": { "type": "integer", "minimum": 1, "maximum": 1000 },
                        "offset": { "type": "integer", "minimum": 0 } } }),
                ),
                "get" => (format!("Fetch one {n} by id."), json!({ "type": "object", "properties": { "id": id }, "required": ["id"] })),
                "create" => (
                    format!("Create a {n}. Returns the saved row."),
                    json!({ "type": "object", "properties": fields, "required": required }),
                ),
                "update" => {
                    let mut props = fields.clone();
                    props.insert("id".into(), id.clone());
                    (
                        format!("Change some fields of a {n}; fields left out keep their value."),
                        json!({ "type": "object", "properties": props, "required": ["id"] }),
                    )
                }
                _ => (format!("Delete a {n} by id."), json!({ "type": "object", "properties": { "id": id }, "required": ["id"] })),
            };
            let mut tool = json!({ "name": format!("{}_{op}", meta.table), "description": description, "inputSchema": schema });
            if !write {
                tool["annotations"] = json!({ "readOnlyHint": true });
            } else if op == "delete" {
                tool["annotations"] = json!({ "destructiveHint": true });
            }
            out.push(tool);
        }
    }
    out
}

/// Run a tool by sending the matching request through the JSON API as this user.
/// `None` for an unknown tool name.
async fn call(mcp: &Mcp, user: &CurrentUser, name: &str, args: &JsonValue) -> Option<JsonValue> {
    let (table, op) = name.rsplit_once('_')?;
    mcp.models.iter().find(|(m, _)| m.table == table)?;
    let id = || args["id"].as_i64();
    let (method, path, body) = match op {
        "list" => {
            let mut q: Vec<(String, String)> = vec![];
            if let Some(filters) = args["filters"].as_object() {
                for (k, v) in filters {
                    q.push((k.clone(), v.as_str().map_or_else(|| v.to_string(), str::to_owned)));
                }
            }
            for k in ["search", "ordering", "limit", "offset"] {
                if let Some(v) = args.get(k).filter(|v| !v.is_null()) {
                    q.push((k.into(), v.as_str().map_or_else(|| v.to_string(), str::to_owned)));
                }
            }
            (Method::GET, format!("/api/{table}/?{}", serde_urlencoded::to_string(&q).unwrap_or_default()), None)
        }
        "get" => (Method::GET, format!("/api/{table}/{}", id()?), None),
        "create" => (Method::POST, format!("/api/{table}/"), Some(args.clone())),
        "update" => {
            let mut fields = args.clone();
            fields.as_object_mut()?.remove("id");
            (Method::PATCH, format!("/api/{table}/{}", id()?), Some(fields))
        }
        "delete" => (Method::DELETE, format!("/api/{table}/{}", id()?), None),
        _ => return None,
    };
    let mut req = Request::builder().method(method).uri(path).header(header::CONTENT_TYPE, "application/json");
    let ext = req.extensions_mut()?;
    ext.insert(user.clone());
    ext.insert(Via("MCP"));
    let req = req.body(body.map_or_else(Body::empty, |b| Body::from(b.to_string()))).ok()?;
    let res = mcp.api.clone().oneshot(req).await.ok()?;
    let status = res.status();
    let bytes = to_bytes(res.into_body(), 4 * 1024 * 1024).await.unwrap_or_default();
    let data: JsonValue = serde_json::from_slice(&bytes).unwrap_or(JsonValue::Null);
    let structured = match (op, status.is_success()) {
        ("delete", true) => json!({ "deleted": true, "id": id() }),
        (_, true) => data,
        // Errors carry the API's own message (field errors, "Not found.", permissions) for the agent to act on.
        (_, false) => json!({ "status": status.as_u16(), "error": data }),
    };
    Some(json!({
        "content": [{ "type": "text", "text": serde_json::to_string_pretty(&structured).unwrap_or_default() }],
        "structuredContent": structured,
        "isError": !status.is_success(),
    }))
}
