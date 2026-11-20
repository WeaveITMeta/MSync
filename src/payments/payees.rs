//! Payees (contract 11.2): the people and companies that can be paid.
//!
//! MSync never stores a bank account or card number. There is no field for one: the payout
//! account is the payee's Stripe Connect account, and the business pays from its own Stripe
//! balance. A payee that is `paused` or `closed`, or whose Stripe account cannot receive
//! transfers, is never paid (checked in `approvals::evaluate` and inside the ledger claim).
//!
//! `stripe_payouts_enabled` is never set through this API. Only the Stripe `account.updated`
//! webhook (`ledger::update_account_status`) turns it on; changing a payee's Stripe account
//! turns it off again until Stripe confirms the new account.

use super::error::PayoutError;
use crate::auth::{
    is_payee, member_payee_keys, scopes, tokens::format_db_time, Directory, Principal,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::{SqliteConnection, SqlitePool};
use uuid::Uuid;

pub const KINDS: &[&str] = &["individual", "company"];
pub const TAX_FORM_STATUSES: &[&str] = &["none", "requested", "on_file"];
pub const STATUSES: &[&str] = &["active", "paused", "closed"];
pub const ACTIVE: &str = "active";

const MAX_TEXT: usize = 200;
const MAX_NOTES: usize = 2000;
const MAX_KEYS: usize = 20;

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct PayeeRow {
    pub id: String,
    pub org_id: String,
    pub display_name: String,
    pub legal_name: Option<String>,
    pub kind: String,
    pub email: Option<String>,
    pub member_id: Option<String>,
    pub stripe_account_id: Option<String>,
    pub stripe_payouts_enabled: bool,
    pub tax_form_status: String,
    pub status: String,
    pub notes: Option<String>,
    pub created_by: String,
    pub created_at: String,
    pub updated_by: String,
    pub updated_at: String,
}

/// A payee as the API shows it: the row plus its payee keys.
#[derive(Debug, Clone, Serialize)]
pub struct Payee {
    #[serde(flatten)]
    pub row: PayeeRow,
    pub payee_keys: Vec<String>,
}

impl Payee {
    /// The payee key written to the ledger `github_username` of this payee's rows: its first
    /// key (alphabetical), or `payee:<id>` when it has none. Blocks and per-member limits key on it.
    pub fn ledger_key(&self) -> String {
        self.payee_keys
            .first()
            .cloned()
            .unwrap_or_else(|| format!("payee:{}", self.row.id))
    }

    pub fn is_active(&self) -> bool {
        self.row.status == ACTIVE
    }

    /// Can this payee be paid right now (status and Stripe account)?
    pub fn payable(&self) -> Result<(), String> {
        if !self.is_active() {
            return Err(format!("payee is {}", self.row.status));
        }
        match &self.row.stripe_account_id {
            Some(a) if !a.is_empty() => {}
            _ => return Err("payee has no Stripe account".into()),
        }
        if !self.row.stripe_payouts_enabled {
            return Err("payee's Stripe account cannot receive transfers yet".into());
        }
        Ok(())
    }
}

/// POST /api/v1/payees. There is no bank or card field, by design.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewPayee {
    pub display_name: String,
    #[serde(default)]
    pub legal_name: Option<String>,
    pub kind: String,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub member_id: Option<String>,
    #[serde(default)]
    pub payee_keys: Vec<String>,
    /// A Stripe Connect account id (`acct_...`). Starts not enabled.
    #[serde(default)]
    pub stripe_account_id: Option<String>,
    #[serde(default)]
    pub tax_form_status: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
}

/// PATCH /api/v1/payees/{id}. Absent fields are unchanged; `null` clears an optional field.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PayeePatch {
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default, with = "double_option")]
    pub legal_name: Option<Option<String>>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default, with = "double_option")]
    pub email: Option<Option<String>>,
    #[serde(default, with = "double_option")]
    pub member_id: Option<Option<String>>,
    #[serde(default)]
    pub payee_keys: Option<Vec<String>>,
    #[serde(default, with = "double_option")]
    pub stripe_account_id: Option<Option<String>>,
    #[serde(default)]
    pub tax_form_status: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default, with = "double_option")]
    pub notes: Option<Option<String>>,
}

