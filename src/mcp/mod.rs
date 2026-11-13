//! MCP server (contract section 7): `POST /mcp`, JSON-RPC 2.0 over HTTP with plain JSON
//! responses. Stateless: no SSE stream, no sessions. Same bearer tokens and scopes as REST.
//!
//! Order of checks on every request: bearer token (401, same body as REST), protocol version
//! header, JSON-RPC framing, method. See `docs/MCP.md`.

pub mod jsonrpc;
pub mod tools;

#[cfg(test)]
mod tests;

use crate::auth::{authenticate, Principal};
use crate::merit::api::MeritContext;
use crate::payments::api::{auth_error, ApiContext};
use actix_web::{get, post, web, HttpRequest, HttpResponse};
use jsonrpc::{error_response, result_response, Incoming, RpcError, INVALID_PARAMS, INVALID_REQUEST, METHOD_NOT_FOUND};
use serde::Deserialize;
use serde_json::{json, Value};
use tracing::warn;

/// Env flag that enables `payouts_run_execute`. Anything but `true` leaves it off.
pub const ALLOW_EXECUTE_ENV: &str = "DSYNC_MCP_ALLOW_EXECUTE";

/// Protocol versions this server speaks, newest first.
pub const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

pub const SERVER_NAME: &str = "dsync";

#[derive(Clone)]
pub struct McpContext {
    pub api: ApiContext,
    pub merit: MeritContext,
    /// `payouts_run_execute` is listed and callable only when true. Read once at startup.
    pub allow_execute: bool,
}

impl McpContext {
    pub fn new(api: ApiContext, merit: MeritContext, allow_execute: bool) -> Self {
        Self { api, merit, allow_execute }
    }

    /// `DSYNC_MCP_ALLOW_EXECUTE=true` (exactly, case-insensitive) enables execute. Default off.
    pub fn allow_execute_from_env() -> bool {
        parse_allow_execute(std::env::var(ALLOW_EXECUTE_ENV).ok().as_deref())
    }
}

pub fn parse_allow_execute(v: Option<&str>) -> bool {
    v.map(|s| s.trim().eq_ignore_ascii_case("true")).unwrap_or(false)
}

#[derive(Debug, Deserialize)]
struct InitializeParams {
    #[serde(rename = "protocolVersion")]
    protocol_version: String,
}

#[derive(Debug, Deserialize)]
struct CallParams {
    name: String,
    #[serde(default)]
    arguments: Option<Value>,
}

fn initialize(params: Option<Value>) -> Result<Value, RpcError> {
    let p: InitializeParams = serde_json::from_value(params.unwrap_or(Value::Null))
        .map_err(|_| RpcError::new(INVALID_PARAMS, "initialize needs params.protocolVersion (a string)"))?;
    let version = if SUPPORTED_PROTOCOL_VERSIONS.contains(&p.protocol_version.as_str()) {
        p.protocol_version
    } else {
        SUPPORTED_PROTOCOL_VERSIONS[0].to_string()
    };
    Ok(json!({
        "protocolVersion": version,
        "capabilities": {"tools": {"listChanged": false}},
        "serverInfo": {"name": SERVER_NAME, "title": "DSync merit and payouts", "version": env!("CARGO_PKG_VERSION")},
        "instructions": "DSync scores work against a fixed rubric and runs payouts. Call rubric_get first. \
            Every write is recorded as the member that owns this token. Tools you cannot use are not listed.",
    }))
}

async fn dispatch(ctx: &McpContext, p: &Principal, method: &str, params: Option<Value>) -> Result<Value, RpcError> {
    match method {
        "initialize" => initialize(params),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(tools::list(ctx, p)),
        "tools/call" => {
            let cp: CallParams = serde_json::from_value(params.unwrap_or(Value::Null))
                .map_err(|_| RpcError::invalid_params("tools/call needs params.name (a string) and optional params.arguments (an object)"))?;
            tools::call(ctx, p, &cp.name, cp.arguments).await
        }
        other => Err(RpcError::new(METHOD_NOT_FOUND, format!("method not found: {other}"))),
    }
}

/// Handle one MCP HTTP request. Public so it can be driven without the actix route macro.
pub async fn handle(ctx: &McpContext, req: &HttpRequest, body: &[u8]) -> HttpResponse {
    let principal = match authenticate(req, &ctx.api.pool).await {
        Ok(p) => p,
        Err(e) => {
            warn!("mcp auth refused: {}", e);
            return auth_error(&e);
        }
    };
    if let Some(v) = req.headers().get("MCP-Protocol-Version") {
        let ok = v.to_str().map(|s| SUPPORTED_PROTOCOL_VERSIONS.contains(&s.trim())).unwrap_or(false);
        if !ok {
            let err = RpcError::new(INVALID_REQUEST, format!("unsupported MCP-Protocol-Version; supported: {}", SUPPORTED_PROTOCOL_VERSIONS.join(", ")));
            return HttpResponse::BadRequest().json(error_response(Value::Null, &err));
        }
    }
    match jsonrpc::parse(body) {
        Err((id, err)) => HttpResponse::BadRequest().json(error_response(id, &err)),
        Ok(Incoming::Notification { .. }) | Ok(Incoming::Response) => HttpResponse::Accepted().finish(),
        Ok(Incoming::Request { id, method, params }) => {
            let resp = match dispatch(ctx, &principal, &method, params).await {
                Ok(result) => result_response(id, result),
                Err(err) => error_response(id, &err),
            };
            HttpResponse::Ok().json(resp)
        }
    }
}

#[post("/mcp")]
pub async fn mcp_post(req: HttpRequest, body: web::Bytes, ctx: web::Data<McpContext>) -> HttpResponse {
    handle(&ctx, &req, &body).await
}

/// No SSE stream is offered (stateless server): GET is 405.
#[get("/mcp")]
pub async fn mcp_get() -> HttpResponse {
    HttpResponse::MethodNotAllowed().insert_header(("Allow", "POST")).finish()
}

/// Register `/mcp`. Needs `web::Data<McpContext>`.
pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(mcp_post).service(mcp_get);
}
