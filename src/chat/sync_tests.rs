//! Role sync against an in-memory fake platform (contract 13.3).
//!
//! Fixture: the Eustress Engine example CRA with merit 0, so only positions project:
//! mckale holds Lead Maintainer (role `..080`, grant expires 2027-05-01), ada Maintainer
//! (`..060`, expires 2027-04-30), bo Reviewer (`..040`, no expiry). founder holds nothing.
//!
//! Mutations: `MSYNC_SYNC_MUTANT=<name>` switches one guard off (`super::mutant`), so each
//! test here can be shown to fail. Unset, every guard is on.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use sqlx::SqlitePool;

use super::*;
use crate::chat::tests::{activate, eustress_files, pool, today};
use crate::chat::ChatPlatform;
use crate::cra::loader::CraHandle;

// ------------------------------------------------------------------ the fake

pub(crate) struct FakePlatform {
    pub platform: ChatPlatform,
    pub configured: bool,
    /// Every role on the platform, managed or not.
    pub roles: Mutex<BTreeMap<String, BTreeSet<String>>>,
    pub safety: Mutex<BTreeMap<String, RoleSafety>>,
    /// Roles whose holders were read, in order.
    pub reads: Mutex<Vec<String>>,
    /// Roles whose safety was asked.
    pub safety_asks: Mutex<Vec<String>>,
    /// Every write the platform accepted or refused: (op, role, user).
    pub writes: Mutex<Vec<(String, String, String)>>,
    pub alerts: Mutex<Vec<String>>,
    /// Failures for the next writes, in order.
    pub fail_writes: Mutex<VecDeque<PlatformError>>,
    pub read_delay: Option<Duration>,
    in_flight: AtomicUsize,
    pub max_in_flight: AtomicUsize,
}

impl FakePlatform {
    pub fn new(platform: ChatPlatform, managed: &[&str]) -> Self {
        let mut roles: BTreeMap<String, BTreeSet<String>> = managed.iter().map(|r| (r.to_string(), BTreeSet::new())).collect();
        // A role the CRA does not map, with holders inside and outside the CRA.
        roles.insert(unmanaged(platform).into(), [ids(platform).founder.to_string(), "OUTSIDER".to_string()].into());
        FakePlatform {
            platform,
            configured: true,
            roles: Mutex::new(roles),
            safety: Mutex::new(BTreeMap::new()),
            reads: Mutex::default(),
            safety_asks: Mutex::default(),
            writes: Mutex::default(),
            alerts: Mutex::default(),
            fail_writes: Mutex::default(),
            read_delay: None,
            in_flight: AtomicUsize::new(0),
            max_in_flight: AtomicUsize::new(0),
        }
    }

    pub fn holds(&self, role: &str, user: &str) -> bool {
        self.roles.lock().unwrap().get(role).map_or(false, |s| s.contains(user))
    }

    /// Someone with platform admin rights edits a role by hand.
    pub fn hand_grant(&self, role: &str, user: &str) {
        self.roles.lock().unwrap().entry(role.into()).or_default().insert(user.into());
    }

    pub fn hand_remove(&self, role: &str, user: &str) {
        self.roles.lock().unwrap().entry(role.into()).or_default().remove(user);
    }

    pub fn write_count(&self) -> usize {
        self.writes.lock().unwrap().len()
    }

    fn write(&self, op: &str, role: &str, user: &str) -> Result<(), PlatformError> {
        self.writes.lock().unwrap().push((op.into(), role.into(), user.into()));
        if let Some(e) = self.fail_writes.lock().unwrap().pop_front() {
            return Err(e);
        }
        let mut roles = self.roles.lock().unwrap();
        let set = roles.entry(role.into()).or_default();
        if op == "add" {
            set.insert(user.into());
        } else {
            set.remove(user);
        }
        Ok(())
    }
}

