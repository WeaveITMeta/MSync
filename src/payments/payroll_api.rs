//! REST for payroll control (contract 11.6): payees, pay orders, invoices and the payee
//! statement. Same style and auth as `api.rs`: bearer token, scope checked in the library
//! function, the actor is always the token's member, and bodies with unknown fields (an actor,
//! an approver) are refused with 400.

use super::api::{payout_error, principal, ApiContext};
use super::error::PayoutError;
use super::invoices::{self, InvoiceFilter, NewInvoice, ReasonBody};
use super::payees::{self, NewPayee, PayeePatch};
use super::payorders::{self, NewOrder, OrderPatch, Transition};
use super::statement;
use actix_web::http::StatusCode;
use actix_web::{get, patch, post, web, HttpRequest, HttpResponse};
use chrono::Utc;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::json;

macro_rules! auth_or_return {
    ($req:expr, $ctx:expr) => {
        match principal(&$req, &$ctx).await {
            Ok(p) => p,
            Err(r) => return r,
        }
    };
}

fn bad_request(msg: String) -> HttpResponse {
    HttpResponse::build(StatusCode::BAD_REQUEST).json(json!({"status": "error", "message": msg}))
}

/// Parse a JSON body; unknown fields and malformed JSON are a 400 with serde's message.
fn body<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, HttpResponse> {
    serde_json::from_slice(bytes).map_err(|e| bad_request(format!("invalid body: {e}")))
}

/// An action endpoint takes no body; an empty body or `{}` is accepted, anything else (for
/// example a body naming an approver) is refused.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NoBody {}

fn no_body(bytes: &[u8]) -> Result<(), HttpResponse> {
    if bytes.iter().all(|b| b.is_ascii_whitespace()) {
        return Ok(());
    }
    body::<NoBody>(bytes).map(|_| ())
}

fn reason_body(bytes: &[u8]) -> Result<ReasonBody, HttpResponse> {
    if bytes.iter().all(|b| b.is_ascii_whitespace()) {
        return Ok(ReasonBody::default());
    }
    body::<ReasonBody>(bytes)
}

fn ok<T: serde::Serialize>(status: StatusCode, key: &str, v: Result<T, PayoutError>) -> HttpResponse {
    match v {
        Ok(v) => HttpResponse::build(status).json(json!({"status": "success", key: v})),
        Err(e) => payout_error(e),
    }
}

macro_rules! parse_or_return {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(r) => return r,
        }
    };
}

// ------------------------------------------------------------------ payees

#[derive(Debug, Deserialize)]
pub struct StatusQuery {
    pub status: Option<String>,
}

#[get("/api/v1/payees")]
pub async fn list_payees(req: HttpRequest, q: web::Query<StatusQuery>, ctx: web::Data<ApiContext>) -> HttpResponse {
    let p = auth_or_return!(req, ctx);
    ok(StatusCode::OK, "payees", payees::list(&ctx.pool, &p, q.status.as_deref()).await)
}

/// There is no field for a bank account or card number (contract 11.2).
#[post("/api/v1/payees")]
pub async fn create_payee(req: HttpRequest, bytes: web::Bytes, ctx: web::Data<ApiContext>) -> HttpResponse {
    let p = auth_or_return!(req, ctx);
    let input: NewPayee = parse_or_return!(body(&bytes));
    ok(StatusCode::CREATED, "payee", payees::create(&ctx.pool, ctx.directory.as_ref(), &p, input, Utc::now()).await)
}

#[get("/api/v1/payees/{id}")]
pub async fn get_payee(req: HttpRequest, path: web::Path<String>, ctx: web::Data<ApiContext>) -> HttpResponse {
    let p = auth_or_return!(req, ctx);
    ok(StatusCode::OK, "payee", payees::get(&ctx.pool, &p, &path).await)
}

#[patch("/api/v1/payees/{id}")]
pub async fn update_payee(req: HttpRequest, path: web::Path<String>, bytes: web::Bytes, ctx: web::Data<ApiContext>) -> HttpResponse {
    let p = auth_or_return!(req, ctx);
    let patch: PayeePatch = parse_or_return!(body(&bytes));
    ok(StatusCode::OK, "payee", payees::update(&ctx.pool, ctx.directory.as_ref(), &p, &path, patch, Utc::now()).await)
}

// ------------------------------------------------------------------ pay orders

