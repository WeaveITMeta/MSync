//! MCP tools. Each tool declares the scope it needs; `tools/list` shows only the tools the
//! caller's scopes allow, and `tools/call` checks the scope again before doing anything.
//!
//! Every tool is an adapter over a function the REST API also calls: `merit::api::*` for merit,
//! `payments::{policy, runs}` for payouts, `payments::{payees, payorders, invoices, statement}`
//! for payroll. There is no other path to a transfer: payroll tools only write ledger rows (by
//! approval), and paying them goes through payout runs and the execute guard.

use super::jsonrpc::RpcError;
use super::McpContext;
use crate::auth::{scopes, AuthError, Principal};
use crate::merit::api::{self as merit_api, LeaderboardParams, MeritApiError, ScoreInput, SubjectInput};
use crate::merit::rubric::{CRITERIA, MAX_LEVEL, MIN_EVIDENCE_CHARS, RUBRIC_ID, RUBRIC_SHA256};
use crate::payments::error::PayoutError;
use crate::payments::ledger::LedgerError;
use crate::payments::policy::{self, PolicyError};
use crate::payments::runs::{self, CreateRunResult};
use crate::payments::{invoices, payees, payorders, shifts, statement, timeclock, timesheet};
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
        description: "Returns the fixed merit rubric msync-rubric-v1: its id and pinned SHA-256 hash, the six criteria with \
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
        description: "Scores one merged or open change (a pull request or merge request) against msync-rubric-v1. You are \
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
    ToolDef {
        name: "payees_list",
        title: "List payees",
        scope: Some(scopes::PAYEES_READ),
        description: "Lists the people and companies that can be paid (payees), optionally by status (active, paused, closed), \
            with their payee keys, Stripe Connect account state and tax form status. MSync stores no bank or card numbers. \
            Compensation data is confidential; never repeat it in a public place.",
        input_schema: status_schema,
        read_only: true,
        destructive: false,
    },
    ToolDef {
        name: "payee_get",
        title: "Get one payee",
        scope: Some(scopes::PAYEES_READ),
        description: "Returns one payee by id: names, kind, email, CRA member id, payee keys, Stripe Connect account and \
            whether it can receive transfers, tax form status, status and notes. Confidential.",
        input_schema: payee_id_schema,
        read_only: true,
        destructive: false,
    },
    ToolDef {
        name: "payorders_list",
        title: "List pay orders",
        scope: Some(scopes::PAYORDERS_READ),
        description: "Lists pay orders (salaries, bonuses, pools, commissions), optionally by status (draft, active, paused, \
            ended, cancelled), each with its amount rule, schedule, approvals of the current revision and recent occurrences.",
        input_schema: status_schema,
        read_only: true,
        destructive: false,
    },
    ToolDef {
        name: "payorder_preview",
        title: "Preview a pay order",
        scope: Some(scopes::PAYORDERS_READ),
        description: "Shows what the next occurrences of a pay order would pay (default 3, at most 24): each payee's cents, \
            the leftover that is not paid, and what each occurrence still waits for. Persists nothing and moves no money.",
        input_schema: preview_schema,
        read_only: true,
        destructive: false,
    },
    ToolDef {
        name: "payorder_create",
        title: "Create a pay order",
        scope: Some(scopes::PAYORDERS_MANAGE),
        description: "Creates a pay order with an amount rule (fixed, pool_percent, pool_by_xp, percent_of_amount) and a \
            schedule (once on a date, or repeat weekly, biweekly, monthly or quarterly). Money is integer cents; percentages \
            are basis points (10000 = 100%). You are the creator (the token's member) and can never approve it; it pays \
            nothing until other members approve it. You cannot create an order that pays you.",
        input_schema: order_create_schema,
        read_only: false,
        destructive: false,
    },
    ToolDef {
        name: "payorder_approve",
        title: "Approve a pay order",
        scope: Some(scopes::PAYORDERS_APPROVE),
        description: "Records your approval (the token's member) of the current revision of a pay order. Refused when you \
            created it, made its latest change, or are paid by it. Once it has the policy's required approvals its due \
            occurrences become ledger rows; approving moves no money. Payout runs pay the rows.",
        input_schema: order_id_schema,
        read_only: false,
        destructive: false,
    },
    ToolDef {
        name: "payorder_pause",
        title: "Pause a pay order",
        scope: Some(scopes::PAYORDERS_MANAGE),
        description: "Pauses an active pay order. Occurrences that fall while it is paused are skipped and never paid later. \
            Resume and cancel are REST calls. You cannot pause an order that pays you.",
        input_schema: order_id_schema,
        read_only: false,
        destructive: false,
    },
    ToolDef {
        name: "invoices_list",
        title: "List invoices",
        scope: Some(scopes::INVOICES_READ),
        description: "Lists invoices, optionally by status (submitted, approved, rejected, scheduled, paid, void) and payee id, \
            each with its lines, totals, approvals and the history of who changed it and when.",
        input_schema: invoice_filter_schema,
        read_only: true,
        destructive: false,
    },
    ToolDef {
        name: "invoice_submit",
        title: "Submit an invoice",
        scope: Some(scopes::INVOICES_SUBMIT),
        description: "Submits an invoice for a payee: number (unique per payee), issue and due dates, line items (description, \
            whole quantity, unit amount in cents), optional tax, and the total, which must equal the lines plus tax. You are \
            the submitter (the token's member). A payee may submit their own invoice; nobody approves their own.",
        input_schema: invoice_submit_schema,
        read_only: false,
        destructive: false,
    },
    ToolDef {
        name: "invoice_approve",
        title: "Approve an invoice",
        scope: Some(scopes::INVOICES_APPROVE),
        description: "Records your approval (the token's member) of a submitted invoice. Refused for the payee and the \
            submitter. With the policy's required approvals the invoice becomes one ledger row, payable on its due date \
            through a payout run. Approving moves no money.",
        input_schema: invoice_id_schema,
        read_only: false,
        destructive: false,
    },
    ToolDef {
        name: "payout_statement",
        title: "Get a payee statement",
        scope: Some(scopes::PAYOUTS_READ),
        description: "Everything paid or pending to one payee between two dates (inclusive), grouped by source (merit, \
            pay_order, invoice), with paid and pending totals. Rejected rows are listed but not counted. Confidential.",
        input_schema: statement_schema,
        read_only: true,
        destructive: false,
    },
    ToolDef {
        name: "audit_list",
        title: "List audit log entries",
        scope: Some(scopes::AUDIT_READ),
        description: "Lists entries of the append-only, hash-chained audit log, oldest first, one page at a time: who did what \
            to what, when, with a JSON detail and each entry's hash. Filter by actor, action (exact, or a prefix ending in \
            '.' or '*', such as 'payout.*') and target. Pass next_after from a page as after to read the next one. Entries \
            never contain secrets, tokens or full Stripe account ids.",
        input_schema: audit_list_schema,
        read_only: true,
        destructive: false,
    },
    ToolDef {
        name: "audit_verify",
        title: "Verify the audit log",
        scope: Some(scopes::AUDIT_READ),
        description: "Walks the whole audit chain from the first entry and checks every link and hash. Returns ok and the \
            head (seq and hash), or the first broken entry and why: a removed entry, a broken link, or contents that no \
            longer match their hash.",
        input_schema: no_args_schema,
        read_only: true,
        destructive: false,
    },
    ToolDef {
        name: "time_clock_in",
        title: "Clock in",
        scope: Some(scopes::TIME_CLOCK),
        description: "Clocks in the token's own member for their current shift. Takes no arguments: it can never clock in \
            anyone else. The entry starts now (never before the shift start) and ends at the shift's end, which is fixed at \
            clock-in; there is no clock-out to remember. A second call for the same shift returns the same entry. Refused \
            outside every shift window (unless the member has an open-shift allowance) and after the shift's end.",
        input_schema: no_args_schema,
        read_only: false,
        destructive: false,
    },
    ToolDef {
        name: "time_entries_list",
        title: "List time entries",
        scope: Some(scopes::TIME_READ),
        description: "Lists time entries, optionally for one payee and a date range (UTC days of the entry start, \
            inclusive). Each entry has its start and end, its state (open, closed, approved, void) and its whole minutes. \
            Confidential.",
        input_schema: entries_schema,
        read_only: true,
        destructive: false,
    },
    ToolDef {
        name: "time_shifts_list",
        title: "List shifts",
        scope: Some(scopes::TIME_READ),
        description: "Lists shifts (weekly patterns, single dated shifts and open-shift allowances), optionally for one \
            payee and by status (active, ended), with their local times, IANA time zone and early clock-in window.",
        input_schema: shifts_schema,
        read_only: true,
        destructive: false,
    },
    ToolDef {
        name: "time_timesheet_get",
        title: "Get a timesheet",
        scope: Some(scopes::TIME_READ),
        description: "Returns one payee's timesheet for a pay period (a period key from a timesheet, or a date; default the \
            current period): every entry, minutes per day by state, what the approved time owes (minutes per day times \
            the rate in force that day, overtime if set, floored to the cent, with the leftover fraction), and the ledger \
            rows already written for the period. Confidential; persists nothing.",
        input_schema: timesheet_schema,
        read_only: true,
        destructive: false,
    },
    ToolDef {
        name: "time_entry_approve",
        title: "Approve a time entry",
        scope: Some(scopes::TIME_APPROVE),
        description: "Approves one closed time entry (its end has passed) as the token's member. Refused when you are the \
            payee of the entry. Approved time becomes pay when its period ends: one ledger row per payee per period, paid \
            through the normal payout approvals and runs. Approving moves no money.",
        input_schema: entry_id_schema,
        read_only: false,
        destructive: false,
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
            "idempotentHint": t.read_only
                || t.name == EXECUTE_TOOL
                || t.name == "payouts_run_approve"
                || t.name == "payorder_approve"
                || t.name == "invoice_approve"
                || t.name == "time_clock_in"
                || t.name == "time_entry_approve",
            "openWorldHint": t.name == EXECUTE_TOOL,
        },
        "_meta": {"msync/scope": t.scope},
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
        | PayoutError::Forbidden(_)
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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StatusArgs {
    #[serde(default)]
    status: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PayeeIdArgs {
    payee_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OrderIdArgs {
    order_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PreviewArgs {
    order_id: String,
    #[serde(default)]
    periods: Option<u32>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct InvoiceIdArgs {
    invoice_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EntryIdArgs {
    entry_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ShiftsArgs {
    #[serde(default)]
    payee_id: Option<String>,
    #[serde(default)]
    status: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EntriesArgs {
    #[serde(default)]
    payee_id: Option<String>,
    #[serde(default)]
    from: Option<String>,
    #[serde(default)]
    to: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TimesheetArgs {
    payee_id: String,
    #[serde(default)]
    period: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StatementArgs {
    payee_id: String,
    from: String,
    to: String,
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
            "payouts_run_execute is disabled on this server. An operator must set MSYNC_MCP_ALLOW_EXECUTE=true and restart \
             the server to enable it. Until then, execute runs through the REST API with a payouts.execute token.",
        ));
    }
    if let Some(scope) = tool.scope {
        if !p.has(scope) && !crate::payments::mutant::on("mcp_no_scope") {
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
        "payees_list" => {
            let input: StatusArgs = parse_args(name, args)?;
            match payees::list(&api.pool, p, input.status.as_deref()).await {
                Ok(v) => Ok(json!({ "payees": v })),
                Err(e) => Err(payout_msg(e)),
            }
        }
        "payee_get" => {
            let input: PayeeIdArgs = parse_args(name, args)?;
            match payees::get(&api.pool, p, &input.payee_id).await {
                Ok(v) => Ok(json!({ "payee": v })),
                Err(e) => Err(payout_msg(e)),
            }
        }
        "payorders_list" => {
            let input: StatusArgs = parse_args(name, args)?;
            match payorders::list(&api.pool, &api.payments, api.directory.as_ref(), p, input.status.as_deref()).await {
                Ok(v) => Ok(json!({ "payorders": v })),
                Err(e) => Err(payout_msg(e)),
            }
        }
        "payorder_preview" => {
            let input: PreviewArgs = parse_args(name, args)?;
            let periods = input.periods.unwrap_or(3);
            match payorders::preview(&api.pool, &api.payments, api.directory.as_ref(), p, &input.order_id, periods, Utc::now()).await {
                Ok(v) => Ok(json!({ "preview": v })),
                Err(e) => Err(payout_msg(e)),
            }
        }
        "payorder_create" => {
            let input: payorders::NewOrder = parse_args(name, args)?;
            match payorders::create(&api.pool, &api.payments, api.directory.as_ref(), p, input, Utc::now()).await {
                Ok(v) => Ok(json!({ "payorder": v })),
                Err(e) => Err(payout_msg(e)),
            }
        }
        "payorder_approve" => {
            let input: OrderIdArgs = parse_args(name, args)?;
            match payorders::approve(&api.pool, &api.payments, api.directory.as_ref(), p, &input.order_id, Utc::now()).await {
                Ok(v) => Ok(json!({ "result": v })),
                Err(e) => Err(payout_msg(e)),
            }
        }
        "payorder_pause" => {
            let input: OrderIdArgs = parse_args(name, args)?;
            match payorders::transition(&api.pool, &api.payments, api.directory.as_ref(), p, &input.order_id, payorders::Transition::Pause, Utc::now()).await {
                Ok(v) => Ok(json!({ "payorder": v })),
                Err(e) => Err(payout_msg(e)),
            }
        }
        "invoices_list" => {
            let input: invoices::InvoiceFilter = parse_args(name, args)?;
            match invoices::list(&api.pool, &api.payments, api.directory.as_ref(), p, &input, Utc::now()).await {
                Ok(v) => Ok(json!({ "invoices": v })),
                Err(e) => Err(payout_msg(e)),
            }
        }
        "invoice_submit" => {
            let input: invoices::NewInvoice = parse_args(name, args)?;
            match invoices::submit(&api.pool, &api.payments, api.directory.as_ref(), p, input, Utc::now()).await {
                Ok(v) => Ok(json!({ "invoice": v })),
                Err(e) => Err(payout_msg(e)),
            }
        }
        "invoice_approve" => {
            let input: InvoiceIdArgs = parse_args(name, args)?;
            match invoices::approve(&api.pool, &api.payments, api.directory.as_ref(), p, &input.invoice_id, Utc::now()).await {
                Ok(v) => Ok(json!({ "result": v })),
                Err(e) => Err(payout_msg(e)),
            }
        }
        "audit_list" => {
            let input: crate::audit::AuditFilter = parse_args(name, args)?;
            match crate::audit::list(&api.pool, &input).await {
                Ok(page) => Ok(json!({ "entries": page.entries, "next_after": page.next_after })),
                Err(e) => {
                    error!("mcp audit db error: {e}");
                    Err("internal error".into())
                }
            }
        }
        "audit_verify" => {
            let _: NoArgs = parse_args(name, args)?;
            match crate::audit::verify(&api.pool).await {
                Ok(report) => Ok(json!({ "verify": report })),
                Err(e) => {
                    error!("mcp audit db error: {e}");
                    Err("internal error".into())
                }
            }
        }
        "time_clock_in" => {
            let _: NoArgs = parse_args(name, args)?;
            match timeclock::clock_in(&api.pool, p, Utc::now()).await {
                Ok(v) => Ok(json!({ "result": v.result, "entry": v.entry })),
                Err(e) => Err(payout_msg(e)),
            }
        }
        "time_entries_list" => {
            let input: EntriesArgs = parse_args(name, args)?;
            let f = timeclock::EntryFilter { payee: input.payee_id, from: input.from, to: input.to };
            match timeclock::list(&api.pool, p, &f, Utc::now()).await {
                Ok(v) => Ok(json!({ "entries": v })),
                Err(e) => Err(payout_msg(e)),
            }
        }
        "time_shifts_list" => {
            let input: ShiftsArgs = parse_args(name, args)?;
            match shifts::list(&api.pool, p, input.payee_id.as_deref(), input.status.as_deref()).await {
                Ok(v) => Ok(json!({ "shifts": v })),
                Err(e) => Err(payout_msg(e)),
            }
        }
        "time_timesheet_get" => {
            let input: TimesheetArgs = parse_args(name, args)?;
            match timesheet::timesheet(&api.pool, &api.payments, api.directory.as_ref(), p, Some(&input.payee_id), input.period.as_deref(), Utc::now()).await {
                Ok(v) => Ok(json!({ "timesheet": v })),
                Err(e) => Err(payout_msg(e)),
            }
        }
        "time_entry_approve" => {
            let input: EntryIdArgs = parse_args(name, args)?;
            match timeclock::approve(&api.pool, api.directory.as_ref(), p, &input.entry_id, Utc::now()).await {
                Ok(v) => Ok(json!({ "entry": v })),
                Err(e) => Err(payout_msg(e)),
            }
        }
        "payout_statement" => {
            let input: StatementArgs = parse_args(name, args)?;
            match statement::statement(&api.pool, p, &input.payee_id, &input.from, &input.to).await {
                Ok(v) => Ok(json!({ "statement": v })),
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

fn audit_list_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "actor": {"type": "string", "description": "Only entries by this actor (a member id, admin-token, system:<name>, discord:<id>, consumer:<key>)."},
            "action": {"type": "string", "description": "An exact action such as token.created, or a prefix ending in '.' or '*' such as payout.*"},
            "target": {"type": "string", "description": "Only entries about this target, such as ledger:<id> or payee:<id>."},
            "after": {"type": "integer", "minimum": 0, "description": "Only entries with seq greater than this; next_after from the previous page."},
            "limit": {"type": "integer", "minimum": 1, "maximum": 500, "description": "Page size, default 100."}
        },
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

// ------------------------------------------------------------------ payroll schemas

fn status_schema() -> Value {
    json!({
        "type": "object",
        "properties": {"status": {"type": "string", "description": "Only rows with this status."}},
        "additionalProperties": false
    })
}

fn payee_id_schema() -> Value {
    json!({
        "type": "object",
        "properties": {"payee_id": {"type": "string", "description": "The payee id from payees_list."}},
        "required": ["payee_id"],
        "additionalProperties": false
    })
}

fn order_id_schema() -> Value {
    json!({
        "type": "object",
        "properties": {"order_id": {"type": "string", "description": "The pay order id from payorders_list."}},
        "required": ["order_id"],
        "additionalProperties": false
    })
}

fn invoice_id_schema() -> Value {
    json!({
        "type": "object",
        "properties": {"invoice_id": {"type": "string", "description": "The invoice id from invoices_list."}},
        "required": ["invoice_id"],
        "additionalProperties": false
    })
}

fn preview_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "order_id": {"type": "string"},
            "periods": {"type": "integer", "minimum": 1, "maximum": payorders::MAX_PREVIEW_PERIODS, "default": 3}
        },
        "required": ["order_id"],
        "additionalProperties": false
    })
}

fn invoice_filter_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "status": {"type": "string", "enum": ["submitted", "approved", "rejected", "scheduled", "paid", "void"]},
            "payee_id": {"type": "string"}
        },
        "additionalProperties": false
    })
}

fn statement_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "payee_id": {"type": "string"},
            "from": {"type": "string", "format": "date", "description": "YYYY-MM-DD, inclusive."},
            "to": {"type": "string", "format": "date", "description": "YYYY-MM-DD, inclusive."}
        },
        "required": ["payee_id", "from", "to"],
        "additionalProperties": false
    })
}