#[async_trait]
impl SyncPlatform for FakePlatform {
    fn platform(&self) -> ChatPlatform {
        self.platform
    }
    fn configured(&self) -> bool {
        self.configured
    }
    async fn role_safety(&self, roles: &[String]) -> Result<BTreeMap<String, RoleSafety>, PlatformError> {
        self.safety_asks.lock().unwrap().extend(roles.iter().cloned());
        let s = self.safety.lock().unwrap();
        Ok(roles.iter().map(|r| (r.clone(), s.get(r).cloned().unwrap_or(RoleSafety::Safe))).collect())
    }
    async fn holders(&self, roles: &[String]) -> Result<BTreeMap<String, BTreeSet<String>>, PlatformError> {
        let n = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_in_flight.fetch_max(n, Ordering::SeqCst);
        if let Some(d) = self.read_delay {
            tokio::time::sleep(d).await;
        }
        self.reads.lock().unwrap().extend(roles.iter().cloned());
        let all = self.roles.lock().unwrap();
        let out = roles.iter().map(|r| (r.clone(), all.get(r).cloned().unwrap_or_default())).collect();
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        Ok(out)
    }
    async fn add_member(&self, role: &str, user: &str) -> Result<(), PlatformError> {
        self.write("add", role, user)
    }
    async fn remove_member(&self, role: &str, user: &str) -> Result<(), PlatformError> {
        self.write("remove", role, user)
    }
    async fn alert(&self, text: &str) -> Result<(), PlatformError> {
        self.alerts.lock().unwrap().push(text.to_string());
        Ok(())
    }
}

// ---------------------------------------------------------------- fixtures

pub(crate) struct Ids {
    pub founder: &'static str,
    pub mckale: &'static str,
    pub ada: &'static str,
    pub bo: &'static str,
    pub lead: &'static str,
    pub maint: &'static str,
    pub reviewer: &'static str,
    pub contributor: &'static str,
}

pub(crate) fn ids(p: ChatPlatform) -> Ids {
    match p {
        ChatPlatform::Discord => Ids {
            founder: "100000000000000101",
            mckale: "100000000000000102",
            ada: "100000000000000103",
            bo: "100000000000000104",
            lead: "100000000000000080",
            maint: "100000000000000060",
            reviewer: "100000000000000040",
            contributor: "100000000000000010",
        },
        ChatPlatform::Slack => Ids {
            founder: "U1000000101",
            mckale: "U1000000102",
            ada: "U1000000103",
            bo: "U1000000104",
            lead: "S1000000080",
            maint: "S1000000060",
            reviewer: "S1000000040",
            contributor: "S1000000010",
        },
    }
}

fn unmanaged(p: ChatPlatform) -> &'static str {
    match p {
        ChatPlatform::Discord => "100000000000000999",
        ChatPlatform::Slack => "S1000000999",
    }
}

fn managed(p: ChatPlatform) -> Vec<&'static str> {
    match p {
        ChatPlatform::Discord => vec![
            "100000000000000080",
            "100000000000000060",
            "100000000000000040",
            "100000000000000030",
            "100000000000000020",
            "100000000000000010",
        ],
        ChatPlatform::Slack => vec!["S1000000080", "S1000000060", "S1000000040", "S1000000030", "S1000000020", "S1000000010"],
    }
}

/// A clock the test moves by hand.
#[derive(Clone)]
pub(crate) struct TestClock(Arc<AtomicI64>);

impl TestClock {
    pub fn at(t: DateTime<Utc>) -> Self {
        TestClock(Arc::new(AtomicI64::new(t.timestamp())))
    }
    pub fn advance(&self, secs: i64) {
        self.0.fetch_add(secs, Ordering::SeqCst);
    }
    pub fn set(&self, t: DateTime<Utc>) {
        self.0.store(t.timestamp(), Ordering::SeqCst);
    }
    pub fn clock(&self) -> Clock {
        let c = self.0.clone();
        Arc::new(move || Utc.timestamp_opt(c.load(Ordering::SeqCst), 0).unwrap())
    }
}

fn noon(d: NaiveDate) -> DateTime<Utc> {
    Utc.from_utc_datetime(&d.and_hms_opt(12, 0, 0).unwrap())
}

pub(crate) struct Rig {
    pub fake: Arc<FakePlatform>,
    pub runner: Arc<SyncRunner>,
    pub pool: SqlitePool,
    pub cra: CraHandle,
    pub clock: TestClock,
    pub id: Ids,
}

pub(crate) async fn rig_with(p: ChatPlatform, fake: FakePlatform) -> Rig {
    let pool = pool().await;
    let cra = CraHandle::new();
    activate(&cra, &eustress_files());
    let fake = Arc::new(fake);
    let clock = TestClock::at(noon(today()));
    let runner = Arc::new(SyncRunner::new(fake.clone(), pool.clone(), cra.clone()).with_clock(clock.clock()));
    Rig { fake, runner, pool, cra, clock, id: ids(p) }
}

