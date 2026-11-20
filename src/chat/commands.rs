//! The chat commands, defined once for Discord and Slack (contract 12.2 and 14.6).
//!
//! Every command is a plain async function: who asked ([`ChatActor`]) and its arguments in,
//! a [`ChatReply`] out. Nothing here knows a Discord or Slack type.
//!
//! Identity rules:
//!
//! - With a CRA snapshot active, a chat account is whoever `members.toml` says it is. `link`
//!   binds nothing and only reports that. An attested member counts as verified.
//! - With no CRA, `link` binds a chat account to a GitHub or GitLab login, unverified. It
//!   refuses a login already bound to a different account on the same platform, and it
//!   refuses to change the caller's existing binding. `payout-setup` refuses every
//!   unverified binding, so typing someone else's login can never open a payout account.

use std::collections::BTreeMap;

use crate::cra::loader::{cached_merits, member_foundry_logins};
use crate::cra::model::{Identities, Member, MemberStatus};
use crate::cra::{discord_projection, slack_projection, Snapshot};
use crate::foundry::identity::{find_contributor, resolve_contributor};
use crate::foundry::{legacy_username, FoundryProvider};
use crate::models::Contributor;

use super::identity::{self, Binding};
use super::{ChatActor, ChatCommand, ChatContext, ChatPlatform, ChatReply, ProjectionApplier, ProjectionPlan};

/// Run one command. `arg` is the command's text argument (the login for `link`).
pub async fn run(ctx: &ChatContext, actor: &ChatActor, cmd: ChatCommand, arg: &str, applier: &dyn ProjectionApplier) -> ChatReply {
    match cmd {
        ChatCommand::Link => link(ctx, actor, arg).await,
        ChatCommand::Stats => stats(ctx, actor).await,
        ChatCommand::Leaderboard => leaderboard(ctx, actor).await,
        ChatCommand::Sync => sync(ctx, actor, applier).await,
        ChatCommand::Payouts => payouts(ctx, actor).await,
        ChatCommand::PayoutSetup => payout_setup(ctx, actor).await,
        ChatCommand::ClockIn => clock_in(ctx, actor, chrono::Utc::now()).await,
        ChatCommand::Shift => shift(ctx, actor, chrono::Utc::now()).await,
    }
}

// ------------------------------------------------------------- resolution

/// Who a chat account is.
#[derive(Debug, Clone, PartialEq)]
pub enum Caller {
    /// Not in the CRA (CRA active) or not bound (no CRA).
    Unknown,
    /// CRA active: the member `members.toml` lists for this chat account. Attested by the
    /// administration, so verified. `contributor` is the merit row their GitHub or GitLab
    /// identities lead to, when there is one.
    Member { id: String, name: String, status: MemberStatus, identities: Identities, contributor: Option<String> },
    /// No CRA: a binding made in chat.
    Bound(Binding),
}

/// The member the CRA lists for this chat account. On Slack the team must be the CRA's
/// `[slack]` team when the CRA names one.
pub fn cra_member<'a>(snap: &'a Snapshot, actor: &ChatActor) -> Option<&'a Member> {
    if actor.platform == ChatPlatform::Slack {
        if let Some(sl) = &snap.org.slack {
            if sl.team_id != actor.team_id {
                return None;
            }
        }
    }
    snap.members
        .members
        .iter()
        .find(|m| actor.platform.identity_of(&m.identities) == Some(actor.user_id.as_str()))
}