/// `Option<Option<T>>`: absent = None, `null` = Some(None), value = Some(Some(v)).
pub(crate) mod double_option {
    use serde::{Deserialize, Deserializer};
    pub fn deserialize<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
    where
        D: Deserializer<'de>,
        T: Deserialize<'de>,
    {
        Ok(Some(Option::<T>::deserialize(d)?))
    }
}

fn clean(s: &str) -> Option<String> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

fn check_len(errs: &mut Vec<String>, field: &str, v: &Option<String>, max: usize) {
    if let Some(v) = v {
        if v.chars().count() > max {
            errs.push(format!("{field} must be at most {max} characters"));
        }
    }
}

fn valid_stripe_account(a: &str) -> bool {
    a.len() > 5
        && a.len() <= 64
        && a.starts_with("acct_")
        && a[5..].chars().all(|c| c.is_ascii_alphanumeric())
}

/// Lowercased, trimmed, unique, sorted payee keys; errors for empty or malformed keys.
fn normalize_keys(keys: &[String], errs: &mut Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if keys.len() > MAX_KEYS {
        errs.push(format!("at most {MAX_KEYS} payee_keys"));
    }
    for k in keys {
        let k = k.trim().to_lowercase();
        if k.is_empty()
            || k.len() > 100
            || k.chars().any(char::is_whitespace)
            || k.starts_with("payee:")
        {
            errs.push(format!("invalid payee key '{k}'"));
            continue;
        }
        if !out.contains(&k) {
            out.push(k);
        }
    }
    out.sort();
    out
}

struct Fields {
    display_name: String,
    legal_name: Option<String>,
    kind: String,
    email: Option<String>,
    member_id: Option<String>,
    payee_keys: Vec<String>,
    stripe_account_id: Option<String>,
    tax_form_status: String,
    status: String,
    notes: Option<String>,
}

fn validate(f: &Fields) -> Vec<String> {
    let mut errs = Vec::new();
    if f.display_name.is_empty() {
        errs.push("display_name is required".into());
    }
    check_len(
        &mut errs,
        "display_name",
        &Some(f.display_name.clone()),
        MAX_TEXT,
    );
    check_len(&mut errs, "legal_name", &f.legal_name, MAX_TEXT);
    check_len(&mut errs, "email", &f.email, MAX_TEXT);
    check_len(&mut errs, "member_id", &f.member_id, MAX_TEXT);
    check_len(&mut errs, "notes", &f.notes, MAX_NOTES);
    if !KINDS.contains(&f.kind.as_str()) {
        errs.push("kind must be individual or company".into());
    }
    if !TAX_FORM_STATUSES.contains(&f.tax_form_status.as_str()) {
        errs.push("tax_form_status must be none, requested or on_file".into());
    }
    if !STATUSES.contains(&f.status.as_str()) {
        errs.push("status must be active, paused or closed".into());
    }
    if let Some(e) = &f.email {
        if !e.contains('@') {
            errs.push("email is not an email address".into());
        }
    }
    if let Some(a) = &f.stripe_account_id {
        if !valid_stripe_account(a) {
            errs.push("stripe_account_id must be a Stripe Connect account id (acct_...)".into());
        }
    }
    errs
}

// ------------------------------------------------------------------ reads

pub async fn keys_of(
    conn: &mut SqliteConnection,
    payee_id: &str,
) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar("SELECT payee_key FROM payee_keys WHERE payee_id = ? ORDER BY payee_key")
        .bind(payee_id)
        .fetch_all(&mut *conn)
        .await
}

/// Load one payee (no scope check; library use).
pub async fn load(pool: &SqlitePool, id: &str) -> Result<Option<Payee>, sqlx::Error> {
    let mut conn = pool.acquire().await?;
    load_conn(&mut conn, id).await
}