pub(crate) async fn rig(p: ChatPlatform) -> Rig {
    rig_with(p, FakePlatform::new(p, &managed(p))).await
}

async fn drift_rows(pool: &SqlitePool) -> Vec<(String, String, String, String, String, Option<String>)> {
    sqlx::query_as("SELECT platform, role_id, member_id, direction, outcome, reverted_at FROM chat_drift_events ORDER BY id")
        .fetch_all(pool)
        .await
        .unwrap()
}

async fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
    for _ in 0..200 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("timed out waiting for: {what}");
}

/// The first run brings the platform to the projection. None of it is drift.
async fn initial(r: &Rig) -> ReconcileReport {
    let rep = r.runner.run(Trigger::Timer).await;
    assert!(rep.error.is_none() && rep.skipped.is_none(), "{rep:?}");
    assert!(r.fake.holds(r.id.lead, r.id.mckale) && r.fake.holds(r.id.maint, r.id.ada) && r.fake.holds(r.id.reviewer, r.id.bo));
    assert!(rep.reverted.is_empty(), "a first sync of CRA members is not drift: {rep:?}");
    rep
}

// ---------------------------------------------------------------- the core

#[tokio::test]
async fn a_hand_granted_managed_role_is_removed_and_recorded() {
    let r = rig(ChatPlatform::Discord).await;
    let first = initial(&r).await;
    assert_eq!(first.added.len(), 3, "{first:?}");

    r.fake.hand_grant(r.id.lead, r.id.founder);
    let rep = r.runner.run(Trigger::Timer).await;
    assert!(!r.fake.holds(r.id.lead, r.id.founder), "the hand-granted role must be removed");
    assert_eq!(rep.reverted, vec![Drift { role: r.id.lead.into(), user: r.id.founder.into(), direction: DriftDirection::Granted }]);
    let rows = drift_rows(&r.pool).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(
        (rows[0].0.as_str(), rows[0].1.as_str(), rows[0].2.as_str(), rows[0].3.as_str(), rows[0].4.as_str()),
        ("discord", r.id.lead, r.id.founder, "granted", "reverted")
    );
    assert!(rows[0].5.is_some(), "reverted_at is set");
    let alerts = r.fake.alerts.lock().unwrap().clone();
    assert!(alerts.iter().any(|a| a.contains("granted by hand") && a.contains(r.id.founder)), "{alerts:?}");
}

#[tokio::test]
async fn a_hand_removed_managed_role_is_restored_and_recorded() {
    let r = rig(ChatPlatform::Discord).await;
    initial(&r).await;
    r.fake.hand_remove(r.id.reviewer, r.id.bo);
    let rep = r.runner.run(Trigger::Timer).await;
    assert!(r.fake.holds(r.id.reviewer, r.id.bo), "the hand-removed role must be restored");
    assert_eq!(rep.reverted, vec![Drift { role: r.id.reviewer.into(), user: r.id.bo.into(), direction: DriftDirection::Removed }]);
    let rows = drift_rows(&r.pool).await;
    assert_eq!(rows.len(), 1);
    assert_eq!((rows[0].3.as_str(), rows[0].4.as_str()), ("removed", "reverted"));
    assert!(r.fake.alerts.lock().unwrap().iter().any(|a| a.contains("removed by hand")));
}

#[tokio::test]
async fn an_unmanaged_role_is_never_read_or_changed() {
    let r = rig(ChatPlatform::Discord).await;
    initial(&r).await;
    r.fake.hand_grant(r.id.lead, r.id.founder);
    r.runner.run(Trigger::Command).await;
    r.runner.run(Trigger::Activation).await;
    let um = unmanaged(ChatPlatform::Discord);
    assert!(!r.fake.reads.lock().unwrap().iter().any(|x| x == um), "an unmanaged role was read");
    assert!(!r.fake.safety_asks.lock().unwrap().iter().any(|x| x == um), "an unmanaged role was inspected");
    assert!(!r.fake.writes.lock().unwrap().iter().any(|(_, role, _)| role == um), "an unmanaged role was written");
    let holders = r.fake.roles.lock().unwrap().get(um).cloned().unwrap();
    assert_eq!(holders, [r.id.founder.to_string(), "OUTSIDER".to_string()].into());
    // Every read was of a CRA-mapped role.
    let mapped: BTreeSet<String> = managed(ChatPlatform::Discord).into_iter().map(String::from).collect();
    assert!(r.fake.reads.lock().unwrap().iter().all(|x| mapped.contains(x)));
}

