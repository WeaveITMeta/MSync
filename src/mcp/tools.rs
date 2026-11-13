//! MCP tools. Each tool declares the scope it needs; `tools/list` shows only the tools the
//! caller's scopes allow, and `tools/call` checks the scope again before doing anything.
//!
//! Every tool is an adapter over a function the REST API also calls: `merit::api::*` for merit,
//! `payments::{policy, runs}` for payouts. There is no other path to a transfer.

use super::jsonrpc::RpcError;
use super::McpContext;
use crate::auth::{scopes, AuthError, Principal};
use crate::merit::api::{self as merit_api, LeaderboardParams, MeritApiError, ScoreInput, SubjectInput};
use crate::merit::rubric::{CRITERIA, MAX_LEVEL, MIN_EVIDENCE_CHARS, RUBRIC_ID, RUBRIC_SHA256};
use crate::payments::error::PayoutError;
use crate::payments::ledger::LedgerError;
use crate::payments::policy::{self, PolicyError};
use crate::payments::runs::{self, CreateRunResult};
use chrono::Utc;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};
use tracing::error;

pub const EXECUTE_TOOL: &str = "payouts_run_execute";

pub struct ToolDef {
    pub name: &'static str,
    pub title: &'static str,
    /// None: any valid token may call it.
    pub scope: Option<&'static str>,
    pub description: &'static str,
    pub input_schema: fn() -> Value,
    pub read_only: bool,
    pub destructive: bool,
}