/// Resolve the caller. With `create`, a CRA member whose logins have no contributor row yet
/// gets one (payout setup needs a row to hold the Stripe account).
pub async fn resolve(ctx: &ChatContext, actor: &ChatActor, create: bool) -> Result<Caller, sqlx::Error> {
    if let Some(active) = ctx.cra.active() {
        let snap = &active.snapshot;
        let Some(m) = cra_member(snap, actor) else { return Ok(Caller::Unknown) };
        let logins = member_foundry_logins(snap, &m.id);
        let mut contributor = None;
        for (p, host, login) in &logins {
            if let Some(c) = find_contributor(&ctx.pool, *p, host, login).await? {
                contributor = Some(c.github_username);
                break;
            }
        }
        if contributor.is_none() && create {
            if let Some((p, host, login)) = logins.first() {
                contributor = Some(resolve_contributor(&ctx.pool, *p, host, login).await?.github_username);
            }
        }
        return Ok(Caller::Member {
            id: m.id.clone(),
            name: m.name.clone(),
            status: m.status,
            identities: m.identities.clone(),
            contributor,
        });
    }
    Ok(match identity::binding_for(&ctx.pool, actor).await? {
        Some(b) => Caller::Bound(b),
        None => Caller::Unknown,
    })
}

// ------------------------------------------------------------------- link

/// Parse `link`'s argument: `alice` or `github:alice` for GitHub, `gitlab:alice` for GitLab.
pub fn parse_login(arg: &str) -> Result<(FoundryProvider, String), String> {
    let arg = arg.trim();
    let (provider, login) = match arg.split_once(':') {
        Some((p, l)) => match FoundryProvider::parse(p) {
            Some(p) => (p, l.trim()),
            None => return Err(format!("❌ `{}` is not a GitHub or GitLab login. Use `alice`, `github:alice` or `gitlab:alice`.", arg)),
        },
        None => (FoundryProvider::GitHub, arg),
    };
    let login = login.trim_start_matches('@');
    if login.is_empty() {
        return Err("❌ Please provide your GitHub username!".to_string());
    }
    if login.len() > 255 || !login.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')) {
        return Err(format!("❌ `{}` is not a valid {} login.", login, provider_label(provider)));
    }
    Ok((provider, login.to_string()))
}

fn provider_label(p: FoundryProvider) -> &'static str {
    match p {
        FoundryProvider::GitHub => "GitHub",
        FoundryProvider::GitLab => "GitLab",
    }
}

fn cra_identities_text(ids: &Identities) -> String {
    let mut parts = Vec::new();
    if let Some(g) = &ids.github {
        parts.push(format!("GitHub `{g}`"));
    }
    if let Some(g) = &ids.gitlab {
        parts.push(format!("GitLab `{g}`"));
    }
    if let Some(d) = &ids.discord {
        parts.push(format!("Discord `{d}`"));
    }
    if let Some(s) = &ids.slack {
        parts.push(format!("Slack `{s}`"));
    }
    if parts.is_empty() {
        "no identities".to_string()
    } else {
        parts.join(", ")
    }
}

/// Audit a refused `link` (contract 13.1). The refusal already happened, so this is best
/// effort: a failed write is logged and the reply is unchanged.
async fn audit_link_refused(ctx: &ChatContext, actor: &ChatActor, login: &str, reason: &str) {
    let who = match actor.platform {
        ChatPlatform::Discord => format!("discord:{}", actor.user_id),
        _ => format!("{}:{}:{}", actor.platform.as_str(), actor.team_id, actor.user_id),
    };
    let login: String = login.trim().chars().take(100).collect();
    crate::audit::append_best_effort(
        &ctx.pool,
        &crate::audit::AuditEntry::new(
            crate::audit::ActorKind::Chat,
            who,
            "chat.link.refused",
            format!("login:{login}"),
            serde_json::json!({
                "platform": actor.platform.as_str(),
                "team_or_guild": actor.storage_team(),
                "user_id": actor.user_id,
                "login": login,
                "reason": reason,
            }),
        ),
    )
    .await;
}

