//! REST for hourly pay and time (contract 14.6). Same style and auth as `payroll_api.rs`:
//! bearer token, scope checked in the library function, the actor is always the token's
//! member, and bodies with unknown fields (a payee, an actor) are refused with 400.

use super::api::{payout_error, principal, ApiContext};
use super::error::PayoutError;
use super::shifts::{self, NewShift, ShiftPatch};
use super::timeclock::{self, AdjustBody, EntryFilter};
use super::timesheet;
use super::wages::{self, NewWage};
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

macro_rules! parse_or_return {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(r) => return r,
        }
    };
}

fn bad_request(msg: String) -> HttpResponse {
    HttpResponse::build(StatusCode::BAD_REQUEST).json(json!({"status": "error", "message": msg}))
}

fn body<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, HttpResponse> {
    serde_json::from_slice(bytes).map_err(|e| bad_request(format!("invalid body: {e}")))
}

/// Empty or `{}`. Anything else, such as a body naming a payee or an approver, is refused.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NoBody {}

fn no_body(bytes: &[u8]) -> Result<(), HttpResponse> {
    if bytes.iter().all(|b| b.is_ascii_whitespace()) {
        return Ok(());
    }
    body::<NoBody>(bytes).map(|_| ())
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReasonOnly {
    #[serde(default)]
    reason: Option<String>,
}

fn ok<T: serde::Serialize>(status: StatusCode, key: &str, v: Result<T, PayoutError>) -> HttpResponse {
    match v {
        Ok(v) => HttpResponse::build(status).json(json!({"status": "success", key: v})),
        Err(e) => payout_error(e),
    }
}

// ------------------------------------------------------------------ clock

/// The caller's own payee; the body must be empty or `{}`.
#[post("/api/v1/time/clock-in")]
pub async fn clock_in(req: HttpRequest, bytes: web::Bytes, ctx: web::Data<ApiContext>) -> HttpResponse {
    let p = auth_or_return!(req, ctx);
    parse_or_return!(no_body(&bytes));
    match timeclock::clock_in(&ctx.pool, &p, Utc::now()).await {
        Ok(r) => {
            let status = if r.result == "created" { StatusCode::CREATED } else { StatusCode::OK };
            HttpResponse::build(status).json(json!({"status": "success", "result": r.result, "entry": r.entry}))
        }
        Err(e) => payout_error(e),
    }
}

#[post("/api/v1/time/clock-out")]
pub async fn clock_out(req: HttpRequest, bytes: web::Bytes, ctx: web::Data<ApiContext>) -> HttpResponse {
    let p = auth_or_return!(req, ctx);
    parse_or_return!(no_body(&bytes));
    ok(StatusCode::OK, "result", timeclock::clock_out(&ctx.pool, &p, Utc::now()).await)
}

// ------------------------------------------------------------------ entries

#[get("/api/v1/time/entries")]
pub async fn list_entries(req: HttpRequest, q: web::Query<EntryFilter>, ctx: web::Data<ApiContext>) -> HttpResponse {
    let p = auth_or_return!(req, ctx);
    ok(StatusCode::OK, "entries", timeclock::list(&ctx.pool, &p, &q, Utc::now()).await)
}

#[post("/api/v1/time/entries/{id}/approve")]
pub async fn approve_entry(req: HttpRequest, path: web::Path<String>, bytes: web::Bytes, ctx: web::Data<ApiContext>) -> HttpResponse {
    let p = auth_or_return!(req, ctx);
    parse_or_return!(no_body(&bytes));
    ok(StatusCode::OK, "entry", timeclock::approve(&ctx.pool, ctx.directory.as_ref(), &p, &path, Utc::now()).await)
}

#[post("/api/v1/time/entries/{id}/adjust")]
pub async fn adjust_entry(req: HttpRequest, path: web::Path<String>, bytes: web::Bytes, ctx: web::Data<ApiContext>) -> HttpResponse {
    let p = auth_or_return!(req, ctx);
    let b: AdjustBody = parse_or_return!(body(&bytes));
    ok(StatusCode::OK, "entry", timeclock::adjust(&ctx.pool, ctx.directory.as_ref(), &p, &path, b, Utc::now()).await)
}

#[post("/api/v1/time/entries/{id}/void")]
pub async fn void_entry(req: HttpRequest, path: web::Path<String>, bytes: web::Bytes, ctx: web::Data<ApiContext>) -> HttpResponse {
    let p = auth_or_return!(req, ctx);
    let b: ReasonOnly = parse_or_return!(body(&bytes));
    ok(StatusCode::OK, "entry", timeclock::void(&ctx.pool, ctx.directory.as_ref(), &p, &path, b.reason.as_deref(), Utc::now()).await)
}

// ------------------------------------------------------------------ shifts

#[derive(Debug, Deserialize)]
pub struct ShiftQuery {
    pub payee: Option<String>,
    pub status: Option<String>,
}

#[get("/api/v1/time/shifts")]
pub async fn list_shifts(req: HttpRequest, q: web::Query<ShiftQuery>, ctx: web::Data<ApiContext>) -> HttpResponse {
    let p = auth_or_return!(req, ctx);
    ok(StatusCode::OK, "shifts", shifts::list(&ctx.pool, &p, q.payee.as_deref(), q.status.as_deref()).await)
}

#[post("/api/v1/time/shifts")]
pub async fn create_shift(req: HttpRequest, bytes: web::Bytes, ctx: web::Data<ApiContext>) -> HttpResponse {
    let p = auth_or_return!(req, ctx);
    let input: NewShift = parse_or_return!(body(&bytes));
    ok(StatusCode::CREATED, "shift", shifts::create(&ctx.pool, ctx.directory.as_ref(), &p, input, Utc::now()).await)
}

#[patch("/api/v1/time/shifts/{id}")]
pub async fn update_shift(req: HttpRequest, path: web::Path<String>, bytes: web::Bytes, ctx: web::Data<ApiContext>) -> HttpResponse {
    let p = auth_or_return!(req, ctx);
    let patch: ShiftPatch = parse_or_return!(body(&bytes));
    ok(StatusCode::OK, "shift", shifts::update(&ctx.pool, ctx.directory.as_ref(), &p, &path, patch, Utc::now()).await)
}

// ------------------------------------------------------------------ timesheet

#[derive(Debug, Deserialize)]
pub struct TimesheetQuery {
    pub payee: Option<String>,
    pub period: Option<String>,
}

#[get("/api/v1/time/timesheet")]
pub async fn get_timesheet(req: HttpRequest, q: web::Query<TimesheetQuery>, ctx: web::Data<ApiContext>) -> HttpResponse {
    let p = auth_or_return!(req, ctx);
    ok(
        StatusCode::OK,
        "timesheet",
        timesheet::timesheet(&ctx.pool, &ctx.payments, ctx.directory.as_ref(), &p, q.payee.as_deref(), q.period.as_deref(), Utc::now()).await,
    )
}

// ------------------------------------------------------------------ wages

#[get("/api/v1/payees/{id}/wage")]
pub async fn get_wage(req: HttpRequest, path: web::Path<String>, ctx: web::Data<ApiContext>) -> HttpResponse {
    let p = auth_or_return!(req, ctx);
    ok(StatusCode::OK, "wage", wages::history(&ctx.pool, &ctx.payments, ctx.directory.as_ref(), &p, &path, Utc::now()).await)
}

#[post("/api/v1/payees/{id}/wage")]
pub async fn set_wage(req: HttpRequest, path: web::Path<String>, bytes: web::Bytes, ctx: web::Data<ApiContext>) -> HttpResponse {
    let p = auth_or_return!(req, ctx);
    let input: NewWage = parse_or_return!(body(&bytes));
    ok(StatusCode::CREATED, "wage", wages::set(&ctx.pool, &ctx.payments, ctx.directory.as_ref(), &p, &path, input, Utc::now()).await)
}

#[post("/api/v1/payees/{id}/wage/{wage_id}/approve")]
pub async fn approve_wage(req: HttpRequest, path: web::Path<(String, String)>, bytes: web::Bytes, ctx: web::Data<ApiContext>) -> HttpResponse {
    let p = auth_or_return!(req, ctx);
    parse_or_return!(no_body(&bytes));
    let (payee, wage) = path.into_inner();
    ok(StatusCode::OK, "result", wages::approve(&ctx.pool, &ctx.payments, ctx.directory.as_ref(), &p, &payee, &wage, Utc::now()).await)
}

/// Register the time routes. Called from `api::configure`.
pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(clock_in)
        .service(clock_out)
        .service(list_entries)
        .service(approve_entry)
        .service(adjust_entry)
        .service(void_entry)
        .service(list_shifts)
        .service(create_shift)
        .service(update_shift)
        .service(get_timesheet)
        .service(get_wage)
        .service(set_wage)
        .service(approve_wage);
}