#[test]
fn discord_reads_only_the_managed_role_ids() {
    use crate::discord::sync::collect_holders;
    use serenity::all::RoleId;
    let wanted: BTreeMap<RoleId, String> = [(RoleId::new(80), "80".to_string())].into();
    let mut out = BTreeMap::new();
    let alice = [RoleId::new(80), RoleId::new(999)];
    let bob = [RoleId::new(999)];
    collect_holders(&mut out, &wanted, [(1u64, &alice[..]), (2u64, &bob[..])]);
    assert_eq!(out.keys().cloned().collect::<Vec<_>>(), vec!["80".to_string()], "an unmanaged role id was kept");
    assert_eq!(out["80"], ["1".to_string()].into());
}

#[tokio::test]
async fn a_non_member_holding_a_managed_role_is_drift() {
    let r = rig(ChatPlatform::Discord).await;
    // Before MSync ever ran: someone outside the CRA holds a managed role.
    r.fake.hand_grant(r.id.contributor, "100000000000000666");
    let rep = r.runner.run(Trigger::Timer).await;
    assert!(!r.fake.holds(r.id.contributor, "100000000000000666"));
    assert_eq!(rep.reverted, vec![Drift { role: r.id.contributor.into(), user: "100000000000000666".into(), direction: DriftDirection::Granted }]);
    let rows = drift_rows(&r.pool).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].2, "100000000000000666");
}

// ---------------------------------------------------------- the rogue admin

#[tokio::test]
async fn story_a_rogue_admin_is_reverted_three_times_then_held_then_released_by_activation() {
    let r = rig(ChatPlatform::Discord).await;
    initial(&r).await;
    let (top, bo) = (r.id.lead, r.id.bo);

    // Bo (Reviewer, with Discord admin rights) grants himself the top managed role.
    for n in 1..=3 {
        r.clock.advance(600);
        r.fake.hand_grant(top, bo);
        let rep = r.runner.run(Trigger::MemberUpdate).await;
        assert!(!r.fake.holds(top, bo), "revert {n} did not happen");
        assert_eq!(rep.reverted.len(), 1, "revert {n}: {rep:?}");
        assert!(rep.newly_held.is_empty());
    }

    // The fourth time within the hour: held, alerted, not reverted.
    r.clock.advance(600);
    r.fake.hand_grant(top, bo);
    let alerts_before = r.fake.alerts.lock().unwrap().len();
    let writes_before = r.fake.write_count();
    let rep = r.runner.run(Trigger::MemberUpdate).await;
    assert!(r.fake.holds(top, bo), "a held pair is not reverted");
    assert_eq!(rep.newly_held, vec![Drift { role: top.into(), user: bo.into(), direction: DriftDirection::Granted }]);
    assert!(rep.reverted.is_empty());
    assert_eq!(r.fake.write_count(), writes_before, "no write for a held pair");
    let alerts = r.fake.alerts.lock().unwrap().clone();
    assert_eq!(alerts.len(), alerts_before + 1);
    assert!(alerts.last().unwrap().contains("stops reverting"), "{alerts:?}");
    let open: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM chat_drift_holds WHERE released_at IS NULL").fetch_one(&r.pool).await.unwrap();
    assert_eq!(open, 1);
    let rows = drift_rows(&r.pool).await;
    assert_eq!(rows.iter().filter(|x| x.4 == "reverted").count(), 3);
    assert_eq!(rows.iter().filter(|x| x.4 == "held").count(), 1);

    // Still held on later runs (timer, member update), with no new alert.
    r.clock.advance(60);
    let rep = r.runner.run(Trigger::Timer).await;
    assert_eq!(rep.held, vec![(top.to_string(), bo.to_string())]);
    assert!(r.fake.holds(top, bo));
    assert_eq!(r.fake.alerts.lock().unwrap().len(), alerts_before + 1, "a hold is alerted once");

    // The next snapshot activation releases the hold and the role is taken back.
    let rep = r.runner.run(Trigger::Activation).await;
    assert_eq!(rep.holds_released, 1);
    assert!(!r.fake.holds(top, bo), "released by activation, then reverted");
    let open: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM chat_drift_holds WHERE released_at IS NULL").fetch_one(&r.pool).await.unwrap();
    assert_eq!(open, 0);
    let reason: String = sqlx::query_scalar("SELECT release_reason FROM chat_drift_holds").fetch_one(&r.pool).await.unwrap();
    assert_eq!(reason, "snapshot activation");
}