/// `link`: bind this chat account to a GitHub or GitLab login, or, with a CRA, report what
/// the CRA says. Never overwrites a binding.
pub async fn link(ctx: &ChatContext, actor: &ChatActor, arg: &str) -> ChatReply {
    let p = actor.platform;
    if ctx.cra.active().is_some() {
        audit_link_refused(ctx, actor, arg, "cra_active").await;
        let head = "🔒 Identities come from the CRA member list (`members.toml`), so link changes nothing.";
        return match resolve(ctx, actor, false).await {
            Ok(Caller::Member { id, name, identities, .. }) => ChatReply::private(format!(
                "{head}\nThe CRA lists this {} account as `{id}` ({name}): {}.\nTo change an identity, ask an administrator to edit `members.toml`.",
                p.label(),
                cra_identities_text(&identities)
            )),
            Ok(_) => ChatReply::private(format!(
                "{head}\nThis {} account is not in the member list. Ask an administrator to add your {} id to `members.toml`.",
                p.label(),
                p.label()
            )),
            Err(e) => ChatReply::private(format!("❌ Database error: {}", e)),
        };
    }

    let (provider, login) = match parse_login(arg) {
        Ok(x) => x,
        Err(msg) => return ChatReply::private(msg),
    };
    let host = provider.default_host();
    let key = legacy_username(provider, host, &login);

    // 1. The caller's binding never changes from chat.
    match identity::binding_for(&ctx.pool, actor).await {
        Ok(Some(b)) if b.contributor.eq_ignore_ascii_case(&key) => {
            return ChatReply::private(format!("✅ Your {} account is already linked to `{}`. Nothing changed.", p.label(), b.contributor));
        }
        Ok(Some(b)) => {
            audit_link_refused(ctx, actor, &key, "caller_already_linked").await;
            return ChatReply::private(format!(
                "❌ Your {} account is already linked to `{}`. A link cannot be changed from chat; ask an administrator.",
                p.label(),
                b.contributor
            ));
        }
        Ok(None) => {}
        Err(e) => return ChatReply::private(format!("❌ Database error: {}", e)),
    }

    // 2. A login bound to a different account on this platform stays where it is.
    let taken = format!(
        "❌ `{}` is already linked to a different {} account. If it is yours, ask an administrator to add you to the CRA member list.",
        login,
        p.label()
    );
    let existing = match find_contributor(&ctx.pool, provider, host, &login).await {
        Ok(c) => c,
        Err(e) => return ChatReply::private(format!("❌ Database error: {}", e)),
    };
    if let Some(c) = &existing {
        match identity::accounts_of(&ctx.pool, p, &c.github_username).await {
            Ok(accounts) if accounts.is_empty() => {}
            Ok(_) => {
                audit_link_refused(ctx, actor, &key, "login_taken").await;
                return ChatReply::private(taken);
            }
            Err(e) => return ChatReply::private(format!("❌ Database error: {}", e)),
        }
        if p == ChatPlatform::Discord {
            if let Some(d) = c.discord_user_id {
                if d.to_string() != actor.user_id {
                    audit_link_refused(ctx, actor, &key, "login_taken").await;
                    return ChatReply::private(taken);
                }
            }
        }
    }

    // 3. Bind, unverified.
    let contributor: Contributor = match existing {
        Some(c) => c,
        None => match resolve_contributor(&ctx.pool, provider, host, &login).await {
            Ok(c) => c,
            Err(e) => return ChatReply::private(format!("❌ Failed to create account: {}", e)),
        },
    };
    match identity::bind(&ctx.pool, actor, &contributor.github_username, false, "link").await {
        Ok(true) => {}
        Ok(false) => {
            audit_link_refused(ctx, actor, &key, "login_taken").await;
            return ChatReply::private(taken);
        }
        Err(e) => return ChatReply::private(format!("❌ Failed to link account: {}", e)),
    }
    if p == ChatPlatform::Discord {
        // Keep the legacy column filled for code that still reads it; never overwrite it.
        if let Ok(d) = actor.user_id.parse::<i64>() {
            let _ = sqlx::query(
                "UPDATE contributors SET discord_user_id = ?, updated_at = CURRENT_TIMESTAMP
                 WHERE github_username = ? AND discord_user_id IS NULL",
            )
            .bind(d)
            .bind(&contributor.github_username)
            .execute(&ctx.pool)
            .await;
        }
    }
    ChatReply::private(format!(
        "✅ Linked your {} account to {} user `{}`!\nThis link is not verified. To receive payouts, ask an administrator to add you to the CRA member list.",
        p.label(),
        provider_label(provider),
        login
    ))
}

