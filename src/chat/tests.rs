//! Shared chat commands: the `link` fix, the payout-setup gate, admin fail-closed, one
//! person on two platforms, and identical replies on Discord and Slack.
//!
//! Fixture: the Eustress Engine example CRA (Discord and Slack). mckale is Lead Maintainer
//! (rank 80) with Discord `100000000000000102` and Slack `U1000000102` in team
//! `T1000000000`; bo-example is Reviewer (40); the admin threshold
//! (`grants.two_person_min_rank`, source_available) is 50.

use std::sync::{Arc, Mutex};

use anyhow::Result;
use async_trait::async_trait;
use chrono::NaiveDate;
use sqlx::sqlite::{SqlitePool, SqlitePoolOptions};

use super::commands::{self, unverified_payout_text};
use super::identity;
use super::*;
use crate::auth::{CraDirectory, Directory, EnvDirectory};
use crate::cra::loader::{ActiveCra, CraHandle};
use crate::cra::{CraFiles, CraRepo};
use crate::payments::stripe::TransferRequest;

pub(crate) fn today() -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 11, 12).unwrap()
}

pub(crate) async fn pool() -> SqlitePool {
    let pool = SqlitePoolOptions::new().max_connections(1).connect("sqlite::memory:").await.unwrap();
    sqlx::migrate!("./migrations").run(&pool).await.unwrap();
    pool
}

/// Records every Stripe call, so a test can prove none was made.
#[derive(Default)]
pub(crate) struct MockStripe {
    pub accounts_created: Mutex<Vec<String>>,
    pub links_created: Mutex<Vec<String>>,
}

#[async_trait]
impl StripeApi for MockStripe {
    async fn create_express_account(&self, login: &str) -> Result<String> {
        self.accounts_created.lock().unwrap().push(login.to_string());
        Ok(format!("acct_{}", login.to_ascii_lowercase()))
    }
    async fn create_account_link(&self, a: &str, _r: &str, _ret: &str) -> Result<String> {
        self.links_created.lock().unwrap().push(a.to_string());
        Ok(format!("https://connect.stripe.test/{a}"))
    }
    async fn create_transfer(&self, _req: &TransferRequest) -> Result<String> {
        anyhow::bail!("no transfers in chat tests")
    }
}

impl MockStripe {
    pub fn calls(&self) -> usize {
        self.accounts_created.lock().unwrap().len() + self.links_created.lock().unwrap().len()
    }
}

pub(crate) fn eustress_files() -> CraFiles {
    CraFiles {
        org: include_str!("../../examples/cra/eustress-engine/.msync/org.toml").into(),
        roles: include_str!("../../examples/cra/eustress-engine/.msync/roles.toml").into(),
        members: include_str!("../../examples/cra/eustress-engine/.msync/members.toml").into(),
        merit: include_str!("../../examples/cra/eustress-engine/.msync/merit.toml").into(),
        payouts: include_str!("../../examples/cra/eustress-engine/.msync/payouts.toml").into(),
    }
}

pub(crate) fn activate(handle: &CraHandle, files: &CraFiles) {
    let snapshot = crate::cra::load(files, today()).unwrap_or_else(|e| panic!("{e:?}"));
    handle.replace(Some(Arc::new(ActiveCra {
        snapshot_id: 1,
        commit_sha: "c0ffee".into(),
        loaded_at: "2026-11-12T00:00:00Z".into(),
        actor: Some("github:example-founder".into()),
        repo: CraRepo::new(crate::foundry::FoundryProvider::GitHub, "github.com", "example-eustress/cra"),
        snapshot,
    })));
}

pub(crate) struct Env {
    pub ctx: Arc<ChatContext>,
    pub stripe: Arc<MockStripe>,
}

pub(crate) async fn env_with(cra: bool) -> Env {
    let pool = pool().await;
    let handle = CraHandle::new();
    if cra {
        activate(&handle, &eustress_files());
    }
    let dir: Arc<dyn Directory> = Arc::new(
        CraDirectory::new(EnvDirectory::new(vec!["github:example-founder".to_string()]), handle.clone()).with_today(today()),
    );
    env_from(pool, handle, dir)
}