#[tokio::test]
async fn an_explicit_sync_releases_a_hold_and_reverts_after_an_hour_the_count_starts_over() {
    let r = rig(ChatPlatform::Discord).await;
    initial(&r).await;
    let (top, bo) = (r.id.lead, r.id.bo);
    for _ in 0..4 {
        r.clock.advance(60);
        r.fake.hand_grant(top, bo);
        r.runner.run(Trigger::MemberUpdate).await;
    }
    assert!(r.fake.holds(top, bo), "held");
    let rep = r.runner.run(Trigger::Command).await;
    assert_eq!(rep.holds_released, 1);
    assert!(!r.fake.holds(top, bo));
    let reason: String = sqlx::query_scalar("SELECT release_reason FROM chat_drift_holds").fetch_one(&r.pool).await.unwrap();
    assert_eq!(reason, "explicit sync");
    // After a release the count starts over: three more reverts before the next hold.
    for n in 1..=3 {
        r.clock.advance(60);
        r.fake.hand_grant(top, bo);
        r.runner.run(Trigger::MemberUpdate).await;
        assert!(!r.fake.holds(top, bo), "revert {n} after release");
    }
}

// ------------------------------------------------------- dangerous mappings

#[test]
fn every_administrator_class_permission_is_refused() {
    use crate::discord::sync::{classify_discord_role, DANGEROUS};
    use serenity::all::Permissions;
    for (perm, name) in DANGEROUS {
        let got = classify_discord_role(perm | Permissions::SEND_MESSAGES, 1, 10);
        assert_eq!(got, RoleSafety::Dangerous(vec![name.to_string()]), "{name}");
    }
    for name in ["Administrator", "Manage Roles", "Manage Server", "Ban Members", "Kick Members"] {
        assert!(DANGEROUS.iter().any(|(_, n)| *n == name), "{name} must be in the refused list");
    }
    // Below the bot and harmless: safe. At or above the bot: refused.
    assert_eq!(classify_discord_role(Permissions::SEND_MESSAGES | Permissions::VIEW_CHANNEL, 3, 10), RoleSafety::Safe);
    assert_eq!(classify_discord_role(Permissions::SEND_MESSAGES, 10, 10), RoleSafety::AboveBot);
    assert_eq!(classify_discord_role(Permissions::SEND_MESSAGES, 11, 10), RoleSafety::AboveBot);
    assert_eq!(classify_discord_role(Permissions::empty(), 0, 0), RoleSafety::AboveBot, "a bot with no role manages nothing");
}

#[tokio::test]
async fn a_dangerous_role_and_a_role_above_the_bot_are_not_managed_and_are_reported() {
    let r = rig(ChatPlatform::Discord).await;
    r.fake.safety.lock().unwrap().insert(r.id.lead.into(), RoleSafety::Dangerous(vec!["Administrator".into()]));
    r.fake.safety.lock().unwrap().insert(r.id.maint.into(), RoleSafety::AboveBot);
    // Someone outside the CRA holds the dangerous role: MSync does not touch it either way.
    r.fake.hand_grant(r.id.lead, "100000000000000666");
    let rep = r.runner.run(Trigger::Timer).await;
    let refused: Vec<&str> = rep.refused.iter().map(|(x, _)| x.as_str()).collect();
    assert_eq!(refused, vec![r.id.lead, r.id.maint]);
    let reads = r.fake.reads.lock().unwrap().clone();
    assert!(!reads.iter().any(|x| x == r.id.lead || x == r.id.maint), "a refused role was read: {reads:?}");
    assert!(!r.fake.writes.lock().unwrap().iter().any(|(_, role, _)| role == r.id.lead || role == r.id.maint));
    assert!(!r.fake.holds(r.id.lead, r.id.mckale) && r.fake.holds(r.id.lead, "100000000000000666"));
    assert!(r.fake.holds(r.id.reviewer, r.id.bo), "safe roles are still managed");
    // Reported in the alert channel and in the sync reply.
    let alerts = r.fake.alerts.lock().unwrap().clone();
    assert_eq!(alerts.len(), 1);
    assert!(alerts[0].contains(r.id.lead) && alerts[0].contains("Administrator"), "{alerts:?}");
    assert!(alerts[0].contains(r.id.maint) && alerts[0].contains("bot's highest role"), "{alerts:?}");
    let reply = rep.to_sync_report(ChatPlatform::Discord).notes.join("\n");
    assert!(reply.contains("Not managed") && reply.contains("Administrator") && reply.contains("bot's highest role"), "{reply}");
    // Reported once in the alert channel, every time in the sync reply.
    let rep = r.runner.run(Trigger::Timer).await;
    assert_eq!(r.fake.alerts.lock().unwrap().len(), 1, "a refusal is alerted once");
    assert_eq!(rep.refused.len(), 2);
}