fn entry_id_schema() -> Value {
    json!({
        "type": "object",
        "properties": {"entry_id": {"type": "string", "description": "The time entry id from time_entries_list."}},
        "required": ["entry_id"],
        "additionalProperties": false
    })
}

fn entries_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "payee_id": {"type": "string"},
            "from": {"type": "string", "format": "date", "description": "YYYY-MM-DD, inclusive (UTC day of the entry start)."},
            "to": {"type": "string", "format": "date", "description": "YYYY-MM-DD, inclusive."}
        },
        "additionalProperties": false
    })
}

fn shifts_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "payee_id": {"type": "string"},
            "status": {"type": "string", "enum": ["active", "ended"]}
        },
        "additionalProperties": false
    })
}

fn timesheet_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "payee_id": {"type": "string"},
            "period": {"type": "string", "description": "A period key from a timesheet (such as weekly-2026-11-20) or a date YYYY-MM-DD. Default: the current period."}
        },
        "required": ["payee_id"],
        "additionalProperties": false
    })
}

fn order_create_schema() -> Value {
    let cents = json!({"type": "integer", "minimum": 0, "maximum": payorders::MAX_CENTS});
    let bps = json!({"type": "integer", "minimum": 0, "maximum": payorders::BPS_FULL, "description": "Basis points: 100 = 1%, 10000 = 100%."});
    let date = json!({"type": "string", "format": "date"});
    json!({
        "type": "object",
        "properties": {
            "name": {"type": "string", "maxLength": 200},
            "currency": {"type": "string", "description": "Defaults to the payout policy currency; must equal it."},
            "memo": {"type": "string"},
            "draft": {"type": "boolean", "description": "true: never materializes until resumed."},
            "rule": {"oneOf": [
                {"type": "object", "properties": {"kind": {"const": "fixed"}, "payee_id": {"type": "string"}, "amount_cents": cents},
                 "required": ["kind", "payee_id", "amount_cents"], "additionalProperties": false},
                {"type": "object", "properties": {"kind": {"const": "pool_percent"}, "pool_cents": cents,
                    "shares": {"type": "array", "items": {"type": "object", "properties": {"payee_id": {"type": "string"}, "bps": bps},
                        "required": ["payee_id", "bps"], "additionalProperties": false}}},
                 "required": ["kind", "pool_cents", "shares"], "additionalProperties": false,
                 "description": "Each payee gets floor(pool * bps / 10000). Shares sum to at most 10000; the rest is not paid."},
                {"type": "object", "properties": {"kind": {"const": "pool_by_xp"}, "pool_cents": cents,
                    "repos": {"type": "array", "items": {"type": "object", "properties": {"provider": {"enum": ["github", "gitlab"]}, "host": {"type": "string"}, "repo": {"type": "string"}},
                        "required": ["provider", "repo"], "additionalProperties": false}},
                    "roles": {"type": "array", "items": {"type": "string"}},
                    "payee_ids": {"type": "array", "items": {"type": "string"}},
                    "min_xp": {"type": "integer", "minimum": 0},
                    "max_share_bps": bps},
                 "required": ["kind", "pool_cents"], "additionalProperties": false,
                 "description": "Split in proportion to XP earned in each period."},
                {"type": "object", "properties": {"kind": {"const": "percent_of_amount"}, "payee_id": {"type": "string"}, "bps": bps,
                    "base_cents": cents, "bases": {"type": "object", "additionalProperties": cents, "description": "Base per occurrence date (YYYY-MM-DD)."}},
                 "required": ["kind", "payee_id", "bps"], "additionalProperties": false}
            ]},
            "schedule": {"oneOf": [
                {"type": "object", "properties": {"kind": {"const": "once"}, "date": date}, "required": ["kind", "date"], "additionalProperties": false},
                {"type": "object", "properties": {"kind": {"const": "repeat"}, "cadence": {"enum": ["weekly", "biweekly", "monthly", "quarterly"]},
                    "start": date, "end": date, "count": {"type": "integer", "minimum": 1, "maximum": payorders::MAX_OCCURRENCES}},
                 "required": ["kind", "cadence", "start"], "additionalProperties": false}
            ]}
        },
        "required": ["name", "rule", "schedule"],
        "additionalProperties": false
    })
}

fn invoice_submit_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "payee_id": {"type": "string"},
            "number": {"type": "string", "maxLength": 64, "description": "Unique per payee."},
            "issue_date": {"type": "string", "format": "date"},
            "due_date": {"type": "string", "format": "date", "description": "The invoice is payable from this date."},
            "currency": {"type": "string"},
            "lines": {"type": "array", "minItems": 1, "items": {"type": "object", "properties": {
                "description": {"type": "string"},
                "quantity": {"type": "integer", "minimum": 1},
                "unit_amount_cents": {"type": "integer", "minimum": 0}
            }, "required": ["description", "quantity", "unit_amount_cents"], "additionalProperties": false}},
            "tax_cents": {"type": "integer", "minimum": 0, "default": 0},
            "total_cents": {"type": "integer", "minimum": 1, "description": "Must equal the sum of quantity * unit_amount_cents plus tax_cents."},
            "memo": {"type": "string"},
            "attachment_ref": {"type": "string", "description": "A reference to a document kept elsewhere."}
        },
        "required": ["payee_id", "number", "issue_date", "due_date", "lines", "total_cents"],
        "additionalProperties": false
    })
}
