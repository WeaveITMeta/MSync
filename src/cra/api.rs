//! Hierarchy reads and CRA administration (`/api/v1`), plus the guard that refuses
//! payout policy writes while the CRA authors the policy.
//!
//! The lookups are plain functions ([`hierarchy_get`], [`standing_get`],
//! [`snapshots_list`]) so other entry points (the MCP server) can call them; the
//! handlers only authenticate and serialize.
//!
//! ```text
//! GET  /api/v1/hierarchy                       hierarchy.read
//! GET  /api/v1/hierarchy/members/{member_id}   hierarchy.read
//! GET  /api/v1/cra/snapshots?limit=            hierarchy.read
//! POST /api/v1/cra/reload                      hierarchy.read and rank 1000
//! PUT  /api/v1/payouts/policy                  409 while a snapshot is active
//! ```

use actix_web::http::StatusCode;
use actix_web::{guard, web, HttpRequest, HttpResponse};
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::SqlitePool;
use tracing::{error, warn};

use super::loader::{
    member_merit, CraContext, CraHandle, CraRuntimeError, ORG_ID, POLICY_AUTHORED_IN_CRA,
};
use super::model::RoleKind;
use super::store::{self, RecordedViolation};
use super::{standing, Mode, Standing};
use crate::auth::{authenticate, scopes, Directory, Principal, OWNER_RANK};
use crate::payments::api::auth_error;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HierarchyRole {
    pub key: String,
    pub title: String,
    pub kind: RoleKind,
    pub rank: u32,
    pub permissions: Vec<String>,
    pub discord_role: Option<String>,
    pub slack_usergroup: Option<String>,
    pub min_merit: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HierarchyView {
    pub org_slug: String,
    pub org_name: String,
    pub mode: Mode,
    pub snapshot_id: i64,
    pub commit_sha: String,
    pub loaded_at: String,
    /// Highest rank first; ties break on the role key. Owners (rank 1000) are engine
    /// configuration and are not listed.
    pub roles: Vec<HierarchyRole>,
}

/// The active snapshot's roles, titles and ranks in rank order, and the org mode.
/// `None` when no snapshot is active.
pub fn hierarchy_get(handle: &CraHandle) -> Option<HierarchyView> {
    let a = handle.active()?;
    let s = &a.snapshot;
    let mut roles: Vec<HierarchyRole> = s
        .roles
        .roles
        .iter()
        .map(|r| HierarchyRole {
            key: r.key.clone(),
            title: r.title.clone(),
            kind: r.kind,
            rank: r.rank,
            permissions: r.permissions.clone(),
            discord_role: r.discord_role.clone(),
            slack_usergroup: r.slack_usergroup.clone(),
            min_merit: r.min_merit,
        })
        .collect();
    roles.sort_by(|a, b| b.rank.cmp(&a.rank).then(a.key.cmp(&b.key)));
    Some(HierarchyView {
        org_slug: s.org.org.slug.clone(),
        org_name: s.org.org.name.clone(),
        mode: s.mode_label,
        snapshot_id: a.snapshot_id,
        commit_sha: a.commit_sha.clone(),
        loaded_at: a.loaded_at.clone(),
        roles,
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MemberStandingView {
    pub member_id: String,
    pub name: String,
    pub snapshot_id: i64,
    /// Vested merit used for the tier (today all merit counts as vested).
    pub vested_merit: u64,
    /// True when engine configuration makes this member an owner.
    pub owner: bool,
    /// The rank the engine uses: 1000 for an owner, else the CRA standing rank.
    pub effective_rank: Option<i64>,
    pub standing: Standing,
}

/// A member's standing in the active snapshot, with merit read fresh from the
/// database. `Ok(None)` when no snapshot is active or the member is not listed.
pub async fn standing_get(
    pool: &SqlitePool,
    handle: &CraHandle,
    directory: &dyn Directory,
    member_id: &str,
    today: NaiveDate,
) -> Result<Option<MemberStandingView>, sqlx::Error> {
    let Some(a) = handle.active() else {
        return Ok(None);
    };
    let Some(m) = a.snapshot.member(member_id) else {
        return Ok(None);
    };
    let merit = member_merit(pool, &a.snapshot, member_id).await?;
    let Some(st) = standing(&a.snapshot, member_id, merit, today) else {
        return Ok(None);
    };
    let effective_rank = directory.rank_of(member_id);
    Ok(Some(MemberStandingView {
        member_id: m.id.clone(),
        name: m.name.clone(),
        snapshot_id: a.snapshot_id,
        vested_merit: merit,
        owner: effective_rank == Some(OWNER_RANK),
        effective_rank,
        standing: st,
    }))
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SnapshotSummary {
    pub id: i64,
    pub status: String,
    pub retryable: bool,
    pub org_slug: Option<String>,
    pub repo: String,
    pub commit_sha: String,
    pub content_hash: Option<String>,
    pub actor: Option<String>,
    pub author: Option<String>,
    pub via_pull_request: bool,
    pub approvers: Vec<String>,
    pub violations: Vec<RecordedViolation>,
    pub plan_markdown: Option<String>,
    pub loaded_at: String,
    pub superseded_at: Option<String>,
}

/// Recent load attempts, newest first, with status and violations.
pub async fn snapshots_list(
    pool: &SqlitePool,
    limit: i64,
) -> Result<Vec<SnapshotSummary>, sqlx::Error> {
    Ok(store::recent(pool, ORG_ID, limit)
        .await?
        .into_iter()
        .map(|r| SnapshotSummary {
            id: r.id,
            status: r.status,
            retryable: r.retryable,
            org_slug: r.org_slug,
            repo: format!("{}:{}:{}", r.provider, r.host, r.repo),
            commit_sha: r.commit_sha,
            content_hash: r.content_hash,
            actor: r.actor,
            author: r.author,
            via_pull_request: r.via_pull_request,
            approvers: r.approvers,
            violations: r.violations,
            plan_markdown: r.plan_markdown,
            loaded_at: r.loaded_at,
            superseded_at: r.superseded_at,
        })
        .collect())
}

// ----------------------------------------------------------------- handlers

fn err(status: StatusCode, msg: impl Into<String>) -> HttpResponse {
    HttpResponse::build(status).json(json!({"status": "error", "message": msg.into()}))
}

async fn principal(
    req: &HttpRequest,
    ctx: &CraContext,
    scope: &str,
) -> Result<Principal, HttpResponse> {
    let p = authenticate(req, &ctx.pool).await.map_err(|e| {
        warn!("cra api auth refused: {}", e);
        auth_error(&e)
    })?;
    p.require(scope).map_err(|e| auth_error(&e))?;
    Ok(p)
}

fn today() -> NaiveDate {
    chrono::Utc::now().date_naive()
}

pub async fn get_hierarchy(req: HttpRequest, ctx: web::Data<CraContext>) -> HttpResponse {
    if let Err(r) = principal(&req, &ctx, scopes::HIERARCHY_READ).await {
        return r;
    }
    match hierarchy_get(&ctx.handle) {
        Some(h) => HttpResponse::Ok().json(json!({"status": "success", "hierarchy": h})),
        None => err(StatusCode::NOT_FOUND, "no CRA snapshot is active"),
    }
}

pub async fn get_member(
    req: HttpRequest,
    path: web::Path<String>,
    ctx: web::Data<CraContext>,
) -> HttpResponse {
    if let Err(r) = principal(&req, &ctx, scopes::HIERARCHY_READ).await {
        return r;
    }
    if ctx.handle.active().is_none() {
        return err(StatusCode::NOT_FOUND, "no CRA snapshot is active");
    }
    match standing_get(
        &ctx.pool,
        &ctx.handle,
        ctx.directory.as_ref(),
        &path,
        today(),
    )
    .await
    {
        Ok(Some(v)) => HttpResponse::Ok().json(json!({"status": "success", "member": v})),
        Ok(None) => err(
            StatusCode::NOT_FOUND,
            "member not found in the active CRA snapshot",
        ),
        Err(e) => {
            error!("standing lookup failed: {}", e);
            err(StatusCode::INTERNAL_SERVER_ERROR, "database error")
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct SnapshotsQuery {
    pub limit: Option<i64>,
}

pub async fn get_snapshots(
    req: HttpRequest,
    q: web::Query<SnapshotsQuery>,
    ctx: web::Data<CraContext>,
) -> HttpResponse {
    if let Err(r) = principal(&req, &ctx, scopes::HIERARCHY_READ).await {
        return r;
    }
    match snapshots_list(&ctx.pool, q.limit.unwrap_or(20)).await {
        Ok(rows) => HttpResponse::Ok().json(json!({"status": "success", "snapshots": rows})),
        Err(e) => {
            error!("snapshot list failed: {}", e);
            err(StatusCode::INTERNAL_SERVER_ERROR, "database error")
        }
    }
}

pub async fn post_reload(req: HttpRequest, ctx: web::Data<CraContext>) -> HttpResponse {
    let p = match principal(&req, &ctx, scopes::HIERARCHY_READ).await {
        Ok(p) => p,
        Err(r) => return r,
    };
    if ctx.directory.rank_of(&p.member_id) != Some(OWNER_RANK) {
        return err(
            StatusCode::FORBIDDEN,
            "reloading the CRA needs rank 1000 (an owner)",
        );
    }
    match ctx.reload_head(today()).await {
        Ok(outcome) => HttpResponse::Ok().json(json!({"status": "success", "result": outcome})),
        Err(CraRuntimeError::NotConfigured) => err(
            StatusCode::CONFLICT,
            CraRuntimeError::NotConfigured.to_string(),
        ),
        Err(CraRuntimeError::Source(e)) => err(StatusCode::BAD_GATEWAY, e.to_string()),
        Err(e) => {
            error!("CRA reload failed: {}", e);
            err(StatusCode::INTERNAL_SERVER_ERROR, "CRA reload failed")
        }
    }
}

/// Answers `PUT /api/v1/payouts/policy` while a snapshot is active (see [`configure`]).
pub async fn put_policy_refused(req: HttpRequest, ctx: web::Data<CraContext>) -> HttpResponse {
    if let Err(r) = principal(&req, &ctx, scopes::PAYOUTS_POLICY).await {
        return r;
    }
    err(StatusCode::CONFLICT, POLICY_AUTHORED_IN_CRA)
}

/// Register the CRA routes. Call this BEFORE `payments::api::configure`: the payout
/// policy PUT here only matches while a snapshot is active (a guard), so with none
/// active the request falls through to the payments handler unchanged.
pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::resource("/api/v1/payouts/policy")
            .guard(guard::Put())
            .guard(guard::fn_guard(|g| {
                g.app_data::<web::Data<CraContext>>()
                    .is_some_and(|c| c.handle.active().is_some())
            }))
            .to(put_policy_refused),
    )
    .service(web::resource("/api/v1/hierarchy").route(web::get().to(get_hierarchy)))
    .service(
        web::resource("/api/v1/hierarchy/members/{member_id}").route(web::get().to(get_member)),
    )
    .service(web::resource("/api/v1/cra/snapshots").route(web::get().to(get_snapshots)))
    .service(web::resource("/api/v1/cra/reload").route(web::post().to(post_reload)));
}