// ------------------------------------------------------------- the triggers

/// A CRA source with one commit (the Eustress example), for the activation hook.
struct OneCommit {
    repo: crate::cra::CraRepo,
}

#[async_trait]
impl crate::cra::CraSource for OneCommit {
    fn repo(&self) -> &crate::cra::CraRepo {
        &self.repo
    }
    async fn fetch_files(&self, _sha: &str) -> Result<crate::cra::CraFiles, crate::cra::SourceError> {
        Ok(eustress_files())
    }
    async fn change_context(
        &self,
        _sha: &str,
        _pusher: Option<&crate::cra::FoundryIdentity>,
        today: NaiveDate,
    ) -> Result<crate::cra::ChangeContext, crate::cra::SourceError> {
        Ok(crate::cra::ChangeContext {
            actor: crate::cra::FoundryIdentity::github("example-founder"),
            via_pull_request: false,
            author: None,
            approvers: vec![],
            today,
        })
    }
    async fn branch_head(&self, _branch: &str) -> Result<String, crate::cra::SourceError> {
        Ok(format!("{:040x}", 1))
    }
}

#[tokio::test]
async fn snapshot_activation_triggers_a_reconcile() {
    use crate::auth::{CraDirectory, Directory, EnvDirectory};
    let pool = pool().await;
    let handle = CraHandle::new();
    let owners = vec!["github:example-founder".to_string()];
    let dir: Arc<dyn Directory> = Arc::new(CraDirectory::new(EnvDirectory::new(owners.clone()), handle.clone()).with_today(today()));
    let repo = crate::cra::CraRepo::new(crate::foundry::FoundryProvider::GitHub, "github.com", "example-eustress/cra");
    let ctx = crate::cra::CraContext::new(
        pool.clone(),
        handle.clone(),
        dir,
        owners,
        Some(crate::cra::CraSettings { repo: repo.clone(), branch: "main".into() }),
        Some(Arc::new(OneCommit { repo }) as Arc<dyn crate::cra::CraSource>),
    );
    let fake = Arc::new(FakePlatform::new(ChatPlatform::Discord, &managed(ChatPlatform::Discord)));
    let clock = TestClock::at(noon(today()));
    let runner = Arc::new(SyncRunner::new(fake.clone(), pool.clone(), handle.clone()).with_clock(clock.clock()));
    let _h = spawn(runner, None); // no timer: only triggers run it
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(fake.write_count(), 0, "nothing runs before a trigger");

    let out = ctx.load_and_apply(&format!("{:040x}", 1), None, today()).await.unwrap();
    assert!(matches!(out, crate::cra::LoadOutcome::Activated { .. }), "{out:?}");
    let id = ids(ChatPlatform::Discord);
    wait_for("the activation reconcile", || fake.holds(id.lead, id.mckale) && fake.holds(id.reviewer, id.bo)).await;
}

#[tokio::test]
async fn the_timer_triggers_a_reconcile() {
    let r = rig(ChatPlatform::Discord).await;
    let _h = spawn(r.runner.clone(), Some(Duration::from_millis(50)));
    wait_for("the first timed run", || r.fake.holds(r.id.lead, r.id.mckale)).await;
    r.fake.hand_grant(r.id.lead, r.id.founder);
    wait_for("a timed run to revert drift", || !r.fake.holds(r.id.lead, r.id.founder)).await;
}