// ------------------------------------------------------------------ stats

async fn contributor_row(ctx: &ChatContext, key: &str) -> Result<Option<Contributor>, sqlx::Error> {
    sqlx::query_as::<_, Contributor>("SELECT * FROM contributors WHERE github_username = ?")
        .bind(key)
        .fetch_optional(&ctx.pool)
        .await
}

fn stats_text(c: &Contributor) -> String {
    format!(
        "📊 **Your Contribution Stats**\n\
        👤 GitHub: `{}`\n\
        🏆 Merit Points: `{}`\n\
        📝 Total Contributions: `{}`\n\
        🎭 Current Role: `{}`\n\
        📅 Member Since: `{}`",
        c.github_username,
        c.total_merit_points,
        c.contributions_count,
        c.current_role,
        c.created_at.format("%Y-%m-%d")
    )
}

/// `stats`: the caller's contribution statistics.
pub async fn stats(ctx: &ChatContext, actor: &ChatActor) -> ChatReply {
    let not_linked = format!(
        "❌ No linked GitHub account found! Use `{}` to connect your GitHub account.",
        actor.platform.command(ChatCommand::Link)
    );
    let key = match resolve(ctx, actor, false).await {
        Ok(Caller::Bound(b)) => b.contributor,
        Ok(Caller::Member { contributor: Some(c), .. }) => c,
        Ok(Caller::Member { id, .. }) => {
            return ChatReply::public(format!("📊 The CRA lists you as `{id}`, but no contributions are recorded for your GitHub or GitLab login yet."))
        }
        Ok(Caller::Unknown) if ctx.cra.active().is_some() => {
            return ChatReply::public(format!(
                "❌ This {} account is not in the CRA member list. Ask an administrator to add it.",
                actor.platform.label()
            ))
        }
        Ok(Caller::Unknown) => return ChatReply::public(not_linked),
        Err(e) => return ChatReply::public(format!("❌ Database error: {}", e)),
    };
    match contributor_row(ctx, &key).await {
        Ok(Some(c)) => ChatReply::public(stats_text(&c)),
        Ok(None) => ChatReply::public(not_linked),
        Err(e) => ChatReply::public(format!("❌ Database error: {}", e)),
    }
}

// ------------------------------------------------------------ leaderboard

/// `leaderboard`: the top ten contributors.
pub async fn leaderboard(ctx: &ChatContext, _actor: &ChatActor) -> ChatReply {
    let top = sqlx::query_as::<_, Contributor>("SELECT * FROM contributors ORDER BY total_merit_points DESC LIMIT 10")
        .fetch_all(&ctx.pool)
        .await;
    let text = match top {
        Ok(contributors) if contributors.is_empty() => "📊 No contributors yet! Be the first to contribute!".to_string(),
        Ok(contributors) => {
            let mut board = "🏆 **Top Contributors**\n\n".to_string();
            for (rank, c) in contributors.iter().enumerate() {
                let medal = match rank {
                    0 => "🥇",
                    1 => "🥈",
                    2 => "🥉",
                    _ => "🏅",
                };
                board.push_str(&format!(
                    "{} **#{}** `{}` - {} points ({} contributions)\n",
                    medal,
                    rank + 1,
                    c.github_username,
                    c.total_merit_points,
                    c.contributions_count
                ));
            }
            board
        }
        Err(e) => format!("❌ Failed to fetch leaderboard: {}", e),
    };
    ChatReply::public(text)
}

// ------------------------------------------------------------ admin gate