pub async fn load_conn(
    conn: &mut SqliteConnection,
    id: &str,
) -> Result<Option<Payee>, sqlx::Error> {
    let Some(row) = sqlx::query_as::<_, PayeeRow>("SELECT * FROM payees WHERE id = ?")
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?
    else {
        return Ok(None);
    };
    let payee_keys = keys_of(conn, id).await?;
    Ok(Some(Payee { row, payee_keys }))
}

/// The payee paid under a payee key, if any.
pub async fn by_key(pool: &SqlitePool, key: &str) -> Result<Option<Payee>, sqlx::Error> {
    let id: Option<String> =
        sqlx::query_scalar("SELECT payee_id FROM payee_keys WHERE payee_key = ?")
            .bind(key.trim().to_lowercase())
            .fetch_optional(pool)
            .await?;
    match id {
        Some(id) => load(pool, &id).await,
        None => Ok(None),
    }
}

/// GET /api/v1/payees (`payees.read`). Optional status filter.
pub async fn list(
    pool: &SqlitePool,
    principal: &Principal,
    status: Option<&str>,
) -> Result<Vec<Payee>, PayoutError> {
    principal.require(scopes::PAYEES_READ)?;
    let ids: Vec<String> = match status {
        Some(s) => {
            sqlx::query_scalar(
                "SELECT id FROM payees WHERE org_id = ? AND status = ? ORDER BY display_name, id",
            )
            .bind(&principal.org_id)
            .bind(s)
            .fetch_all(pool)
            .await?
        }
        None => {
            sqlx::query_scalar("SELECT id FROM payees WHERE org_id = ? ORDER BY display_name, id")
                .bind(&principal.org_id)
                .fetch_all(pool)
                .await?
        }
    };
    let mut out = Vec::new();
    for id in ids {
        if let Some(p) = load(pool, &id).await? {
            out.push(p);
        }
    }
    Ok(out)
}

/// GET /api/v1/payees/{id} (`payees.read`).
pub async fn get(pool: &SqlitePool, principal: &Principal, id: &str) -> Result<Payee, PayoutError> {
    principal.require(scopes::PAYEES_READ)?;
    let p = load(pool, id)
        .await?
        .ok_or(PayoutError::NotFound("payee"))?;
    if p.row.org_id != principal.org_id {
        return Err(PayoutError::NotFound("payee"));
    }
    Ok(p)
}

// ------------------------------------------------------------------ who is the payee