#[tokio::test]
async fn two_triggers_at_once_do_not_overlap_or_double_apply() {
    let mut fake = FakePlatform::new(ChatPlatform::Discord, &managed(ChatPlatform::Discord));
    fake.read_delay = Some(Duration::from_millis(80));
    let r = rig_with(ChatPlatform::Discord, fake).await;
    let (a, b) = tokio::join!(r.runner.run(Trigger::Timer), r.runner.run(Trigger::MemberJoin));
    assert_eq!(r.fake.max_in_flight.load(Ordering::SeqCst), 1, "two runs overlapped");
    assert_eq!(r.fake.write_count(), 3, "each change is applied once: {:?}", r.fake.writes.lock().unwrap());
    assert_eq!(a.applied() + b.applied(), 3);
    // Through the background task as well: a burst of triggers coalesces.
    let h = spawn(r.runner.clone(), None);
    for t in [Trigger::MemberUpdate, Trigger::MemberUpdate, Trigger::MemberJoin] {
        h.trigger(t);
    }
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(r.fake.max_in_flight.load(Ordering::SeqCst), 1);
    assert_eq!(r.fake.write_count(), 3);
}

// --------------------------------------------------------------- failures

#[tokio::test]
async fn an_api_failure_leaves_the_change_unapplied_and_the_next_run_applies_it() {
    let r = rig(ChatPlatform::Discord).await;
    r.fake.fail_writes.lock().unwrap().push_back(PlatformError::Api("HTTP 500".into()));
    let rep = r.runner.run(Trigger::Timer).await;
    assert_eq!(rep.failed.len(), 1, "{rep:?}");
    assert_eq!(rep.applied(), 2, "the failed change is not counted as applied");
    let (role, user) = (rep.failed[0].0.clone(), rep.failed[0].1.clone());
    assert!(!r.fake.holds(&role, &user));
    let rep = r.runner.run(Trigger::Timer).await;
    assert!(r.fake.holds(&role, &user), "the next run applies it");
    assert_eq!(rep.added, vec![(role, user)]);
    assert!(rep.reverted.is_empty(), "a retried change is not drift");
    assert!(drift_rows(&r.pool).await.is_empty());
    // Repeating a finished run changes nothing.
    let writes = r.fake.write_count();
    let rep = r.runner.run(Trigger::Timer).await;
    assert_eq!((rep.applied(), r.fake.write_count()), (0, writes));
}

#[tokio::test]
async fn a_rate_limit_stops_the_run_and_backs_off_until_the_retry_time() {
    let r = rig(ChatPlatform::Discord).await;
    r.fake.fail_writes.lock().unwrap().push_back(PlatformError::RateLimited { retry_after_secs: Some(30) });
    let rep = r.runner.run(Trigger::Timer).await;
    assert!(rep.rate_limited);
    assert_eq!(r.fake.write_count(), 1, "the run stops at the rate limit");
    // Inside the back-off: skipped, nothing attempted.
    r.clock.advance(10);
    let rep = r.runner.run(Trigger::MemberUpdate).await;
    assert!(rep.skipped.as_deref().unwrap_or("").contains("backing off"), "{rep:?}");
    assert_eq!(r.fake.write_count(), 1);
    // After it: the next tick finishes the job.
    r.clock.advance(25);
    let rep = r.runner.run(Trigger::Timer).await;
    assert_eq!(rep.applied(), 3, "{rep:?}");
    assert!(r.fake.holds(r.id.lead, r.id.mckale) && r.fake.holds(r.id.maint, r.id.ada) && r.fake.holds(r.id.reviewer, r.id.bo));
}

#[tokio::test]
async fn a_platform_that_is_not_configured_is_skipped_without_error() {
    let mut fake = FakePlatform::new(ChatPlatform::Slack, &managed(ChatPlatform::Slack));
    fake.configured = false;
    let r = rig_with(ChatPlatform::Slack, fake).await;
    let rep = r.runner.run(Trigger::Timer).await;
    assert!(rep.skipped.is_some() && rep.error.is_none(), "{rep:?}");
    assert!(r.fake.reads.lock().unwrap().is_empty() && r.fake.write_count() == 0);
    // No CRA: skipped too.
    r.cra.replace(None);
    let mut fake = FakePlatform::new(ChatPlatform::Slack, &managed(ChatPlatform::Slack));
    fake.configured = true;
    let runner = SyncRunner::new(Arc::new(fake), r.pool.clone(), r.cra.clone());
    assert!(runner.run(Trigger::Timer).await.skipped.unwrap().contains("no CRA"));
}

// ------------------------------------------------- expiry and suspension