/// The rank an admin-only command needs with a CRA active: `grants.two_person_min_rank`, the
/// rank at which the policy already treats a role as one that manages others.
pub fn admin_rank_threshold(snap: &Snapshot) -> u32 {
    snap.policy.grants_two_person_min_rank
}

/// May this caller run an admin-only command? With a CRA active: their CRA rank must be at
/// or above [`admin_rank_threshold`] (the platform's admin flag does not count). With no CRA:
/// the platform must report them as an admin. Anything unknown is a refusal.
pub fn admin_check(ctx: &ChatContext, actor: &ChatActor) -> Result<(), String> {
    if let Some(active) = ctx.cra.active() {
        let snap = &active.snapshot;
        let need = admin_rank_threshold(snap);
        let Some(m) = cra_member(snap, actor) else {
            return Err(format!(
                "❌ This command needs CRA rank {need} or above, and this {} account is not in the CRA member list.",
                actor.platform.label()
            ));
        };
        return match ctx.directory.rank_of(&m.id) {
            Some(r) if r >= i64::from(need) => Ok(()),
            Some(r) => Err(format!("❌ This command needs CRA rank {need} or above; yours is {r}.")),
            None => Err(format!("❌ This command needs CRA rank {need} or above, and your rank could not be read.")),
        };
    }
    match actor.is_platform_admin {
        Some(true) => Ok(()),
        Some(false) => Err(format!("❌ You need {} administrator permissions to run this command!", actor.platform.label())),
        None => Err(format!(
            "❌ Could not confirm your {} administrator permissions, so this command is refused.",
            actor.platform.label()
        )),
    }
}

// ------------------------------------------------------------------- sync

/// The CRA projection for one platform.
pub fn projection_plan(snap: &Snapshot, platform: ChatPlatform, merits: &BTreeMap<String, u64>, today: chrono::NaiveDate) -> ProjectionPlan {
    let users = snap
        .members
        .members
        .iter()
        .filter_map(|m| platform.identity_of(&m.identities).map(|s| s.to_string()))
        .collect();
    match platform {
        ChatPlatform::Discord => {
            let p = discord_projection(snap, merits, today);
            ProjectionPlan { managed: p.role_order.into_iter().map(|r| r.discord_role_id).collect(), assignments: p.assignments, users }
        }
        ChatPlatform::Slack => {
            let p = slack_projection(snap, merits, today);
            ProjectionPlan { managed: p.usergroup_order.into_iter().map(|g| g.usergroup_id).collect(), assignments: p.assignments, users }
        }
    }
}

/// `sync`: apply the projection for the caller's platform. With a CRA: the CRA's job titles
/// (Discord roles or Slack user groups). With no CRA: the pre-CRA merit roles (Discord).
pub async fn sync(ctx: &ChatContext, actor: &ChatActor, applier: &dyn ProjectionApplier) -> ChatReply {
    if let Err(denied) = admin_check(ctx, actor) {
        return ChatReply::private(denied);
    }
    let report = match ctx.cra.active() {
        Some(active) => {
            let plan = projection_plan(&active.snapshot, actor.platform, &cached_merits(&ctx.cra), ctx.today());
            applier.apply(&plan).await
        }
        None => applier.apply_legacy().await,
    };
    let mut text = format!(
        "✅ Role sync complete!\n\
        ✓ Successfully synced: {}\n\
        ✗ Failed: {}",
        report.synced, report.failed
    );
    for n in &report.notes {
        text.push('\n');
        text.push_str(n);
    }
    ChatReply::public(text)
}

// ---------------------------------------------------------------- payouts