pub(crate) fn env_from(pool: SqlitePool, handle: CraHandle, directory: Arc<dyn Directory>) -> Env {
    let stripe = Arc::new(MockStripe::default());
    let mut payments = crate::payments::PaymentConfig::from_lookup(|_| None);
    payments.public_url = "https://msync.example".into();
    payments.admin_token = Some("admin-secret".into());
    let ctx = Arc::new(ChatContext {
        pool,
        payments: Arc::new(payments),
        stripe: stripe.clone(),
        directory,
        cra: handle,
        today: Some(today()),
    });
    Env { ctx, stripe }
}

/// Records what `sync` asked the platform to do.
#[derive(Default)]
pub(crate) struct RecordingApplier {
    pub plans: Mutex<Vec<ProjectionPlan>>,
    pub legacy: Mutex<u32>,
}

#[async_trait]
impl ProjectionApplier for RecordingApplier {
    async fn apply(&self, plan: &ProjectionPlan) -> SyncReport {
        self.plans.lock().unwrap().push(plan.clone());
        SyncReport { synced: plan.users.len() as u32, ..Default::default() }
    }
    async fn apply_legacy(&self) -> SyncReport {
        *self.legacy.lock().unwrap() += 1;
        SyncReport::default()
    }
}

impl RecordingApplier {
    fn calls(&self) -> usize {
        self.plans.lock().unwrap().len() + *self.legacy.lock().unwrap() as usize
    }
}

const ALICE_D: &str = "111111111111111111";
const MALLORY_D: &str = "666666666666666666";
const TEAM: &str = "T1000000000";

fn d(user: &str) -> ChatActor {
    ChatActor::discord(user, None)
}
fn s(user: &str) -> ChatActor {
    ChatActor::slack(TEAM, user, None)
}

async fn bound(ctx: &ChatContext, actor: &ChatActor) -> Option<String> {
    identity::binding_for(&ctx.pool, actor).await.unwrap().map(|b| b.contributor)
}

async fn count(pool: &SqlitePool, sql: &str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(pool).await.unwrap()
}

// ------------------------------------------------------------------- link

#[tokio::test]
async fn link_refuses_a_login_bound_to_another_user_on_both_platforms() {
    let e = env_with(false).await;
    for (alice, mallory) in [(d(ALICE_D), d(MALLORY_D)), (s("U0ALICE0001"), s("U0MALLORY01"))] {
        let r = commands::link(&e.ctx, &alice, "alice").await;
        assert!(r.text.contains("Linked your"), "{}", r.text);
        let r = commands::link(&e.ctx, &mallory, "alice").await;
        assert!(r.text.contains("already linked to a different"), "{:?}: {}", mallory.platform, r.text);
        // Case and the github: prefix do not get around it.
        let r = commands::link(&e.ctx, &mallory, "github:ALICE").await;
        assert!(r.text.contains("already linked to a different"), "{}", r.text);
        assert_eq!(bound(&e.ctx, &mallory).await, None);
        assert_eq!(bound(&e.ctx, &alice).await.as_deref(), Some("alice"));
    }
}

#[tokio::test]
async fn link_refuses_to_change_an_existing_binding() {
    let e = env_with(false).await;
    for alice in [d(ALICE_D), s("U0ALICE0001")] {
        commands::link(&e.ctx, &alice, "alice").await;
        let r = commands::link(&e.ctx, &alice, "bob").await;
        assert!(r.text.contains("cannot be changed from chat"), "{}", r.text);
        assert_eq!(bound(&e.ctx, &alice).await.as_deref(), Some("alice"));
        // Re-linking the same login is a no-op, not an error.
        let r = commands::link(&e.ctx, &alice, "Alice").await;
        assert!(r.text.contains("already linked to `alice`"), "{}", r.text);
    }
    assert_eq!(count(&e.ctx.pool, "SELECT COUNT(*) FROM chat_identities ci JOIN contributors c ON c.id = ci.contributor_id WHERE c.github_username = 'bob'").await, 0);
}