#[tokio::test]
async fn an_expired_grant_loses_its_role_on_the_next_run() {
    let r = rig(ChatPlatform::Discord).await;
    initial(&r).await;
    // 2027-05-02: mckale's grant (to 2027-05-01) and ada's (to 2027-04-30) have expired.
    r.clock.set(noon(NaiveDate::from_ymd_opt(2027, 5, 2).unwrap()));
    let rep = r.runner.run(Trigger::Timer).await;
    assert!(!r.fake.holds(r.id.lead, r.id.mckale) && !r.fake.holds(r.id.maint, r.id.ada));
    assert!(r.fake.holds(r.id.reviewer, r.id.bo), "a grant without expiry stays");
    assert_eq!(rep.removed.len(), 2, "{rep:?}");
    assert!(rep.reverted.is_empty(), "expiry is a projection change, not drift");
}

#[tokio::test]
async fn a_suspended_member_loses_their_roles_on_the_next_run() {
    let r = rig(ChatPlatform::Discord).await;
    initial(&r).await;
    // Bo is suspended in a new snapshot.
    let mut files = eustress_files();
    let before = "id = \"bo-example\"\nname = \"Bo Example\"\nstatus = \"active\"";
    let normalized = files.members.replace("\r\n", "\n");
    assert!(normalized.contains(before), "fixture changed");
    files.members = normalized.replace(before, "id = \"bo-example\"\nname = \"Bo Example\"\nstatus = \"suspended\"");
    activate(&r.cra, &files);
    let rep = r.runner.run(Trigger::Activation).await;
    assert!(!r.fake.holds(r.id.reviewer, r.id.bo), "a suspended member holds nothing");
    assert_eq!(rep.removed, vec![(r.id.reviewer.to_string(), r.id.bo.to_string())]);
}

// --------------------------------------------------- one logic, two platforms

#[tokio::test]
async fn the_same_reconcile_runs_for_discord_and_slack() {
    let mut seen = Vec::new();
    for p in [ChatPlatform::Discord, ChatPlatform::Slack] {
        let r = rig(p).await;
        let first = initial(&r).await;
        r.fake.hand_grant(r.id.lead, r.id.founder);
        r.fake.hand_remove(r.id.reviewer, r.id.bo);
        let rep = r.runner.run(Trigger::Timer).await;
        assert!(!r.fake.holds(r.id.lead, r.id.founder) && r.fake.holds(r.id.reviewer, r.id.bo), "{p:?}");
        let rows = drift_rows(&r.pool).await;
        assert!(rows.iter().all(|x| x.0 == p.as_str()), "{rows:?}");
        let mut dirs: Vec<&str> = rows.iter().map(|x| x.3.as_str()).collect();
        dirs.sort();
        seen.push((first.added.len(), rep.reverted.len(), dirs.into_iter().map(String::from).collect::<Vec<_>>()));
        assert!(r.fake.alerts.lock().unwrap().iter().any(|a| a.contains(p.label())));
    }
    assert_eq!(seen[0], seen[1], "Discord and Slack behave the same");
    assert_eq!(seen[0], (3, 2, vec!["granted".to_string(), "removed".to_string()]));
}

#[tokio::test]
async fn record_sync_event_writes_only_drift_today() {
    let pool = pool().await;
    let now = noon(today());
    let mk = |kind| SyncEvent {
        platform: ChatPlatform::Slack,
        kind,
        role: "S1".into(),
        member: "U1".into(),
        direction: Some(DriftDirection::Granted),
        observed_at: now,
        reverted_at: Some(now),
        snapshot_id: Some(7),
        detail: String::new(),
    };
    assert!(record_sync_event(&pool, &mk(SyncEventKind::DriftReverted)).await.unwrap().is_some());
    assert!(record_sync_event(&pool, &mk(SyncEventKind::DriftHeld)).await.unwrap().is_some());
    for k in [SyncEventKind::ChangeApplied, SyncEventKind::RoleRefused, SyncEventKind::HoldsReleased] {
        assert!(record_sync_event(&pool, &mk(k)).await.unwrap().is_none());
    }
    assert_eq!(drift_rows(&pool).await.len(), 2);
    // Every kind reaches the audit log, and the chain stays intact.
    let actions: Vec<String> = sqlx::query_scalar("SELECT action FROM audit_log WHERE action LIKE 'chat.sync.%' ORDER BY seq")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(
        actions,
        vec![
            "chat.sync.drift_reverted",
            "chat.sync.drift_held",
            "chat.sync.change_applied",
            "chat.sync.role_refused",
            "chat.sync.holds_released"
        ]
    );
    assert!(crate::audit::verify(&pool).await.unwrap().ok);
}