/// `payouts`: contribution payouts pending approval (admin-only). Approval itself is
/// API-only (admin token).
pub async fn payouts(ctx: &ChatContext, actor: &ChatActor) -> ChatReply {
    if let Err(denied) = admin_check(ctx, actor) {
        return ChatReply::private(denied);
    }
    let text = match crate::payments::ledger::list(&ctx.pool, Some(crate::payments::ledger::PENDING)).await {
        Ok(entries) if entries.is_empty() => "No payouts pending approval.".to_string(),
        Ok(entries) => {
            let total: i64 = entries.iter().map(|e| e.amount_cents).sum();
            let mut out = format!("**{} payout(s) pending approval** (total {:.2})\n", entries.len(), total as f64 / 100.0);
            for e in entries.iter().take(15) {
                out.push_str(&format!(
                    "- PR #{} `{}` -> `{}`: {:.2} {} (id `{}`)\n",
                    e.pr_number,
                    e.github_username,
                    e.branch,
                    e.amount_cents as f64 / 100.0,
                    e.currency.to_uppercase(),
                    e.id
                ));
            }
            if entries.len() > 15 {
                out.push_str(&format!("...and {} more\n", entries.len() - 15));
            }
            out.push_str("Approve or reject via the admin API.");
            out
        }
        Err(e) => format!("Failed to load payouts: {}", e),
    };
    ChatReply::private(text)
}

// ----------------------------------------------------------- payout-setup

/// The refusal for an unverified binding.
pub fn unverified_payout_text(p: ChatPlatform) -> String {
    format!(
        "❌ Your {} account is linked, but the link is not verified, so it cannot set up payouts. \
         Ask an administrator to add you to the CRA member list (`members.toml`) with your GitHub or GitLab login and your {} id.",
        p.label(),
        p.label()
    )
}

/// `payout-setup`: the caller's private Stripe onboarding link. Only for a verified caller:
/// a CRA-attested member, or a binding proven by OAuth sign-in (the chat account and the
/// GitHub or GitLab login both proven for one MSync account).
pub async fn payout_setup(ctx: &ChatContext, actor: &ChatActor) -> ChatReply {
    use crate::payments::onboarding;
    let p = actor.platform;
    let key = match resolve(ctx, actor, true).await {
        Ok(Caller::Unknown) if ctx.cra.active().is_some() => {
            return ChatReply::private(format!(
                "This {} account is not in the CRA member list. Ask an administrator to add you to the CRA member list (`members.toml`), then run `{}` again.",
                p.label(),
                p.command(ChatCommand::PayoutSetup)
            ))
        }
        Ok(Caller::Unknown) => {
            return ChatReply::private(format!(
                "Link your GitHub account first with `{} github:<username>`, then run `{}` again.",
                p.command(ChatCommand::Link),
                p.command(ChatCommand::PayoutSetup)
            ))
        }
        Ok(Caller::Bound(b))
            if !b.verified
                && (crate::accounts::mutant::on("payout_ignores_oauth")
                    || !crate::accounts::chat_binding_is_verified(&ctx.pool, p, actor.storage_team(), &actor.user_id, &b.contributor)
                        .await
                        .unwrap_or(false)) =>
        {
            return ChatReply::private(unverified_payout_text(p))
        }
        Ok(Caller::Bound(b)) => b.contributor,
        Ok(Caller::Member { status, .. }) if status != MemberStatus::Active => {
            return ChatReply::private(format!("Your CRA membership is {}, so payouts cannot be set up.", status.as_str()))
        }
        Ok(Caller::Member { contributor: Some(c), .. }) => c,
        Ok(Caller::Member { .. }) => {
            return ChatReply::private(
                "The CRA lists you, but with no GitHub or GitLab login, so there is nothing to pay. Ask an administrator to add your login to `members.toml`.",
            )
        }
        Err(e) => return ChatReply::private(format!("Failed to look up your account: {}", e)),
    };
    let enabled: Result<Option<i64>, sqlx::Error> =
        sqlx::query_scalar("SELECT stripe_payouts_enabled FROM contributors WHERE github_username = ?")
            .bind(&key)
            .fetch_optional(&ctx.pool)
            .await;
    match enabled {
        Ok(Some(e)) if e != 0 => return ChatReply::private("Your payout account is already set up and verified. Nothing else to do."),
        Ok(_) => {}
        Err(e) => return ChatReply::private(format!("Failed to look up your account: {}", e)),
    }
    match onboarding::onboarding_link_for_verified_login(&ctx.pool, ctx.stripe.as_ref(), &ctx.payments, &key).await {
        Ok(link) => ChatReply::private(format!(
            "Set up payouts for `{}` here (private link, expires in a few minutes):\n{}",
            key, link.url
        )),
        Err(e) => {
            tracing::error!("payout-setup failed for {}: {}", key, e);
            ChatReply::private("Couldn't create your payout setup link right now. Please try again later or ask a maintainer.")
        }
    }
}