#[get("/api/v1/payorders")]
pub async fn list_orders(req: HttpRequest, q: web::Query<StatusQuery>, ctx: web::Data<ApiContext>) -> HttpResponse {
    let p = auth_or_return!(req, ctx);
    ok(StatusCode::OK, "payorders", payorders::list(&ctx.pool, &ctx.payments, ctx.directory.as_ref(), &p, q.status.as_deref()).await)
}

#[post("/api/v1/payorders")]
pub async fn create_order(req: HttpRequest, bytes: web::Bytes, ctx: web::Data<ApiContext>) -> HttpResponse {
    let p = auth_or_return!(req, ctx);
    let input: NewOrder = parse_or_return!(body(&bytes));
    ok(StatusCode::CREATED, "payorder", payorders::create(&ctx.pool, &ctx.payments, ctx.directory.as_ref(), &p, input, Utc::now()).await)
}

#[get("/api/v1/payorders/{id}")]
pub async fn get_order(req: HttpRequest, path: web::Path<String>, ctx: web::Data<ApiContext>) -> HttpResponse {
    let p = auth_or_return!(req, ctx);
    ok(StatusCode::OK, "payorder", payorders::get(&ctx.pool, &ctx.payments, ctx.directory.as_ref(), &p, &path).await)
}

#[patch("/api/v1/payorders/{id}")]
pub async fn update_order(req: HttpRequest, path: web::Path<String>, bytes: web::Bytes, ctx: web::Data<ApiContext>) -> HttpResponse {
    let p = auth_or_return!(req, ctx);
    let patch: OrderPatch = parse_or_return!(body(&bytes));
    ok(StatusCode::OK, "payorder", payorders::update(&ctx.pool, &ctx.payments, ctx.directory.as_ref(), &p, &path, patch, Utc::now()).await)
}

#[post("/api/v1/payorders/{id}/approve")]
pub async fn approve_order(req: HttpRequest, path: web::Path<String>, bytes: web::Bytes, ctx: web::Data<ApiContext>) -> HttpResponse {
    let p = auth_or_return!(req, ctx);
    parse_or_return!(no_body(&bytes));
    ok(StatusCode::OK, "result", payorders::approve(&ctx.pool, &ctx.payments, ctx.directory.as_ref(), &p, &path, Utc::now()).await)
}

async fn order_transition(req: HttpRequest, id: &str, bytes: &[u8], ctx: &ApiContext, t: Transition) -> HttpResponse {
    let p = auth_or_return!(req, ctx);
    parse_or_return!(no_body(bytes));
    ok(StatusCode::OK, "payorder", payorders::transition(&ctx.pool, &ctx.payments, ctx.directory.as_ref(), &p, id, t, Utc::now()).await)
}

#[post("/api/v1/payorders/{id}/pause")]
pub async fn pause_order(req: HttpRequest, path: web::Path<String>, bytes: web::Bytes, ctx: web::Data<ApiContext>) -> HttpResponse {
    order_transition(req, &path, &bytes, &ctx, Transition::Pause).await
}

#[post("/api/v1/payorders/{id}/resume")]
pub async fn resume_order(req: HttpRequest, path: web::Path<String>, bytes: web::Bytes, ctx: web::Data<ApiContext>) -> HttpResponse {
    order_transition(req, &path, &bytes, &ctx, Transition::Resume).await
}

#[post("/api/v1/payorders/{id}/cancel")]
pub async fn cancel_order(req: HttpRequest, path: web::Path<String>, bytes: web::Bytes, ctx: web::Data<ApiContext>) -> HttpResponse {
    order_transition(req, &path, &bytes, &ctx, Transition::Cancel).await
}

#[derive(Debug, Deserialize)]
pub struct PreviewQuery {
    pub periods: Option<u32>,
}

#[get("/api/v1/payorders/{id}/preview")]
pub async fn preview_order(req: HttpRequest, path: web::Path<String>, q: web::Query<PreviewQuery>, ctx: web::Data<ApiContext>) -> HttpResponse {
    let p = auth_or_return!(req, ctx);
    let periods = q.periods.unwrap_or(3);
    ok(StatusCode::OK, "preview", payorders::preview(&ctx.pool, &ctx.payments, ctx.directory.as_ref(), &p, &path, periods, Utc::now()).await)
}

// ------------------------------------------------------------------ invoices