#[tokio::test]
async fn link_marks_bindings_unverified_and_fills_the_legacy_discord_column() {
    let e = env_with(false).await;
    commands::link(&e.ctx, &d(ALICE_D), "alice").await;
    let b = identity::binding_for(&e.ctx.pool, &d(ALICE_D)).await.unwrap().unwrap();
    assert!(!b.verified);
    let legacy: Option<i64> = sqlx::query_scalar("SELECT discord_user_id FROM contributors WHERE github_username = 'alice'")
        .fetch_one(&e.ctx.pool)
        .await
        .unwrap();
    assert_eq!(legacy, Some(ALICE_D.parse().unwrap()));
}

#[tokio::test]
async fn link_with_a_cra_binds_nothing_and_reports_the_cra() {
    let e = env_with(true).await;
    // A stranger trying to claim McKale's login gets nothing.
    let r = commands::link(&e.ctx, &d(MALLORY_D), "Simbuilder").await;
    assert!(r.private);
    assert!(r.text.contains("not in the member list"), "{}", r.text);
    // A member is told what the CRA says, and nothing is bound either.
    let r = commands::link(&e.ctx, &d("100000000000000102"), "someone-else").await;
    assert!(r.text.contains("`mckale`") && r.text.contains("GitHub `Simbuilder`"), "{}", r.text);
    let r = commands::link(&e.ctx, &s("U1000000102"), "").await;
    assert!(r.text.contains("`mckale`") && r.text.contains("Slack `U1000000102`"), "{}", r.text);
    assert_eq!(count(&e.ctx.pool, "SELECT COUNT(*) FROM chat_identities").await, 0);
    assert_eq!(count(&e.ctx.pool, "SELECT COUNT(*) FROM contributors").await, 0);
}

// ------------------------------------------------------------ payout-setup

#[tokio::test]
async fn story_mallory_links_alices_login_and_cannot_take_her_payouts() {
    let e = env_with(false).await;
    // Alice linked her Discord account to her GitHub login and has a payout account.
    commands::link(&e.ctx, &d(ALICE_D), "alice").await;
    sqlx::query("UPDATE contributors SET stripe_account_id = 'acct_alice_real' WHERE github_username = 'alice'")
        .execute(&e.ctx.pool)
        .await
        .unwrap();

    // Mallory types Alice's login.
    let r = commands::link(&e.ctx, &d(MALLORY_D), "alice").await;
    assert!(r.text.starts_with("❌"), "{}", r.text);
    // Mallory asks for a payout link.
    let r = commands::payout_setup(&e.ctx, &d(MALLORY_D)).await;
    assert!(r.private && !r.text.contains("connect.stripe"), "{}", r.text);

    // No Stripe account and no onboarding link for Mallory, and Alice is untouched.
    assert_eq!(e.stripe.calls(), 0, "Stripe was called for Mallory");
    assert_eq!(bound(&e.ctx, &d(ALICE_D)).await.as_deref(), Some("alice"));
    assert_eq!(bound(&e.ctx, &d(MALLORY_D)).await, None);
    let (discord, acct): (Option<i64>, Option<String>) =
        sqlx::query_as("SELECT discord_user_id, stripe_account_id FROM contributors WHERE github_username = 'alice'")
            .fetch_one(&e.ctx.pool)
            .await
            .unwrap();
    assert_eq!(discord, Some(ALICE_D.parse().unwrap()));
    assert_eq!(acct.as_deref(), Some("acct_alice_real"));
}

#[tokio::test]
async fn story_mallory_links_a_login_nobody_linked_yet_and_still_gets_no_payout_account() {
    let e = env_with(false).await;
    let r = commands::link(&e.ctx, &d(MALLORY_D), "alice").await;
    assert!(r.text.contains("not verified"), "{}", r.text);
    let r = commands::payout_setup(&e.ctx, &d(MALLORY_D)).await;
    assert_eq!(r.text, unverified_payout_text(ChatPlatform::Discord));
    assert_eq!(e.stripe.calls(), 0);
}