// --------------------------------------------------------- clock-in, shift

/// The chat account as an audit actor (`discord:<id>` or `slack:<team>:<id>`).
pub fn chat_actor_id(actor: &ChatActor) -> String {
    match actor.platform {
        ChatPlatform::Discord => format!("discord:{}", actor.user_id),
        ChatPlatform::Slack => format!("slack:{}:{}", actor.team_id, actor.user_id),
    }
}

/// The payee behind a chat account, only through a verified identity: a CRA listing (an
/// active member), or a binding proven by an OAuth sign-in (`accounts`). An unverified
/// binding made with `link` is refused, so typing someone else's login can never clock in as
/// them. Err is the private reply.
pub async fn verified_payee(ctx: &ChatContext, actor: &ChatActor) -> Result<crate::payments::payees::Payee, String> {
    use crate::payments::payees;
    let p = actor.platform;
    let db = |e: sqlx::Error| format!("❌ Database error: {e}");
    let (payee, who) = match resolve(ctx, actor, false).await.map_err(db)? {
        Caller::Unknown if ctx.cra.active().is_some() => {
            return Err(format!("❌ This {} account is not in the CRA member list, so it cannot clock in. Ask an administrator to add it.", p.label()))
        }
        Caller::Unknown => {
            return Err(format!(
                "❌ This {} account is not linked to a verified identity, so it cannot clock in. Ask an administrator to add you to the CRA member list.",
                p.label()
            ))
        }
        Caller::Member { status, .. } if status != MemberStatus::Active => {
            return Err(format!("❌ Your CRA membership is {}, so you cannot clock in.", status.as_str()))
        }
        Caller::Member { id, contributor, .. } => {
            let own: Option<String> = sqlx::query_scalar("SELECT id FROM payees WHERE member_id = ? AND status <> 'closed' ORDER BY id LIMIT 1")
                .bind(&id)
                .fetch_optional(&ctx.pool)
                .await
                .map_err(db)?;
            let mut found = match own {
                Some(pid) => payees::load(&ctx.pool, &pid).await.map_err(db)?,
                None => None,
            };
            if found.is_none() {
                if let Some(c) = contributor {
                    found = payees::by_key(&ctx.pool, &c.to_lowercase()).await.map_err(db)?;
                }
            }
            if found.is_none() {
                if let Some(active) = ctx.cra.active() {
                    for (prov, host, login) in member_foundry_logins(&active.snapshot, &id) {
                        if let Some(x) = payees::by_key(&ctx.pool, &legacy_username(prov, &host, &login).to_lowercase()).await.map_err(db)? {
                            found = Some(x);
                            break;
                        }
                    }
                }
            }
            (found, id)
        }
        Caller::Bound(b) => {
            let proven = b.verified
                || crate::accounts::chat_binding_is_verified(&ctx.pool, p, actor.storage_team(), &actor.user_id, &b.contributor)
                    .await
                    .unwrap_or(false);
            if !proven && !crate::payments::mutant::on("chat_unverified_clocks") {
                audit_clock_in_refused(ctx, actor, "unverified_binding").await;
                return Err(format!(
                    "❌ Your {} account is linked, but the link is not verified, so it cannot clock in. Ask an administrator to add you to the CRA member list, or sign in to MSync with both accounts.",
                    p.label()
                ));
            }
            (payees::by_key(&ctx.pool, &b.contributor.to_lowercase()).await.map_err(db)?, b.contributor)
        }
    };
    payee.ok_or_else(|| format!("❌ `{who}` has no payee record, so there is no time to clock. Ask an administrator."))
}