/// Every tool this server offers. `standing_get` and `hierarchy_get` (contract section 7) are
/// not offered yet: they depend on CRA loading.
pub const TOOLS: &[ToolDef] = &[
    ToolDef {
        name: "rubric_get",
        title: "Get the merit rubric",
        scope: None,
        description: "Returns the fixed merit rubric dsync-rubric-v1: its id and pinned SHA-256 hash, the six criteria with \
            their weights and the meaning of each level 0 to 4, the hard gates, the score formula, the points formula and \
            the event rules in force. Call this before merit_score_submit: a submission must echo the id and hash exactly.",
        input_schema: no_args_schema,
        read_only: true,
        destructive: false,
    },
    ToolDef {
        name: "merit_score_submit",
        title: "Submit a rubric score",
        scope: Some(scopes::MERIT_SCORE),
        description: "Scores one merged or open change (a pull request or merge request) against dsync-rubric-v1. You are \
            the scorer: the token's member is recorded, and you cannot name anyone else. Give all six criteria, each with a \
            level 0 to 4 and at least 20 characters of evidence you checked yourself. Set ci_passed and reverted truthfully: \
            ci_passed false caps the score at 40 and reverted true sets it to 0. You may score a change only once, and never \
            work you authored. Returns the stored score, the current aggregate (median) and any merit adjustment it caused.",
        input_schema: score_submit_schema,
        read_only: false,
        destructive: false,
    },
    ToolDef {
        name: "merit_scores_list",
        title: "List scores for a change",
        scope: Some(scopes::MERIT_READ),
        description: "Lists every rubric score submitted for one change, oldest first, with the aggregate the merit rules use \
            (the median of the counted scorers, or null when fewer than min_scorers have scored).",
        input_schema: subject_args_schema,
        read_only: true,
        destructive: false,
    },
    ToolDef {
        name: "leaderboard_get",
        title: "Get the leaderboard",
        scope: Some(scopes::MERIT_READ),
        description: "Returns the merit leaderboard. With no arguments it ranks people by total XP across every repository, \
            and each row drills down into per-repository amounts that add up to the total. With provider and repo it ranks by \
            the XP earned in that repository alone. Private repository names are included in this view; never repeat them in \
            a public place.",
        input_schema: leaderboard_schema,
        read_only: true,
        destructive: false,
    },
    ToolDef {
        name: "member_merit_get",
        title: "Get one person's merit",
        scope: Some(scopes::MERIT_READ),
        description: "Returns one person's XP by repository and by event kind. Both lists add up to the total. Use the \
            contributor_id from a leaderboard row. Private repository names are included.",
        input_schema: member_schema,
        read_only: true,
        destructive: false,
    },
    ToolDef {
        name: "payouts_policy_get",
        title: "Get the payout policy",
        scope: Some(scopes::PAYOUTS_READ),
        description: "Returns the payout policy in force: whether payouts are enabled, the schedule, limits, approval rules, \
            eligibility and rates, with its version. source is 'default' when no policy has been stored.",
        input_schema: no_args_schema,
        read_only: true,
        destructive: false,
    },
    ToolDef {
        name: "payouts_ledger_list",
        title: "List payout ledger rows",
        scope: Some(scopes::PAYOUTS_READ),
        description: "Lists payout ledger rows (one per merged change on a tracked branch), optionally filtered by status \
            (pending_approval, approved, paid, failed, rejected).",
        input_schema: ledger_schema,
        read_only: true,
        destructive: false,
    },
    ToolDef {
        name: "payouts_run_preview",
        title: "Preview a payout run",
        scope: Some(scopes::PAYOUTS_PREPARE),
        description: "Dry run. Evaluates every open ledger row against the payout policy now and returns the plan: which rows \
            would be included, which are skipped and why, and the total. Persists nothing and moves no money.",
        input_schema: no_args_schema,
        read_only: true,
        destructive: false,
    },
    ToolDef {
        name: "payouts_run_create",
        title: "Create a payout run",
        scope: Some(scopes::PAYOUTS_PREPARE),
        description: "Creates and stores a prepared payout run from the open ledger rows. The run waits for approvals and an \
            execute call; creating it moves no money. Refused while payouts are disabled by policy. The creator is the \
            token's member.",
        input_schema: no_args_schema,
        read_only: false,
        destructive: false,
    },
    ToolDef {
        name: "payouts_run_approve",
        title: "Approve a payout run",
        scope: Some(scopes::PAYOUTS_APPROVE),
        description: "Records your approval (the token's member) on every row of a prepared run. Rows where you are the payee \
            are never approved and are reported in skipped_payee. Approving moves no money.",
        input_schema: run_id_schema,
        read_only: false,
        destructive: false,
    },
    ToolDef {
        name: EXECUTE_TOOL,
        title: "Execute a payout run",
        scope: Some(scopes::PAYOUTS_EXECUTE),
        description: "Pays every row of a run that is payable right now under the policy in force, through Stripe. This moves \
            real money. Rows still missing approvals, inside the hold period, or over a limit are reported as not_payable and \
            left for later. Safe to repeat: a paid row is never paid twice. Disabled unless an operator enables it.",
        input_schema: run_id_schema,
        read_only: false,
        destructive: true,
    },
];

pub fn find(name: &str) -> Option<&'static ToolDef> {
    TOOLS.iter().find(|t| t.name == name)
}

fn scope_ok(p: &Principal, t: &ToolDef) -> bool {
    t.scope.map(|s| p.has(s)).unwrap_or(true)
}

/// Is the tool listed for this caller on this server?
pub fn visible(ctx: &McpContext, p: &Principal, t: &ToolDef) -> bool {
    scope_ok(p, t) && (t.name != EXECUTE_TOOL || ctx.allow_execute)
}

fn tool_json(t: &ToolDef) -> Value {
    json!({
        "name": t.name,
        "title": t.title,
        "description": t.description,
        "inputSchema": (t.input_schema)(),
        "annotations": {
            "title": t.title,
            "readOnlyHint": t.read_only,
            "destructiveHint": t.destructive,
            "idempotentHint": t.read_only || t.name == EXECUTE_TOOL || t.name == "payouts_run_approve",
            "openWorldHint": t.name == EXECUTE_TOOL,
        },
        "_meta": {"dsync/scope": t.scope},
    })
}