#[tokio::test]
async fn payout_setup_refuses_every_unverified_binding_including_alices_own() {
    let e = env_with(false).await;
    for alice in [d(ALICE_D), s("U0ALICE0001")] {
        commands::link(&e.ctx, &alice, "alice").await;
        let r = commands::payout_setup(&e.ctx, &alice).await;
        assert!(r.private);
        assert_eq!(r.text, unverified_payout_text(alice.platform));
        assert!(r.text.contains("CRA member list"), "{}", r.text);
    }
    assert_eq!(e.stripe.calls(), 0);
}

#[tokio::test]
async fn bindings_from_before_this_change_are_unverified() {
    // Run every migration except the chat one, write a binding the old way, then migrate.
    let pool = SqlitePoolOptions::new().max_connections(1).connect("sqlite::memory:").await.unwrap();
    let all = sqlx::migrate!("./migrations");
    // Every migration before the chat one (later migrations, such as accounts, come after it).
    let chat = all.migrations.iter().position(|m| m.description.contains("chat platforms")).expect("the chat migration exists");
    let mut before = sqlx::migrate!("./migrations");
    before.migrations = std::borrow::Cow::Owned(all.migrations[..chat].to_vec());
    before.run(&pool).await.unwrap();
    sqlx::query("INSERT INTO contributors (id, github_username, discord_user_id, created_at, updated_at) VALUES (?, 'alice', 42, datetime('now'), datetime('now'))")
        .bind(uuid::Uuid::new_v4())
        .execute(&pool)
        .await
        .unwrap();
    all.run(&pool).await.unwrap();

    let handle = CraHandle::new();
    let e = env_from(pool, handle.clone(), Arc::new(CraDirectory::new(EnvDirectory::default(), handle)));
    let b = identity::binding_for(&e.ctx.pool, &d("42")).await.unwrap().unwrap();
    assert_eq!(b, identity::Binding { contributor: "alice".into(), verified: false });
    let r = commands::payout_setup(&e.ctx, &d("42")).await;
    assert_eq!(r.text, unverified_payout_text(ChatPlatform::Discord));
    assert_eq!(e.stripe.calls(), 0);
    // The backfilled binding is still a binding: another user cannot claim alice.
    let r = commands::link(&e.ctx, &d(MALLORY_D), "alice").await;
    assert!(r.text.contains("already linked to a different"), "{}", r.text);
}

#[tokio::test]
async fn payout_setup_accepts_a_cra_attested_member_on_both_platforms() {
    let e = env_with(true).await;
    let r = commands::payout_setup(&e.ctx, &d("100000000000000102")).await;
    assert!(r.private);
    assert!(r.text.contains("https://connect.stripe.test/acct_simbuilder"), "{}", r.text);
    // Slack reaches the same contributor and the same account; no second account.
    let r = commands::payout_setup(&e.ctx, &s("U1000000102")).await;
    assert!(r.text.contains("https://connect.stripe.test/acct_simbuilder"), "{}", r.text);
    assert_eq!(*e.stripe.accounts_created.lock().unwrap(), vec!["Simbuilder".to_string()]);
    // A Slack user from another team is not McKale, even with the same user id.
    let other = ChatActor::slack("T9999999999", "U1000000102", None);
    let r = commands::payout_setup(&e.ctx, &other).await;
    assert!(r.text.contains("not in the CRA member list"), "{}", r.text);
    // Someone not in the CRA gets nothing.
    let r = commands::payout_setup(&e.ctx, &d(MALLORY_D)).await;
    assert!(r.text.contains("not in the CRA member list"), "{}", r.text);
    assert_eq!(e.stripe.accounts_created.lock().unwrap().len(), 1);
}

// -------------------------------------------------------------- admin gate