/// Is the principal the person paid through this payee record? True when the record's member
/// id is the principal's member, or any of its payee keys belongs to the principal (token keys,
/// keys declared on any of the member's tokens, or the directory).
pub async fn principal_is(
    pool: &SqlitePool,
    directory: &dyn Directory,
    principal: &Principal,
    member_id: Option<&str>,
    keys: &[String],
) -> Result<bool, sqlx::Error> {
    if member_id == Some(principal.member_id.as_str()) {
        return Ok(true);
    }
    for k in keys {
        let k = k.to_lowercase();
        if principal.payee_keys.iter().any(|p| p.to_lowercase() == k) {
            return Ok(true);
        }
        if is_payee(pool, directory, &principal.member_id, &k).await? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Is the principal the payee of the stored payee `payee_id`?
pub async fn principal_is_payee_of(
    pool: &SqlitePool,
    directory: &dyn Directory,
    principal: &Principal,
    payee_id: &str,
) -> Result<bool, sqlx::Error> {
    match load(pool, payee_id).await? {
        Some(p) => {
            principal_is(
                pool,
                directory,
                principal,
                p.row.member_id.as_deref(),
                &p.payee_keys,
            )
            .await
        }
        None => Ok(false),
    }
}

/// Could the principal be paid by an order open to any payee (a `pool_by_xp` with no payee
/// list)? Yes when they own any payee record or have declared any payee key. Fail closed.
pub async fn principal_could_be_any_payee(
    pool: &SqlitePool,
    principal: &Principal,
) -> Result<bool, sqlx::Error> {
    if !principal.payee_keys.is_empty() {
        return Ok(true);
    }
    if !member_payee_keys(pool, &principal.member_id)
        .await?
        .is_empty()
    {
        return Ok(true);
    }
    let own: Option<i64> = sqlx::query_scalar("SELECT 1 FROM payees WHERE member_id = ? LIMIT 1")
        .bind(&principal.member_id)
        .fetch_optional(pool)
        .await?;
    Ok(own.is_some())
}

// ------------------------------------------------------------------ writes

async fn record_event(
    conn: &mut SqliteConnection,
    payee_id: &str,
    action: &str,
    actor: &str,
    detail: serde_json::Value,
    now: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO payee_events (payee_id, action, actor, detail, created_at) VALUES (?, ?, ?, ?, ?)")
        .bind(payee_id)
        .bind(action)
        .bind(actor)
        .bind(detail.to_string())
        .bind(now)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

async fn write_keys(
    conn: &mut SqliteConnection,
    payee_id: &str,
    org: &str,
    keys: &[String],
) -> Result<(), PayoutError> {
    for k in keys {
        let owner: Option<String> =
            sqlx::query_scalar("SELECT payee_id FROM payee_keys WHERE payee_key = ?")
                .bind(k)
                .fetch_optional(&mut *conn)
                .await?;
        if let Some(o) = owner {
            if o != payee_id {
                return Err(PayoutError::Invalid(vec![format!(
                    "payee key '{k}' already belongs to another payee"
                )]));
            }
        }
    }
    sqlx::query("DELETE FROM payee_keys WHERE payee_id = ?")
        .bind(payee_id)
        .execute(&mut *conn)
        .await?;
    for k in keys {
        sqlx::query("INSERT INTO payee_keys (payee_key, payee_id, org_id) VALUES (?, ?, ?)")
            .bind(k)
            .bind(payee_id)
            .bind(org)
            .execute(&mut *conn)
            .await?;
    }
    Ok(())
}

/// With no Stripe account of its own, a payee takes the Stripe account and payout-enabled state
/// of the contributor with one of its payee keys (contract 11.2 link by payee key).
async fn link_contributor_account(
    conn: &mut SqliteConnection,
    payee_id: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE payees SET
           stripe_account_id = (SELECT c.stripe_account_id FROM contributors c JOIN payee_keys k ON k.payee_key = lower(c.github_username)
                                WHERE k.payee_id = payees.id AND c.stripe_account_id IS NOT NULL ORDER BY k.payee_key LIMIT 1),
           stripe_payouts_enabled = COALESCE((SELECT c.stripe_payouts_enabled FROM contributors c JOIN payee_keys k ON k.payee_key = lower(c.github_username)
                                WHERE k.payee_id = payees.id AND c.stripe_account_id IS NOT NULL ORDER BY k.payee_key LIMIT 1), 0)
         WHERE id = ? AND stripe_account_id IS NULL",
    )
    .bind(payee_id)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// POST /api/v1/payees (`payees.manage`). A member cannot create a payee record that pays
/// themselves. The creator is the token's member.
pub async fn create(
    pool: &SqlitePool,
    directory: &dyn Directory,
    principal: &Principal,
    input: NewPayee,
    now: DateTime<Utc>,
) -> Result<Payee, PayoutError> {
    principal.require(scopes::PAYEES_MANAGE)?;
    let mut errs = Vec::new();
    let keys = normalize_keys(&input.payee_keys, &mut errs);
    let f = Fields {
        display_name: input.display_name.trim().to_string(),
        legal_name: input.legal_name.as_deref().and_then(clean),
        kind: input.kind.trim().to_string(),
        email: input.email.as_deref().and_then(clean),
        member_id: input.member_id.as_deref().and_then(clean),
        payee_keys: keys,
        stripe_account_id: input.stripe_account_id.as_deref().and_then(clean),
        tax_form_status: input
            .tax_form_status
            .as_deref()
            .map(str::trim)
            .unwrap_or("none")
            .to_string(),
        status: ACTIVE.into(),
        notes: input.notes.as_deref().and_then(clean),
    };
    errs.extend(validate(&f));
    if !errs.is_empty() {
        return Err(PayoutError::Invalid(errs));
    }
    if principal_is(
        pool,
        directory,
        principal,
        f.member_id.as_deref(),
        &f.payee_keys,
    )
    .await?
    {
        return Err(PayoutError::Forbidden(
            "a payee cannot create or edit their own payee record".into(),
        ));
    }
    let id = Uuid::new_v4().to_string();
    let now_s = format_db_time(now);
    let mut tx = pool.begin().await?;
    sqlx::query(
        "INSERT INTO payees (id, org_id, display_name, legal_name, kind, email, member_id, stripe_account_id,
         stripe_payouts_enabled, tax_form_status, status, notes, created_by, created_at, updated_by, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, 0, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&principal.org_id)
    .bind(&f.display_name)
    .bind(&f.legal_name)
    .bind(&f.kind)
    .bind(&f.email)
    .bind(&f.member_id)
    .bind(&f.stripe_account_id)
    .bind(&f.tax_form_status)
    .bind(&f.status)
    .bind(&f.notes)
    .bind(&principal.member_id)
    .bind(&now_s)
    .bind(&principal.member_id)
    .bind(&now_s)
    .execute(&mut *tx)
    .await?;
    write_keys(&mut tx, &id, &principal.org_id, &f.payee_keys).await?;
    link_contributor_account(&mut tx, &id).await?;
    record_event(
        &mut tx,
        &id,
        "create",
        &principal.member_id,
        json!({}),
        &now_s,
    )
    .await?;
    crate::audit::append_in(
        &mut tx,
        &crate::audit::AuditEntry::by_principal(
            principal,
            "payee.created",
            format!("payee:{id}"),
            json!({
                "payee_id": id,
                "display_name": f.display_name,
                "kind": f.kind,
                "member_id": f.member_id,
                "payee_keys": f.payee_keys,
                "stripe_account": f.stripe_account_id.as_deref().map(crate::audit::stripe_last4),
                "status": f.status,
            }),
        ),
    )
    .await?;
    tx.commit().await?;
    load(pool, &id).await?.ok_or(PayoutError::NotFound("payee"))
}

/// PATCH /api/v1/payees/{id} (`payees.manage`). A member cannot edit a payee record that pays
/// them, before or after the change. Changing the Stripe account turns payouts off for the payee
/// until the Stripe webhook confirms the new account.
pub async fn update(
    pool: &SqlitePool,
    directory: &dyn Directory,
    principal: &Principal,
    id: &str,
    patch: PayeePatch,
    now: DateTime<Utc>,
) -> Result<Payee, PayoutError> {
    principal.require(scopes::PAYEES_MANAGE)?;
    let cur = load(pool, id)
        .await?
        .ok_or(PayoutError::NotFound("payee"))?;
    if cur.row.org_id != principal.org_id {
        return Err(PayoutError::NotFound("payee"));
    }
    if principal_is(
        pool,
        directory,
        principal,
        cur.row.member_id.as_deref(),
        &cur.payee_keys,
    )
    .await?
    {
        return Err(PayoutError::Forbidden(
            "a payee cannot create or edit their own payee record".into(),
        ));
    }
    let mut errs = Vec::new();
    let mut changed: Vec<&str> = Vec::new();
    let keys = match &patch.payee_keys {
        Some(k) => {
            changed.push("payee_keys");
            normalize_keys(k, &mut errs)
        }
        None => cur.payee_keys.clone(),
    };
    macro_rules! pick {
        ($field:ident, $cur:expr) => {
            match &patch.$field {
                Some(v) => {
                    changed.push(stringify!($field));
                    v.as_deref().and_then(clean)
                }
                None => $cur,
            }
        };
    }
    let f = Fields {
        display_name: match &patch.display_name {
            Some(v) => {
                changed.push("display_name");
                v.trim().to_string()
            }
            None => cur.row.display_name.clone(),
        },
        legal_name: pick!(legal_name, cur.row.legal_name.clone()),
        kind: match &patch.kind {
            Some(v) => {
                changed.push("kind");
                v.trim().to_string()
            }
            None => cur.row.kind.clone(),
        },
        email: pick!(email, cur.row.email.clone()),
        member_id: pick!(member_id, cur.row.member_id.clone()),
        payee_keys: keys,
        stripe_account_id: pick!(stripe_account_id, cur.row.stripe_account_id.clone()),
        tax_form_status: match &patch.tax_form_status {
            Some(v) => {
                changed.push("tax_form_status");
                v.trim().to_string()
            }
            None => cur.row.tax_form_status.clone(),
        },
        status: match &patch.status {
            Some(v) => {
                changed.push("status");
                v.trim().to_string()
            }
            None => cur.row.status.clone(),
        },
        notes: pick!(notes, cur.row.notes.clone()),
    };
    errs.extend(validate(&f));
    if cur.row.status == "closed" && f.status != "closed" {
        errs.push("a closed payee cannot be reopened; create a new payee".into());
    }
    if !errs.is_empty() {
        return Err(PayoutError::Invalid(errs));
    }
    if principal_is(
        pool,
        directory,
        principal,
        f.member_id.as_deref(),
        &f.payee_keys,
    )
    .await?
    {
        return Err(PayoutError::Forbidden(
            "a payee cannot create or edit their own payee record".into(),
        ));
    }
    let account_changed = f.stripe_account_id != cur.row.stripe_account_id;
    let now_s = format_db_time(now);
    let mut tx = pool.begin().await?;
    sqlx::query(
        "UPDATE payees SET display_name = ?, legal_name = ?, kind = ?, email = ?, member_id = ?, stripe_account_id = ?,
         stripe_payouts_enabled = CASE WHEN ? THEN 0 ELSE stripe_payouts_enabled END,
         tax_form_status = ?, status = ?, notes = ?, updated_by = ?, updated_at = ? WHERE id = ?",
    )
    .bind(&f.display_name)
    .bind(&f.legal_name)
    .bind(&f.kind)
    .bind(&f.email)
    .bind(&f.member_id)
    .bind(&f.stripe_account_id)
    .bind(account_changed)
    .bind(&f.tax_form_status)
    .bind(&f.status)
    .bind(&f.notes)
    .bind(&principal.member_id)
    .bind(&now_s)
    .bind(id)
    .execute(&mut *tx)
    .await?;
    if patch.payee_keys.is_some() {
        write_keys(&mut tx, id, &principal.org_id, &f.payee_keys).await?;
    }
    link_contributor_account(&mut tx, id).await?;
    record_event(
        &mut tx,
        id,
        "update",
        &principal.member_id,
        json!({ "fields": changed }),
        &now_s,
    )
    .await?;
    // Always audited, and always with the Stripe account change spelled out (last four only).
    crate::audit::append_in(
        &mut tx,
        &crate::audit::AuditEntry::by_principal(
            principal,
            "payee.updated",
            format!("payee:{id}"),
            json!({
                "payee_id": id,
                "fields": changed,
                "stripe_account_changed": account_changed,
                "stripe_account_before": cur.row.stripe_account_id.as_deref().map(crate::audit::stripe_last4),
                "stripe_account_after": f.stripe_account_id.as_deref().map(crate::audit::stripe_last4),
                "status_before": cur.row.status,
                "status_after": f.status,
                "member_id": f.member_id,
                "payee_keys": f.payee_keys,
            }),
        ),
    )
    .await?;
    tx.commit().await?;
    load(pool, id).await?.ok_or(PayoutError::NotFound("payee"))
}