pub fn list(ctx: &McpContext, p: &Principal) -> Value {
    let tools: Vec<Value> = TOOLS.iter().filter(|t| visible(ctx, p, t)).map(tool_json).collect();
    json!({ "tools": tools })
}

// ------------------------------------------------------------------ results

fn success(value: Value) -> Value {
    let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
    json!({
        "content": [{"type": "text", "text": text}],
        "structuredContent": value,
        "isError": false,
    })
}

fn failure(message: &str) -> Value {
    json!({
        "content": [{"type": "text", "text": message}],
        "structuredContent": {"error": message},
        "isError": true,
    })
}

/// A tool outcome before it is wrapped: Ok(object) or a caller-safe failure message.
type Outcome = Result<Value, String>;

fn merit_msg(e: MeritApiError) -> String {
    e.message()
}

fn auth_msg(a: &AuthError) -> String {
    match a {
        AuthError::MissingScope(s) => format!("missing scope {s}"),
        AuthError::Db(d) => {
            error!("mcp auth db error: {d}");
            "internal error".into()
        }
        _ => "unauthorized".into(),
    }
}

fn policy_msg(e: PolicyError) -> String {
    let msg = e.to_string();
    match e {
        PolicyError::Auth(a) => auth_msg(&a),
        PolicyError::Invalid(_) | PolicyError::Conflict(_) => msg,
        PolicyError::Corrupt(detail) => {
            error!("stored payout policy is unusable: {detail}");
            "stored payout policy is unusable; an operator must fix it".into()
        }
        PolicyError::Db(d) => {
            error!("mcp policy db error: {d}");
            "internal error".into()
        }
    }
}

/// Caller-safe message for a payout failure. Database detail is logged, never returned.
fn payout_msg(e: PayoutError) -> String {
    let msg = e.to_string();
    match e {
        PayoutError::Auth(a) => auth_msg(&a),
        PayoutError::NotFound(_)
        | PayoutError::InvalidState(_)
        | PayoutError::PayeeCannotApprove
        | PayoutError::PolicyDisabled
        | PayoutError::Invalid(_) => msg,
        PayoutError::Policy(p) => policy_msg(p),
        PayoutError::Ledger(l) => match l {
            LedgerError::NotFound | LedgerError::InvalidState(..) | LedgerError::NotPayable(_) => msg,
            LedgerError::Policy(detail) => {
                error!("payout policy unavailable: {detail}");
                "payout policy unavailable".into()
            }
            LedgerError::Db(d) => {
                error!("mcp ledger db error: {d}");
                "internal error".into()
            }
        },
        PayoutError::Db(d) => {
            error!("mcp payout db error: {d}");
            "internal error".into()
        }
    }
}

fn to_value<T: serde::Serialize>(v: &T) -> Outcome {
    merit_api::to_json(v).map_err(merit_msg)
}