#[tokio::test]
async fn admin_commands_fail_closed_without_a_cra_when_the_admin_flag_is_unknown() {
    let e = env_with(false).await;
    for platform_actor in [d(ALICE_D), s("U0ALICE0001")] {
        let applier = RecordingApplier::default();
        let r = commands::sync(&e.ctx, &platform_actor, &applier).await;
        assert!(r.text.contains("Could not confirm"), "{}", r.text);
        let r = commands::payouts(&e.ctx, &platform_actor).await;
        assert!(r.text.contains("Could not confirm"), "{}", r.text);
        let not_admin = ChatActor { is_platform_admin: Some(false), ..platform_actor.clone() };
        let r = commands::sync(&e.ctx, &not_admin, &applier).await;
        assert!(r.text.contains("administrator permissions"), "{}", r.text);
        assert_eq!(applier.calls(), 0, "sync ran for a caller who is not an admin");

        let admin = ChatActor { is_platform_admin: Some(true), ..platform_actor.clone() };
        let r = commands::sync(&e.ctx, &admin, &applier).await;
        assert!(r.text.contains("Role sync complete"), "{}", r.text);
        assert_eq!(*applier.legacy.lock().unwrap(), 1, "no CRA: the legacy sync runs");
        let r = commands::payouts(&e.ctx, &admin).await;
        assert_eq!(r.text, "No payouts pending approval.");
    }
}

/// A directory that can read no rank at all.
struct NoRanks;
impl Directory for NoRanks {
    fn rank_of(&self, _member_id: &str) -> Option<i64> {
        None
    }
}

#[tokio::test]
async fn admin_commands_with_a_cra_need_rank_and_fail_closed_when_it_is_unknown() {
    let e = env_with(true).await;
    for (mckale, bo, stranger) in [
        (d("100000000000000102"), d("100000000000000104"), d(MALLORY_D)),
        (s("U1000000102"), s("U1000000104"), s("U0MALLORY01")),
    ] {
        let applier = RecordingApplier::default();
        // The platform admin flag does not count while a CRA is active.
        let stranger_admin = ChatActor { is_platform_admin: Some(true), ..stranger };
        let r = commands::sync(&e.ctx, &stranger_admin, &applier).await;
        assert!(r.text.contains("not in the CRA member list"), "{}", r.text);
        let bo_admin = ChatActor { is_platform_admin: Some(true), ..bo };
        let r = commands::sync(&e.ctx, &bo_admin, &applier).await;
        assert!(r.text.contains("needs CRA rank 50 or above; yours is 40"), "{}", r.text);
        let r = commands::payouts(&e.ctx, &bo_admin).await;
        assert!(r.text.contains("yours is 40"), "{}", r.text);
        assert_eq!(applier.calls(), 0);

        let r = commands::sync(&e.ctx, &mckale, &applier).await;
        assert!(r.text.contains("Role sync complete"), "{}", r.text);
        assert_eq!(applier.plans.lock().unwrap().len(), 1);
    }

    // The same member with no readable rank is refused.
    let e2 = env_from(e.ctx.pool.clone(), e.ctx.cra.clone(), Arc::new(NoRanks));
    let applier = RecordingApplier::default();
    for mckale in [d("100000000000000102"), s("U1000000102")] {
        let r = commands::sync(&e2.ctx, &ChatActor { is_platform_admin: Some(true), ..mckale }, &applier).await;
        assert!(r.text.contains("could not be read"), "{}", r.text);
    }
    assert_eq!(applier.calls(), 0);
}

#[tokio::test]
async fn sync_hands_each_platform_its_own_cra_projection() {
    let e = env_with(true).await;
    let applier = RecordingApplier::default();
    commands::sync(&e.ctx, &d("100000000000000102"), &applier).await;
    commands::sync(&e.ctx, &s("U1000000102"), &applier).await;
    let plans = applier.plans.lock().unwrap();
    let (discord, slack) = (&plans[0], &plans[1]);
    // Discord: role ids and Discord user ids.
    assert!(discord.assignments.contains(&("100000000000000102".into(), "100000000000000080".into())));
    assert!(discord.managed.iter().all(|r| r.starts_with("1000000000000000")));
    // Slack: user group ids and Slack user ids, the full membership of each group.
    let groups = slack.by_group();
    assert_eq!(groups.get("S1000000080"), Some(&vec!["U1000000102".to_string()]));
    assert_eq!(groups.get("S1000000060"), Some(&vec!["U1000000103".to_string()]));
    assert_eq!(slack.managed.first().map(String::as_str), Some("S1000000080"), "highest rank first");
    assert!(slack.users.contains("U1000000101") && !slack.users.iter().any(|u| u.starts_with('1')));
}

