//! The database side of accounts: sign-in and linking (through [`rules::decide`]), sessions,
//! sign-in states, email links, rate counters, verified logins and the chat verification that
//! a completed OAuth sign-in carries outward.

use serde::Serialize;
use sqlx::{Sqlite, SqliteConnection, SqlitePool, Transaction};

use super::mutant;
use super::providers::ProviderProfile;
use super::rules::{self, Decision, Facts, Provider, Refusal, UnlinkRefusal};

#[derive(Debug, thiserror::Error)]
pub enum AccountsError {
    #[error("refused: {}", .0.code())]
    Refused(Refusal),
    #[error("the account is not active")]
    NotActive,
    #[error("random source failed")]
    Rng,
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

impl From<getrandom::Error> for AccountsError {
    fn from(_: getrandom::Error) -> Self {
        AccountsError::Rng
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignInResult {
    pub account_id: String,
    pub identity_id: String,
    pub decision: Decision,
}

fn label_for(p: &ProviderProfile) -> String {
    match p.provider {
        Provider::Email => rules::mask_email(&p.subject),
        Provider::Google => p.email.as_deref().map(rules::mask_email).unwrap_or_else(|| "Google account".into()),
        _ => p.handle.clone().map(|h| format!("@{h}")).unwrap_or_else(|| p.subject.clone()),
    }
}

async fn event(conn: &mut SqliteConnection, account: &str, kind: &str, provider: Provider, label: &str, now: i64) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO identity_events (account_id, kind, provider, label, created_at) VALUES (?, ?, ?, ?, ?)")
        .bind(account)
        .bind(kind)
        .bind(provider.as_str())
        .bind(label)
        .bind(now)
        .execute(conn)
        .await?;
    Ok(())
}

/// Complete a sign-in or an explicit link for an identity the provider has proven.
///
/// Everything happens in one transaction: gather the facts, [`rules::decide`], apply, record
/// the event, record a GitHub or GitLab login as verified, and mark matching chat bindings
/// verified. A refusal is recorded as an event and changes nothing else.
pub async fn complete_sign_in(
    pool: &SqlitePool,
    profile: &ProviderProfile,
    link_to: Option<&str>,
    now: i64,
) -> Result<SignInResult, AccountsError> {
    let mut tx = pool.begin().await?;
    let owner: Option<(String, String)> = sqlx::query_as("SELECT id, account_id FROM identities WHERE provider = ? AND subject = ?")
        .bind(profile.provider.as_str())
        .bind(&profile.subject)
        .fetch_optional(&mut *tx)
        .await?;
    let email = profile.email.as_deref().and_then(rules::normalize_email);
    let email_verified = profile.email_verified && email.is_some();
    let email_owners: Vec<String> = match (&email, email_verified) {
        (Some(e), true) => sqlx::query_scalar("SELECT DISTINCT account_id FROM identities WHERE email = ? ORDER BY account_id")
            .bind(e)
            .fetch_all(&mut *tx)
            .await?,
        _ => Vec::new(),
    };
    let link_to_has_provider = match link_to {
        Some(a) => {
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM identities WHERE account_id = ? AND provider = ?")
                .bind(a)
                .bind(profile.provider.as_str())
                .fetch_one(&mut *tx)
                .await?
                > 0
        }
        None => false,
    };
    let facts = Facts {
        provider: profile.provider,
        email: email.as_deref(),
        email_verified,
        owner: owner.as_ref().map(|(_, a)| a.as_str()),
        link_to,
        link_to_has_provider,
        email_owners: &email_owners,
    };
    let decision = rules::decide(&facts);
    let label = label_for(profile);
    let stored_email = if email_verified { email.clone() } else { None };

    let (account_id, identity_id) = match &decision {
        Decision::Refuse(r) => {
            if let Some(a) = link_to {
                event(&mut tx, a, "refused", profile.provider, &format!("{label} ({})", r.code()), now).await?;
            }
            tx.commit().await?;
            return Err(AccountsError::Refused(*r));
        }
        Decision::SignIn(account) => {
            let identity_id = match &owner {
                Some((id, _)) => id.clone(),
                None => return Err(AccountsError::Refused(Refusal::OtherAccount)),
            };
            sqlx::query(
                "UPDATE identities SET last_login_at = ?, handle = COALESCE(?, handle), email = COALESCE(?, email),
                 verified_at = ? WHERE id = ?",
            )
            .bind(now)
            .bind(&profile.handle)
            .bind(&stored_email)
            .bind(now)
            .bind(&identity_id)
            .execute(&mut *tx)
            .await?;
            (account.clone(), identity_id)
        }
        Decision::AttachTo { account, auto } => {
            let id = insert_identity(&mut tx, account, profile, stored_email.as_deref(), now).await?;
            event(&mut tx, account, if *auto { "auto_linked" } else { "linked" }, profile.provider, &label, now).await?;
            (account.clone(), id)
        }
        Decision::CreateAccount => {
            let account = rules::random_id("acc_")?;
            let name = profile
                .display_name
                .clone()
                .or_else(|| profile.handle.clone())
                .unwrap_or_else(|| "MSync member".to_string());
            sqlx::query("INSERT INTO accounts (id, display_name, status, created_at) VALUES (?, ?, 'active', ?)")
                .bind(&account)
                .bind(name.chars().take(100).collect::<String>())
                .bind(now)
                .execute(&mut *tx)
                .await?;
            let id = insert_identity(&mut tx, &account, profile, stored_email.as_deref(), now).await?;
            event(&mut tx, &account, "created", profile.provider, &label, now).await?;
            (account, id)
        }
    };

    let status: String = sqlx::query_scalar("SELECT status FROM accounts WHERE id = ?")
        .bind(&account_id)
        .fetch_one(&mut *tx)
        .await?;
    if status != "active" {
        tx.commit().await?;
        return Err(AccountsError::NotActive);
    }

    // A GitHub or GitLab login proven here is verified for this account.
    if let (Provider::Github | Provider::Gitlab, Some(login)) = (profile.provider, &profile.login) {
        sqlx::query("DELETE FROM account_verified_logins WHERE identity_id = ?")
            .bind(&identity_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "INSERT INTO account_verified_logins (provider, login, account_id, identity_id, verified_at) VALUES (?, ?, ?, ?, ?)
             ON CONFLICT (provider, login) DO UPDATE SET account_id = excluded.account_id,
             identity_id = excluded.identity_id, verified_at = excluded.verified_at",
        )
        .bind(profile.provider.as_str())
        .bind(login.trim().to_ascii_lowercase())
        .bind(&account_id)
        .bind(&identity_id)
        .bind(now)
        .execute(&mut *tx)
        .await?;
    }
    if !mutant::on("no_chat_propagation") {
        propagate_chat_verification(&mut tx, &account_id).await?;
    }
    tx.commit().await?;
    Ok(SignInResult { account_id, identity_id, decision })
}

async fn insert_identity(
    tx: &mut Transaction<'_, Sqlite>,
    account: &str,
    p: &ProviderProfile,
    email: Option<&str>,
    now: i64,
) -> Result<String, AccountsError> {
    let id = rules::random_id("idn_")?;
    let res = sqlx::query(
        "INSERT INTO identities (id, account_id, provider, subject, email, handle, verified_at, created_at, last_login_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(account)
    .bind(p.provider.as_str())
    .bind(&p.subject)
    .bind(email)
    .bind(&p.handle)
    .bind(now)
    .bind(now)
    .bind(now)
    .execute(&mut **tx)
    .await;
    match res {
        Ok(_) => Ok(id),
        // A unique index caught a race: another account took this identity, or this account
        // gained a second identity of the provider. Either way, refused.
        Err(sqlx::Error::Database(d)) if d.message().contains("UNIQUE") => {
            if d.message().contains("subject") {
                Err(AccountsError::Refused(Refusal::OtherAccount))
            } else {
                Err(AccountsError::Refused(Refusal::SecondOfProvider))
            }
        }
        Err(e) => Err(e.into()),
    }
}

/// Contributors whose code-host login this account has proven (GitHub on github.com, GitLab on
/// gitlab.com), as a subquery over `?1` = account id.
const PROVEN_CONTRIBUTORS: &str = "
    SELECT f.contributor_id FROM foundry_identities f
    JOIN account_verified_logins v ON v.provider = f.provider AND v.login = f.login
    WHERE v.account_id = ?1 AND f.host = CASE f.provider WHEN 'github' THEN 'github.com' ELSE 'gitlab.com' END
    UNION
    SELECT c.id FROM contributors c
    JOIN account_verified_logins v ON (v.provider = 'github' AND lower(c.github_username) = v.login)
                                   OR (v.provider = 'gitlab' AND lower(c.github_username) = 'gitlab:' || v.login)
    WHERE v.account_id = ?1";

/// Mark every chat binding verified whose chat account AND whose contributor's GitHub or GitLab
/// login this account has both proven by OAuth. Proving only the chat account does not prove the
/// login it was bound to by typing, so one side alone verifies nothing.
async fn propagate_chat_verification(conn: &mut SqliteConnection, account_id: &str) -> Result<u64, sqlx::Error> {
    let sql = format!(
        "UPDATE chat_identities SET verified = 1
         WHERE verified = 0
           AND EXISTS (SELECT 1 FROM identities i WHERE i.account_id = ?1 AND (
                 (chat_identities.platform = 'discord' AND i.provider = 'discord'
                    AND chat_identities.team_or_guild = '' AND i.subject = chat_identities.user_id)
              OR (chat_identities.platform = 'slack' AND i.provider = 'slack'
                    AND i.subject = chat_identities.team_or_guild || ':' || chat_identities.user_id)))
           AND contributor_id IN ({PROVEN_CONTRIBUTORS})"
    );
    Ok(sqlx::query(&sql).bind(account_id).execute(conn).await?.rows_affected())
}

/// Is this GitHub or GitLab login proven by OAuth for an active account? For the payout
/// eligibility check. The identity that proved it must still be linked.
pub async fn login_is_verified(pool: &SqlitePool, provider: crate::foundry::FoundryProvider, login: &str) -> Result<bool, sqlx::Error> {
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM account_verified_logins v
         JOIN identities i ON i.id = v.identity_id
         JOIN accounts a ON a.id = v.account_id AND a.status = 'active'
         WHERE v.provider = ? AND v.login = ?",
    )
    .bind(provider.as_str())
    .bind(login.trim().trim_start_matches('@').to_ascii_lowercase())
    .fetch_one(pool)
    .await?;
    Ok(n > 0)
}

/// Is the chat binding of `(platform, team, user)` to `contributor_key` proven: one active
/// account holds both that chat account and the contributor's GitHub or GitLab login, each
/// proven by OAuth? `payout-setup` accepts such a binding.
pub async fn chat_binding_is_verified(
    pool: &SqlitePool,
    platform: crate::chat::ChatPlatform,
    team: &str,
    user_id: &str,
    contributor_key: &str,
) -> Result<bool, sqlx::Error> {
    let (provider, subject) = match platform {
        crate::chat::ChatPlatform::Discord => ("discord", user_id.to_string()),
        crate::chat::ChatPlatform::Slack => ("slack", format!("{team}:{user_id}")),
    };
    let sql = format!(
        "SELECT COUNT(*) FROM identities i
         JOIN accounts a ON a.id = i.account_id AND a.status = 'active'
         WHERE i.provider = ?2 AND i.subject = ?3
           AND EXISTS (SELECT 1 FROM contributors c WHERE c.github_username = ?4
                       AND c.id IN ({}))",
        PROVEN_CONTRIBUTORS.replace("?1", "i.account_id")
    );
    let n: i64 = sqlx::query_scalar(&sql)
        .bind("unused")
        .bind(provider)
        .bind(subject)
        .bind(contributor_key)
        .fetch_one(pool)
        .await?;
    Ok(n > 0)
}

// ------------------------------------------------------------------ sessions

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    pub id: String,
    pub account_id: String,
    pub identity_id: Option<String>,
}

/// A new browser session. Returns the raw cookie token; only its hash is stored.
pub async fn create_session(
    pool: &SqlitePool,
    account_id: &str,
    identity_id: Option<&str>,
    ip: Option<&str>,
    user_agent: Option<&str>,
    now: i64,
) -> Result<String, AccountsError> {
    let token = rules::random_token()?;
    sqlx::query(
        "INSERT INTO account_sessions (id, account_id, identity_id, created_at, expires_at, last_used_at, ip, user_agent)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(rules::token_hash(&token))
    .bind(account_id)
    .bind(identity_id)
    .bind(now)
    .bind(now + rules::SESSION_TTL_SECS)
    .bind(now)
    .bind(ip)
    .bind(user_agent.map(|u| u.chars().take(200).collect::<String>()))
    .execute(pool)
    .await?;
    Ok(token)
}

/// The session a cookie token names, checked against the database on every request: unknown,
/// revoked, expired, or an account that is no longer active all mean no session.
pub async fn session_for_token(pool: &SqlitePool, token: &str, now: i64) -> Result<Option<SessionInfo>, sqlx::Error> {
    if !rules::token_shape_ok(token) {
        return Ok(None);
    }
    let sql = if mutant::on("session_ignore_revoked") {
        "SELECT s.id, s.account_id, s.identity_id FROM account_sessions s JOIN accounts a ON a.id = s.account_id
         WHERE s.id = ? AND s.expires_at > ? AND a.status = 'active'"
    } else {
        "SELECT s.id, s.account_id, s.identity_id FROM account_sessions s JOIN accounts a ON a.id = s.account_id
         WHERE s.id = ? AND s.revoked_at IS NULL AND s.expires_at > ? AND a.status = 'active'"
    };
    let row: Option<(String, String, Option<String>)> =
        sqlx::query_as(sql).bind(rules::token_hash(token)).bind(now).fetch_optional(pool).await?;
    let Some((id, account_id, identity_id)) = row else { return Ok(None) };
    sqlx::query("UPDATE account_sessions SET last_used_at = ? WHERE id = ?").bind(now).bind(&id).execute(pool).await?;
    Ok(Some(SessionInfo { id, account_id, identity_id }))
}

pub async fn revoke_session(pool: &SqlitePool, session_id: &str, now: i64) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE account_sessions SET revoked_at = COALESCE(revoked_at, ?) WHERE id = ?")
        .bind(now)
        .bind(session_id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn revoke_all_sessions(pool: &SqlitePool, account_id: &str, now: i64) -> Result<u64, sqlx::Error> {
    Ok(sqlx::query("UPDATE account_sessions SET revoked_at = ? WHERE account_id = ? AND revoked_at IS NULL")
        .bind(now)
        .bind(account_id)
        .execute(pool)
        .await?
        .rows_affected())
}

/// Suspend an account: every session ends on the next request.
pub async fn suspend_account(pool: &SqlitePool, account_id: &str, reason: &str, now: i64) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE accounts SET status = 'suspended', suspended_at = ?, suspended_reason = ? WHERE id = ?")
        .bind(now)
        .bind(reason)
        .bind(account_id)
        .execute(pool)
        .await?;
    revoke_all_sessions(pool, account_id, now).await?;
    Ok(())
}

// -------------------------------------------------------------------- unlink

#[derive(Debug, thiserror::Error)]
pub enum UnlinkError {
    #[error("refused: {}", .0.code())]
    Refused(UnlinkRefusal),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

/// Unlink one of the account's identities. Refused for the last one. Sessions opened with it
/// are revoked, and a GitHub or GitLab login it proved is no longer verified.
pub async fn unlink_identity(pool: &SqlitePool, account_id: &str, identity_id: &str, now: i64) -> Result<(), UnlinkError> {
    let mut tx = pool.begin().await?;
    let owned: Vec<(String, String, Option<String>, String)> =
        sqlx::query_as("SELECT id, provider, handle, subject FROM identities WHERE account_id = ? ORDER BY created_at, id")
            .bind(account_id)
            .fetch_all(&mut *tx)
            .await?;
    let ids: Vec<String> = owned.iter().map(|o| o.0.clone()).collect();
    rules::can_unlink(&ids, identity_id).map_err(UnlinkError::Refused)?;
    let (_, provider, handle, subject) = owned.into_iter().find(|o| o.0 == identity_id).expect("checked by can_unlink");
    sqlx::query("DELETE FROM identities WHERE id = ? AND account_id = ?").bind(identity_id).bind(account_id).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM account_verified_logins WHERE identity_id = ?").bind(identity_id).execute(&mut *tx).await?;
    sqlx::query("UPDATE account_sessions SET revoked_at = ? WHERE identity_id = ? AND revoked_at IS NULL")
        .bind(now)
        .bind(identity_id)
        .execute(&mut *tx)
        .await?;
    let p = Provider::parse(&provider).unwrap_or(Provider::Email);
    let label = handle.map(|h| format!("@{h}")).unwrap_or_else(|| if p == Provider::Email { rules::mask_email(&subject) } else { subject });
    event(&mut tx, account_id, "unlinked", p, &label, now).await?;
    tx.commit().await?;
    Ok(())
}

// -------------------------------------------------------------- the account view

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct AccountRow {
    pub id: String,
    pub display_name: String,
    pub status: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct IdentityView {
    pub id: String,
    pub provider: String,
    /// For display only.
    pub handle: Option<String>,
    /// Masked.
    pub email: Option<String>,
    pub verified: bool,
    pub last_login_at: i64,
    /// The identity this session signed in with.
    pub current: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct EventView {
    pub kind: String,
    pub provider: String,
    pub label: String,
    pub at: i64,
}

pub async fn account(pool: &SqlitePool, id: &str) -> Result<Option<AccountRow>, sqlx::Error> {
    let r: Option<(String, String, String, i64)> =
        sqlx::query_as("SELECT id, display_name, status, created_at FROM accounts WHERE id = ?").bind(id).fetch_optional(pool).await?;
    Ok(r.map(|(id, display_name, status, created_at)| AccountRow { id, display_name, status, created_at }))
}

pub async fn identities_of(pool: &SqlitePool, account_id: &str, current: Option<&str>) -> Result<Vec<IdentityView>, sqlx::Error> {
    #[allow(clippy::type_complexity)]
    let rows: Vec<(String, String, Option<String>, Option<String>, Option<i64>, i64)> = sqlx::query_as(
        "SELECT id, provider, handle, email, verified_at, last_login_at FROM identities WHERE account_id = ? ORDER BY created_at, id",
    )
    .bind(account_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, provider, handle, email, verified_at, last_login_at)| IdentityView {
            current: current == Some(id.as_str()),
            id,
            provider,
            handle,
            email: email.as_deref().map(rules::mask_email),
            verified: verified_at.is_some(),
            last_login_at,
        })
        .collect())
}

pub async fn events_of(pool: &SqlitePool, account_id: &str) -> Result<Vec<EventView>, sqlx::Error> {
    let rows: Vec<(String, String, String, i64)> = sqlx::query_as(
        "SELECT kind, provider, label, created_at FROM identity_events WHERE account_id = ? ORDER BY created_at DESC, id DESC LIMIT 50",
    )
    .bind(account_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|(kind, provider, label, at)| EventView { kind, provider, label, at }).collect())
}

// ---------------------------------------------------------------- CRA member

/// The CRA member an account is, from its OAuth-proven identities only:
/// GitHub and GitLab logins first; when none of them is listed, Discord and Slack ids (Slack
/// only in the organization's `[slack]` team). Two different members on the same level is
/// ambiguous and resolves to nobody (fail closed). Rank and permissions then come from the
/// snapshot; nothing is read from an identity row but who the person is.
pub async fn member_for_account(pool: &SqlitePool, snap: &crate::cra::Snapshot, account_id: &str) -> Result<Option<String>, sqlx::Error> {
    let logins: Vec<(String, String)> = sqlx::query_as(
        "SELECT v.provider, v.login FROM account_verified_logins v JOIN identities i ON i.id = v.identity_id WHERE v.account_id = ?",
    )
    .bind(account_id)
    .fetch_all(pool)
    .await?;
    let chat: Vec<(String, String)> =
        sqlx::query_as("SELECT provider, subject FROM identities WHERE account_id = ? AND provider IN ('discord', 'slack')")
            .bind(account_id)
            .fetch_all(pool)
            .await?;
    let one = |ids: std::collections::BTreeSet<String>| if ids.len() == 1 { ids.into_iter().next() } else { None };

    let mut code_host = std::collections::BTreeSet::new();
    for m in &snap.members.members {
        for (p, login) in &logins {
            let listed = match p.as_str() {
                "github" => m.identities.github.as_deref(),
                "gitlab" => m.identities.gitlab.as_deref(),
                _ => None,
            };
            if listed.is_some_and(|l| l.eq_ignore_ascii_case(login)) {
                code_host.insert(m.id.clone());
            }
        }
    }
    if !code_host.is_empty() && !mutant::on("member_any_identity") {
        return Ok(one(code_host));
    }
    let team = snap.org.slack.as_ref().map(|s| s.team_id.as_str());
    let mut chat_members = std::collections::BTreeSet::new();
    for m in &snap.members.members {
        for (p, subject) in &chat {
            let hit = match p.as_str() {
                "discord" => m.identities.discord.as_deref() == Some(subject.as_str()),
                "slack" => match (team, subject.split_once(':'), m.identities.slack.as_deref()) {
                    (Some(t), Some((st, su)), Some(ms)) => st == t && su == ms,
                    _ => false,
                },
                _ => false,
            };
            if hit {
                chat_members.insert(m.id.clone());
            }
        }
    }
    if mutant::on("member_any_identity") {
        chat_members.extend(code_host);
    }
    Ok(one(chat_members))
}

// ------------------------------------------------------------- sign-in states

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginState {
    pub pkce_verifier: String,
    pub link_account_id: Option<String>,
    pub next_path: Option<String>,
}

/// Record a sign-in attempt. Returns the `state` to send to the provider.
pub async fn create_login_state(
    pool: &SqlitePool,
    provider: Provider,
    browser_token: &str,
    pkce_verifier: &str,
    link_to: Option<&str>,
    next: Option<&str>,
    now: i64,
) -> Result<String, AccountsError> {
    let state = rules::random_token()?;
    sqlx::query(
        "INSERT INTO oauth_login_states (state_hash, browser_hash, provider, pkce_verifier, link_account_id, next_path, created_at, expires_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(rules::token_hash(&state))
    .bind(rules::token_hash(browser_token))
    .bind(provider.as_str())
    .bind(pkce_verifier)
    .bind(link_to)
    .bind(next)
    .bind(now)
    .bind(now + rules::LOGIN_STATE_TTL_SECS)
    .execute(pool)
    .await?;
    Ok(state)
}

/// Take a sign-in attempt: the `state` must exist, be unused and unexpired, be for this
/// provider, and have been started in this browser. Single use, in the same statement.
pub async fn take_login_state(
    pool: &SqlitePool,
    provider: Provider,
    state: &str,
    browser_token: &str,
    now: i64,
) -> Result<Option<LoginState>, sqlx::Error> {
    if !rules::token_shape_ok(state) {
        return Ok(None);
    }
    let browser = if mutant::on("state_ignore_browser") { "%".to_string() } else { rules::token_hash(browser_token) };
    let row: Option<(String, Option<String>, Option<String>)> = sqlx::query_as(
        "UPDATE oauth_login_states SET used_at = ?
         WHERE state_hash = ? AND used_at IS NULL AND expires_at > ? AND provider = ? AND browser_hash LIKE ?
         RETURNING pkce_verifier, link_account_id, next_path",
    )
    .bind(now)
    .bind(rules::token_hash(state))
    .bind(now)
    .bind(provider.as_str())
    .bind(browser)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(pkce_verifier, link_account_id, next_path)| LoginState { pkce_verifier, link_account_id, next_path }))
}

// ---------------------------------------------------------------- email links

/// Increment a fixed-window counter and return the count including this request.
pub async fn bump_rate(pool: &SqlitePool, key: &str, window_secs: i64, now: i64) -> Result<u32, sqlx::Error> {
    let n: i64 = sqlx::query_scalar(
        "INSERT INTO auth_rate (key, count, reset_at) VALUES (?1, 1, ?2 + ?3)
         ON CONFLICT (key) DO UPDATE SET
           count = CASE WHEN auth_rate.reset_at <= ?2 THEN 1 ELSE auth_rate.count + 1 END,
           reset_at = CASE WHEN auth_rate.reset_at <= ?2 THEN ?2 + ?3 ELSE auth_rate.reset_at END
         RETURNING count",
    )
    .bind(key)
    .bind(now)
    .bind(window_secs)
    .fetch_one(pool)
    .await?;
    Ok(u32::try_from(n).unwrap_or(u32::MAX))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EmailRequest {
    /// A link was created (and handed to the mail sender).
    Sent { token: String, email: String },
    Invalid,
    Limited(rules::RateDecision),
}

/// Create a one-time email link after the per-address and per-IP limits.
pub async fn create_email_token(
    pool: &SqlitePool,
    raw_email: &str,
    ip: &str,
    link_to: Option<&str>,
    now: i64,
) -> Result<EmailRequest, AccountsError> {
    let Some(email) = rules::normalize_email(raw_email) else { return Ok(EmailRequest::Invalid) };
    let ip_count = bump_rate(pool, &format!("ip:{ip}"), rules::PER_IP.window_secs, now).await?;
    let email_count = bump_rate(pool, &rules::email_rate_key(&email), rules::PER_EMAIL.window_secs, now).await?;
    match rules::rate_decision(ip_count, email_count) {
        rules::RateDecision::Allowed => {}
        d => return Ok(EmailRequest::Limited(d)),
    }
    let token = rules::random_token()?;
    sqlx::query(
        "INSERT INTO email_tokens (token_hash, email, link_account_id, ip, created_at, expires_at) VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(rules::token_hash(&token))
    .bind(&email)
    .bind(link_to)
    .bind(ip)
    .bind(now)
    .bind(now + rules::EMAIL_TOKEN_TTL_SECS)
    .execute(pool)
    .await?;
    Ok(EmailRequest::Sent { token, email })
}

/// Where an email token stands, without using it (mail scanners fetch links before people do).
pub async fn email_token_state(pool: &SqlitePool, token: &str, now: i64) -> Result<Option<(rules::TokenState, String)>, sqlx::Error> {
    if !rules::token_shape_ok(token) {
        return Ok(None);
    }
    let r: Option<(String, i64, Option<i64>)> = sqlx::query_as("SELECT email, expires_at, used_at FROM email_tokens WHERE token_hash = ?")
        .bind(rules::token_hash(token))
        .fetch_optional(pool)
        .await?;
    Ok(r.map(|(e, exp, used)| (rules::token_state(exp, used, now), e)))
}

/// Use an email token: single use and unexpired, enforced in the same statement.
/// Returns the address and the account it was requested to link to, if any.
pub async fn consume_email_token(pool: &SqlitePool, token: &str, now: i64) -> Result<Option<(String, Option<String>)>, sqlx::Error> {
    if !rules::token_shape_ok(token) {
        return Ok(None);
    }
    sqlx::query_as(
        "UPDATE email_tokens SET used_at = ? WHERE token_hash = ? AND used_at IS NULL AND expires_at > ? RETURNING email, link_account_id",
    )
    .bind(now)
    .bind(rules::token_hash(token))
    .bind(now)
    .fetch_optional(pool)
    .await
}

/// The profile an email link proves.
pub fn email_profile(email: &str) -> ProviderProfile {
    ProviderProfile {
        provider: Provider::Email,
        subject: email.to_string(),
        handle: None,
        display_name: email.split('@').next().map(str::to_string),
        email: Some(email.to_string()),
        email_verified: true,
        login: None,
        team: None,
    }
}