#[derive(Debug, Deserialize)]
pub struct InvoiceQuery {
    pub status: Option<String>,
    pub payee_id: Option<String>,
}

#[get("/api/v1/invoices")]
pub async fn list_invoices(req: HttpRequest, q: web::Query<InvoiceQuery>, ctx: web::Data<ApiContext>) -> HttpResponse {
    let p = auth_or_return!(req, ctx);
    let f = InvoiceFilter { status: q.status.clone(), payee_id: q.payee_id.clone() };
    ok(StatusCode::OK, "invoices", invoices::list(&ctx.pool, &ctx.payments, ctx.directory.as_ref(), &p, &f, Utc::now()).await)
}

#[post("/api/v1/invoices")]
pub async fn submit_invoice(req: HttpRequest, bytes: web::Bytes, ctx: web::Data<ApiContext>) -> HttpResponse {
    let p = auth_or_return!(req, ctx);
    let input: NewInvoice = parse_or_return!(body(&bytes));
    ok(StatusCode::CREATED, "invoice", invoices::submit(&ctx.pool, &ctx.payments, ctx.directory.as_ref(), &p, input, Utc::now()).await)
}

#[get("/api/v1/invoices/{id}")]
pub async fn get_invoice(req: HttpRequest, path: web::Path<String>, ctx: web::Data<ApiContext>) -> HttpResponse {
    let p = auth_or_return!(req, ctx);
    ok(StatusCode::OK, "invoice", invoices::get(&ctx.pool, &ctx.payments, ctx.directory.as_ref(), &p, &path, Utc::now()).await)
}

#[post("/api/v1/invoices/{id}/approve")]
pub async fn approve_invoice(req: HttpRequest, path: web::Path<String>, bytes: web::Bytes, ctx: web::Data<ApiContext>) -> HttpResponse {
    let p = auth_or_return!(req, ctx);
    parse_or_return!(no_body(&bytes));
    ok(StatusCode::OK, "result", invoices::approve(&ctx.pool, &ctx.payments, ctx.directory.as_ref(), &p, &path, Utc::now()).await)
}

#[post("/api/v1/invoices/{id}/reject")]
pub async fn reject_invoice(req: HttpRequest, path: web::Path<String>, bytes: web::Bytes, ctx: web::Data<ApiContext>) -> HttpResponse {
    let p = auth_or_return!(req, ctx);
    let b = parse_or_return!(reason_body(&bytes));
    ok(StatusCode::OK, "invoice", invoices::reject(&ctx.pool, &ctx.payments, ctx.directory.as_ref(), &p, &path, b.reason.as_deref(), Utc::now()).await)
}

#[post("/api/v1/invoices/{id}/void")]
pub async fn void_invoice(req: HttpRequest, path: web::Path<String>, bytes: web::Bytes, ctx: web::Data<ApiContext>) -> HttpResponse {
    let p = auth_or_return!(req, ctx);
    let b = parse_or_return!(reason_body(&bytes));
    ok(StatusCode::OK, "invoice", invoices::void(&ctx.pool, &ctx.payments, ctx.directory.as_ref(), &p, &path, b.reason.as_deref(), Utc::now()).await)
}

// ------------------------------------------------------------------ statement

#[derive(Debug, Deserialize)]
pub struct StatementQuery {
    pub payee: String,
    pub from: String,
    pub to: String,
}

#[get("/api/v1/payouts/statement")]
pub async fn payout_statement(req: HttpRequest, q: web::Query<StatementQuery>, ctx: web::Data<ApiContext>) -> HttpResponse {
    let p = auth_or_return!(req, ctx);
    ok(StatusCode::OK, "statement", statement::statement(&ctx.pool, &p, &q.payee, &q.from, &q.to).await)
}

/// Register the payroll routes. Called from `api::configure`.
pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(list_payees)
        .service(create_payee)
        .service(get_payee)
        .service(update_payee)
        .service(list_orders)
        .service(create_order)
        .service(preview_order)
        .service(approve_order)
        .service(pause_order)
        .service(resume_order)
        .service(cancel_order)
        .service(get_order)
        .service(update_order)
        .service(list_invoices)
        .service(submit_invoice)
        .service(approve_invoice)
        .service(reject_invoice)
        .service(void_invoice)
        .service(get_invoice)
        .service(payout_statement);
}