// ------------------------------------------------------------------ arguments

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NoArgs {}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MemberArgs {
    contributor_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LedgerArgs {
    #[serde(default)]
    status: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RunIdArgs {
    run_id: String,
}

fn parse_args<T: DeserializeOwned>(tool: &str, args: Value) -> Result<T, RpcError> {
    serde_json::from_value(args).map_err(|e| RpcError::invalid_params(format!("invalid arguments for {tool}: {e}")))
}

// ------------------------------------------------------------------ call

/// `tools/call`. Protocol problems (unknown tool, arguments that do not match the schema) are
/// JSON-RPC errors; everything else, including a missing scope, is a result with `isError: true`.
pub async fn call(ctx: &McpContext, p: &Principal, name: &str, args: Option<Value>) -> Result<Value, RpcError> {
    let Some(tool) = find(name) else {
        return Err(RpcError::invalid_params(format!("unknown tool '{name}'")));
    };
    if tool.name == EXECUTE_TOOL && !ctx.allow_execute {
        return Ok(failure(
            "payouts_run_execute is disabled on this server. An operator must set DSYNC_MCP_ALLOW_EXECUTE=true and restart \
             the server to enable it. Until then, execute runs through the REST API with a payouts.execute token.",
        ));
    }
    if let Some(scope) = tool.scope {
        if !p.has(scope) {
            return Ok(failure(&format!("tool '{}' requires scope '{}', which this token does not have", tool.name, scope)));
        }
    }
    let args = match args {
        None | Some(Value::Null) => json!({}),
        Some(v @ Value::Object(_)) => v,
        Some(_) => return Err(RpcError::invalid_params("arguments must be an object")),
    };
    let api = &ctx.api;
    let merit = &ctx.merit;
    let outcome: Outcome = match tool.name {
        "rubric_get" => {
            let _: NoArgs = parse_args(name, args)?;
            Ok(merit_api::rubric_view(&merit.seams.rules()))
        }
        "merit_score_submit" => {
            let input: ScoreInput = parse_args(name, args)?;
            match merit_api::submit_score(merit, p, input).await {
                Ok(r) => to_value(&r),
                Err(e) => Err(merit_msg(e)),
            }
        }
        "merit_scores_list" => {
            let input: SubjectInput = parse_args(name, args)?;
            match merit_api::list_scores(merit, p, &input).await {
                Ok(r) => to_value(&r),
                Err(e) => Err(merit_msg(e)),
            }
        }
        "leaderboard_get" => {
            let input: LeaderboardParams = parse_args(name, args)?;
            match merit_api::leaderboard_view(merit, p, &input).await {
                Ok(r) => to_value(&r),
                Err(e) => Err(merit_msg(e)),
            }
        }
        "member_merit_get" => {
            let input: MemberArgs = parse_args(name, args)?;
            match merit_api::member_merit(merit, p, &input.contributor_id).await {
                Ok(r) => to_value(&r),
                Err(e) => Err(merit_msg(e)),
            }
        }
        "payouts_policy_get" => {
            let _: NoArgs = parse_args(name, args)?;
            match policy::get_policy(&api.pool, &api.payments, p).await {
                Ok(eff) => Ok(json!({
                    "source": if eff.is_stored() { "stored" } else { "default" },
                    "version": eff.version,
                    "updated_by": eff.updated_by,
                    "updated_at": eff.updated_at,
                    "policy": eff.policy,
                })),
                Err(e) => Err(policy_msg(e)),
            }
        }
        "payouts_ledger_list" => {
            let input: LedgerArgs = parse_args(name, args)?;
            match runs::list_ledger(&api.pool, p, input.status.as_deref()).await {
                Ok(rows) => Ok(json!({ "entries": rows })),
                Err(e) => Err(payout_msg(e)),
            }
        }
        "payouts_run_preview" | "payouts_run_create" => {
            let _: NoArgs = parse_args(name, args)?;
            let dry_run = tool.name == "payouts_run_preview";
            match runs::create_run(&api.pool, &api.payments, api.directory.as_ref(), p, dry_run, Utc::now()).await {
                Ok(CreateRunResult::DryRun(plan)) => Ok(json!({"dry_run": true, "plan": plan})),
                Ok(CreateRunResult::Created { run, plan }) => Ok(json!({"dry_run": false, "run": run, "plan": plan})),
                Err(e) => Err(payout_msg(e)),
            }
        }
        "payouts_run_approve" => {
            let input: RunIdArgs = parse_args(name, args)?;
            match runs::approve_run(&api.pool, api.directory.as_ref(), p, &input.run_id).await {
                Ok(r) => Ok(json!({ "report": r })),
                Err(e) => Err(payout_msg(e)),
            }
        }
        EXECUTE_TOOL => {
            let input: RunIdArgs = parse_args(name, args)?;
            match runs::execute_run(&api.pool, api.stripe.as_ref(), &api.payments, api.directory.as_ref(), p, &input.run_id, Utc::now()).await {
                Ok(r) => Ok(json!({ "report": r })),
                Err(e) => Err(payout_msg(e)),
            }
        }
        other => {
            error!("mcp tool '{other}' is listed but has no handler");
            Err("internal error".into())
        }
    };
    Ok(match outcome {
        Ok(v) => success(v),
        Err(msg) => failure(&msg),
    })
}

// ------------------------------------------------------------------ schemas

fn no_args_schema() -> Value {
    json!({"type": "object", "properties": {}, "additionalProperties": false})
}

fn subject_schema() -> Value {
    json!({
        "type": "object",
        "description": "The change being scored.",
        "properties": {
            "provider": {"type": "string", "enum": ["github", "gitlab"]},
            "host": {"type": "string", "description": "GitHub or GitLab host. Defaults to github.com or gitlab.com."},
            "repo": {"type": "string", "description": "Repository path, for example org/repo or group/sub/project."},
            "subject_id": {
                "type": ["string", "integer"],
                "description": "The global id of the pull or merge request (GitHub pull_request.id, GitLab object_attributes.id), not its number. 'n<number>' when the id was unknown at ingest."
            }
        },
        "required": ["provider", "repo", "subject_id"],
        "additionalProperties": false
    })
}

fn subject_args_schema() -> Value {
    subject_schema()
}

fn score_submit_schema() -> Value {
    let mut props = serde_json::Map::new();
    for c in CRITERIA.iter() {
        let meanings: Vec<String> = c.levels.iter().enumerate().map(|(i, t)| format!("{i}: {t}")).collect();
        props.insert(
            c.key.to_string(),
            json!({
                "type": "object",
                "description": format!("Weight {}. Levels: {}.", c.weight, meanings.join("; ")),
                "properties": {
                    "level": {"type": "integer", "minimum": 0, "maximum": MAX_LEVEL},
                    "evidence": {"type": "string", "minLength": MIN_EVIDENCE_CHARS, "description": "What you checked that supports this level."}
                },
                "required": ["level", "evidence"],
                "additionalProperties": false
            }),
        );
    }
    let keys: Vec<&str> = CRITERIA.iter().map(|c| c.key).collect();
    json!({
        "type": "object",
        "properties": {
            "subject": subject_schema(),
            "rubric_id": {"type": "string", "const": RUBRIC_ID},
            "rubric_hash": {"type": "string", "const": RUBRIC_SHA256, "description": "Echo the hash from rubric_get."},
            "ci_passed": {"type": "boolean", "description": "false caps the score at 40."},
            "reverted": {"type": "boolean", "description": "true sets the score to 0."},
            "criteria": {"type": "object", "properties": props, "required": keys, "additionalProperties": false}
        },
        "required": ["subject", "rubric_id", "rubric_hash", "ci_passed", "reverted", "criteria"],
        "additionalProperties": false
    })
}

fn leaderboard_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "provider": {"type": "string", "enum": ["github", "gitlab"], "description": "With repo: rank by that repository alone."},
            "host": {"type": "string", "description": "Defaults to github.com or gitlab.com."},
            "repo": {"type": "string", "description": "Repository path. Requires provider."},
            "limit": {"type": "integer", "minimum": 1, "maximum": merit_api::MAX_LEADERBOARD_LIMIT, "default": merit_api::DEFAULT_LEADERBOARD_LIMIT}
        },
        "additionalProperties": false
    })
}

fn member_schema() -> Value {
    json!({
        "type": "object",
        "properties": {"contributor_id": {"type": "string", "format": "uuid", "description": "contributor_id from a leaderboard row."}},
        "required": ["contributor_id"],
        "additionalProperties": false
    })
}

fn ledger_schema() -> Value {
    json!({
        "type": "object",
        "properties": {"status": {"type": "string", "enum": ["pending_approval", "approved", "paid", "failed", "rejected"]}},
        "additionalProperties": false
    })
}

fn run_id_schema() -> Value {
    json!({
        "type": "object",
        "properties": {"run_id": {"type": "string", "description": "The run id from payouts_run_create."}},
        "required": ["run_id"],
        "additionalProperties": false
    })
}