async fn audit_clock_in_refused(ctx: &ChatContext, actor: &ChatActor, reason: &str) {
    crate::audit::append_best_effort(
        &ctx.pool,
        &crate::audit::AuditEntry::new(
            crate::audit::ActorKind::Chat,
            chat_actor_id(actor),
            "time.clock_in.refused",
            format!("chat:{}", chat_actor_id(actor)),
            serde_json::json!({ "platform": actor.platform.as_str(), "team_or_guild": actor.storage_team(), "user_id": actor.user_id, "reason": reason }),
        ),
    )
    .await;
}

fn local_text(t: &str, tz: &str) -> String {
    match (crate::auth::tokens::parse_db_time(t), crate::payments::shifts::parse_tz(tz)) {
        (Some(t), Ok(z)) => t.with_timezone(&z).format("%a %Y-%m-%d %H:%M %Z").to_string(),
        _ => format!("{t} UTC"),
    }
}

/// `clock-in`: clock in for the caller's current shift. The shift sets the end; there is no
/// clock-out in chat. A second clock-in answers with the same entry.
pub async fn clock_in(ctx: &ChatContext, actor: &ChatActor, now: chrono::DateTime<chrono::Utc>) -> ChatReply {
    use crate::payments::timeclock;
    let payee = match verified_payee(ctx, actor).await {
        Ok(p) => p,
        Err(msg) => return ChatReply::private(msg),
    };
    let who = timeclock::Actor { kind: crate::audit::ActorKind::Chat, id: chat_actor_id(actor), token_id: None };
    match timeclock::clock_in_for(&ctx.pool, &payee, &who, now).await {
        Ok(r) => {
            let e = &r.entry.row;
            let head = if r.result == "created" { "✅ Clocked in." } else { "✅ You are already clocked in for this shift. Nothing changed." };
            ChatReply::private(format!(
                "{head}\nPaid from {} to {} ({} minutes). The shift's end ends your time; you do not need to clock out.",
                local_text(&e.start_at, &e.time_zone),
                local_text(&e.end_at, &e.time_zone),
                r.entry.minutes
            ))
        }
        Err(e) => ChatReply::private(format!("❌ Not clocked in: {e}")),
    }
}

/// `shift`: the caller's current or next shift.
pub async fn shift(ctx: &ChatContext, actor: &ChatActor, now: chrono::DateTime<chrono::Utc>) -> ChatReply {
    use crate::payments::shifts;
    let payee = match verified_payee(ctx, actor).await {
        Ok(p) => p,
        Err(msg) => return ChatReply::private(msg),
    };
    let specs = match shifts::active_specs(&ctx.pool, &payee.row.id).await {
        Ok(s) => s,
        Err(e) => return ChatReply::private(format!("❌ Database error: {e}")),
    };
    let open = specs.iter().any(|s| matches!(s.pattern, shifts::Pattern::Open { .. }));
    let Some(o) = shifts::current_or_next(&specs, now) else {
        return ChatReply::private(if open {
            "You have no scheduled shift in the next two weeks. You have an open-shift allowance, so a clock-in starts one.".to_string()
        } else {
            "You have no shift in the next two weeks.".to_string()
        });
    };
    let tz = specs.iter().find(|s| s.id == o.shift_id).map(|s| s.tz).unwrap_or(chrono_tz::UTC);
    let fmt = |t: chrono::DateTime<chrono::Utc>| t.with_timezone(&tz).format("%a %Y-%m-%d %H:%M %Z").to_string();
    let when = if o.window_start <= now { "Your current shift" } else { "Your next shift" };
    ChatReply::private(format!("{when}: {} to {}. You can clock in from {}.", fmt(o.start), fmt(o.end), fmt(o.window_start)))
}