// ----------------------------------------------------- one person, two apps

#[tokio::test]
async fn one_contributor_reached_from_discord_and_from_slack_is_the_same_row() {
    let e = env_with(false).await;
    commands::link(&e.ctx, &d(ALICE_D), "alice").await;
    commands::link(&e.ctx, &s("U0ALICE0001"), "alice").await;
    assert_eq!(bound(&e.ctx, &d(ALICE_D)).await.as_deref(), Some("alice"));
    assert_eq!(bound(&e.ctx, &s("U0ALICE0001")).await.as_deref(), Some("alice"));
    let pool = &e.ctx.pool;
    assert_eq!(count(pool, "SELECT COUNT(*) FROM contributors").await, 1);
    assert_eq!(count(pool, "SELECT COUNT(*) FROM chat_identities").await, 2);
    assert_eq!(count(pool, "SELECT COUNT(DISTINCT contributor_id) FROM chat_identities").await, 1);
    assert_eq!(
        count(pool, "SELECT COUNT(*) FROM chat_identities ci JOIN contributors c ON c.id = ci.contributor_id").await,
        2,
        "both rows point at the contributor row as stored"
    );
}

#[tokio::test]
async fn the_same_command_gives_the_same_reply_on_discord_and_slack() {
    // Without a CRA, through chat bindings.
    let e = env_with(false).await;
    commands::link(&e.ctx, &d(ALICE_D), "alice").await;
    commands::link(&e.ctx, &s("U0ALICE0001"), "alice").await;
    sqlx::query("UPDATE contributors SET total_merit_points = 120, contributions_count = 4 WHERE github_username = 'alice'")
        .execute(&e.ctx.pool)
        .await
        .unwrap();
    let a = commands::stats(&e.ctx, &d(ALICE_D)).await;
    let b = commands::stats(&e.ctx, &s("U0ALICE0001")).await;
    assert_eq!(a, b);
    assert!(a.text.contains("Merit Points: `120`"), "{}", a.text);
    assert_eq!(commands::leaderboard(&e.ctx, &d(ALICE_D)).await, commands::leaderboard(&e.ctx, &s("U0ALICE0001")).await);

    // With a CRA, through members.toml.
    let e = env_with(true).await;
    crate::foundry::identity::resolve_contributor(&e.ctx.pool, crate::foundry::FoundryProvider::GitHub, "github.com", "Simbuilder")
        .await
        .unwrap();
    let a = commands::stats(&e.ctx, &d("100000000000000102")).await;
    let b = commands::stats(&e.ctx, &s("U1000000102")).await;
    assert_eq!(a, b);
    assert!(a.text.contains("GitHub: `Simbuilder`"), "{}", a.text);
    let a = commands::payout_setup(&e.ctx, &d("100000000000000102")).await;
    let b = commands::payout_setup(&e.ctx, &s("U1000000102")).await;
    assert_eq!(a, b);
    assert!(a.private);
}

#[test]
fn command_names_and_platform_spellings() {
    assert_eq!(ChatPlatform::Discord.command(ChatCommand::PayoutSetup), "/payout-setup");
    assert_eq!(ChatPlatform::Slack.command(ChatCommand::PayoutSetup), "/msync-payout-setup");
    for c in ChatCommand::ALL {
        assert_eq!(ChatCommand::from_name(c.name()), Some(c));
    }
    assert_eq!(commands::parse_login("gitlab:Ada").unwrap(), (crate::foundry::FoundryProvider::GitLab, "Ada".into()));
    assert!(commands::parse_login("bitbucket:x").is_err());
    assert!(commands::parse_login("a b").is_err());
    assert!(commands::parse_login("  ").is_err());
}
