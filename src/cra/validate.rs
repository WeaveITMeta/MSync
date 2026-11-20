//! Cross-field and cross-file rules (contract section 3 plus the rules this
//! module adds; all are listed in `docs/CRA_CONFIG.md`). Every rule pushes
//! its own error; nothing stops at the first one.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{Duration, NaiveDate};

use super::catalog::{self, PermissionClass};
use super::model::*;
use super::{CraError, CraFileKind, ErrorCode, Snapshot, KNOWN_RUBRICS};

/// Highest rank the repository may assign. 1000 is owners only.
pub const MAX_REPO_RANK: u32 = 999;

/// Fields a consumer may be allowed to read.
pub const CONSUMER_FIELDS: &[&str] = &[
    "status",
    "tier",
    "positions.title",
    "positions.rank",
    "positions.expires_at",
    "merit.vested",
    "permissions",
];

const WEEKDAYS: &[&str] = &[
    "monday",
    "tuesday",
    "wednesday",
    "thursday",
    "friday",
    "saturday",
    "sunday",
];

/// `[a-z0-9-]`, non-empty, not starting or ending with `-`.
pub fn is_valid_key(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('-')
        && !s.ends_with('-')
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// A Discord snowflake: 17 to 20 ASCII digits.
pub fn is_snowflake(s: &str) -> bool {
    (17..=20).contains(&s.len()) && s.chars().all(|c| c.is_ascii_digit())
}

/// The kinds of Slack id the CRA names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlackIdKind {
    Team,
    User,
    Usergroup,
    Channel,
}

impl SlackIdKind {
    fn prefixes(self) -> &'static [char] {
        match self {
            SlackIdKind::Team => &['T', 'E'],
            SlackIdKind::User => &['U', 'W'],
            SlackIdKind::Usergroup => &['S'],
            SlackIdKind::Channel => &['C', 'G'],
        }
    }
}

/// A Slack id: the prefix for its kind, then 8 to 20 uppercase letters or
/// digits (Slack ids are 9 to 11 characters today; the upper bound leaves
/// room without accepting arbitrary text).
pub fn is_slack_id(s: &str, kind: SlackIdKind) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    kind.prefixes().contains(&first)
        && (9..=21).contains(&s.len())
        && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
}

struct V<'a> {
    s: &'a Snapshot,
    today: NaiveDate,
    errors: Vec<CraError>,
}

impl<'a> V<'a> {
    fn err(&mut self, file: CraFileKind, code: ErrorCode, msg: impl Into<String>) {
        self.errors.push(CraError::new(file, code, msg));
    }
}

pub fn validate(s: &Snapshot, today: NaiveDate) -> Vec<CraError> {
    let mut v = V {
        s,
        today,
        errors: Vec::new(),
    };
    org(&mut v);
    roles(&mut v);
    members(&mut v);
    merit(&mut v);
    payouts(&mut v);
    v.errors
}

fn known_permission(s: &Snapshot, key: &str) -> bool {
    s.permission_class(key).is_some()
}

fn known_surface(s: &Snapshot, surface: &str) -> bool {
    catalog::CATALOG
        .iter()
        .any(|(k, _)| catalog::surface_of(k) == surface)
        || s.org
            .permissions_extra
            .iter()
            .any(|p| catalog::surface_of(&p.key) == surface)
}

// ---------------------------------------------------------------- org.toml

fn org(v: &mut V) {
    use CraFileKind::Org as F;
    let s = v.s;
    let o = &s.org;
    if !is_valid_key(&o.org.slug) {
        v.err(
            F,
            ErrorCode::InvalidKey,
            format!("org.slug `{}` must match [a-z0-9-]", o.org.slug),
        );
    }
    if o.org.name.trim().is_empty() {
        v.err(F, ErrorCode::EmptyValue, "org.name is empty");
    }
    if let Some(d) = &o.discord {
        if !is_snowflake(&d.guild_id) {
            v.err(
                F,
                ErrorCode::InvalidSnowflake,
                format!("discord.guild_id `{}` is not a snowflake", d.guild_id),
            );
        }
        if let Some(c) = &d.alert_channel_id {
            if !is_snowflake(c) {
                v.err(
                    F,
                    ErrorCode::InvalidSnowflake,
                    format!("discord.alert_channel_id `{c}` is not a snowflake"),
                );
            }
        }
    }

    if let Some(sl) = &o.slack {
        if !is_slack_id(&sl.team_id, SlackIdKind::Team) {
            v.err(
                F,
                ErrorCode::InvalidSlackId,
                format!(
                    "slack.team_id `{}` is not a Slack team id (T...)",
                    sl.team_id
                ),
            );
        }
        if let Some(c) = &sl.alert_channel_id {
            if !is_slack_id(c, SlackIdKind::Channel) {
                v.err(
                    F,
                    ErrorCode::InvalidSlackId,
                    format!(
                        "slack.alert_channel_id `{c}` is not a Slack channel id (C... or G...)"
                    ),
                );
            }
        }
    }
    if o.discord.is_none() && o.slack.is_none() {
        v.err(
            F,
            ErrorCode::NoChatPlatform,
            "org.toml needs [discord], [slack], or both",
        );
    }

    // permissions_extra
    let mut seen = BTreeSet::new();
    for p in &o.permissions_extra {
        if !catalog::is_permission_shape(&p.key) {
            v.err(
                F,
                ErrorCode::InvalidPermission,
                format!(
                    "permissions_extra `{}` must look like surface.action",
                    p.key
                ),
            );
        }
        if catalog::catalog_class(&p.key).is_some() {
            v.err(
                F,
                ErrorCode::DuplicatePermission,
                format!("permissions_extra `{}` is already in the catalog", p.key),
            );
        }
        if !seen.insert(p.key.clone()) {
            v.err(
                F,
                ErrorCode::DuplicatePermission,
                format!("permissions_extra `{}` is declared twice", p.key),
            );
        }
    }

    // [[foundry]] repositories
    let mut seen = BTreeSet::new();
    for f in &o.foundries {
        let parts: Vec<&str> = f.repo.split('/').collect();
        if parts.len() < 2 || parts.iter().any(|p| p.trim().is_empty()) {
            v.err(
                F,
                ErrorCode::InvalidRepo,
                format!("[[foundry]] repo `{}` must be `owner/name`", f.repo),
            );
        }
        if let Some(h) = &f.host {
            if h.trim().is_empty() || h.contains('/') || h.contains(':') {
                v.err(
                    F,
                    ErrorCode::InvalidRepo,
                    format!("[[foundry]] host `{h}` must be a bare host name"),
                );
            }
        }
        let host = f
            .host
            .clone()
            .unwrap_or_else(|| default_host(f.provider).to_string())
            .to_ascii_lowercase();
        if !seen.insert((f.provider, host.clone(), f.repo.to_ascii_lowercase())) {
            v.err(
                F,
                ErrorCode::DuplicateFoundry,
                format!(
                    "[[foundry]] repository {}:{}/{} is listed twice",
                    f.provider.as_str(),
                    host,
                    f.repo
                ),
            );
        }
        if !f.canonical && !f.payout_branches.is_empty() {
            v.err(
                F,
                ErrorCode::MirrorPaysOut,
                format!(
                    "[[foundry]] repository `{}` is not canonical but lists payout_branches",
                    f.repo
                ),
            );
        }
    }

    // consumers
    let mut seen = BTreeSet::new();
    for c in &o.consumers {
        if !is_valid_key(&c.key) {
            v.err(
                F,
                ErrorCode::InvalidKey,
                format!("consumer key `{}` must match [a-z0-9-]", c.key),
            );
        }
        if !seen.insert(c.key.clone()) {
            v.err(
                F,
                ErrorCode::DuplicateConsumer,
                format!("consumer `{}` is declared twice", c.key),
            );
        }
        for scope in &c.scopes {
            let ok = match scope.strip_suffix(".*") {
                Some(surface) => known_surface(s, surface),
                None => known_permission(s, scope),
            };
            if !ok {
                v.err(
                    F,
                    ErrorCode::UnknownScope,
                    format!(
                        "consumer `{}` scope `{scope}` names no known permission or surface",
                        c.key
                    ),
                );
            }
        }
        for field in &c.fields {
            if !CONSUMER_FIELDS.contains(&field.as_str()) {
                v.err(
                    F,
                    ErrorCode::UnknownConsumerField,
                    format!(
                        "consumer `{}` field `{field}` is not one of {}",
                        c.key,
                        CONSUMER_FIELDS.join(", ")
                    ),
                );
            }
        }
    }

    // compartments
    let mut seen = BTreeSet::new();
    for c in &o.compartments {
        if !is_valid_key(&c.key) {
            v.err(
                F,
                ErrorCode::InvalidKey,
                format!("compartment key `{}` must match [a-z0-9-]", c.key),
            );
        }
        if !seen.insert(c.key.clone()) {
            v.err(
                F,
                ErrorCode::DuplicateCompartment,
                format!("compartment `{}` is declared twice", c.key),
            );
        }
    }
    if !o.compartments.is_empty() && !s.policy.compartments_enabled {
        v.err(
            F,
            ErrorCode::CompartmentsDisabled,
            "compartments are declared but the effective policy has compartments.enabled = false",
        );
    }
}

pub(crate) fn default_host(p: FoundryProvider) -> &'static str {
    match p {
        FoundryProvider::Github => "github.com",
        FoundryProvider::Gitlab => "gitlab.com",
    }
}

// -------------------------------------------------------------- roles.toml

fn roles(v: &mut V) {
    use CraFileKind::Roles as F;
    let s = v.s;
    let p = &s.policy;
    let mut keys = BTreeSet::new();
    let mut discord_roles: BTreeMap<&str, &str> = BTreeMap::new();
    let mut slack_groups: BTreeMap<&str, &str> = BTreeMap::new();
    for r in &s.roles.roles {
        if !is_valid_key(&r.key) {
            v.err(
                F,
                ErrorCode::InvalidKey,
                format!("role key `{}` must match [a-z0-9-]", r.key),
            );
        }
        if !keys.insert(r.key.clone()) {
            v.err(
                F,
                ErrorCode::DuplicateRoleKey,
                format!("role key `{}` is declared twice", r.key),
            );
        }
        if r.title.trim().is_empty() {
            v.err(
                F,
                ErrorCode::EmptyValue,
                format!("role `{}` has an empty title", r.key),
            );
        }
        if r.rank > MAX_REPO_RANK {
            v.err(F, ErrorCode::RankOutOfRange, format!("role `{}` rank {} is above {MAX_REPO_RANK}; rank 1000 is owners only and comes from engine configuration", r.key, r.rank));
        }
        if let Some(d) = &r.discord_role {
            if !is_snowflake(d) {
                v.err(
                    F,
                    ErrorCode::InvalidSnowflake,
                    format!("role `{}` discord_role `{d}` is not a snowflake", r.key),
                );
            }
            if let Some(prev) = discord_roles.insert(d.as_str(), r.key.as_str()) {
                v.err(
                    F,
                    ErrorCode::DuplicateDiscordRole,
                    format!(
                        "discord_role `{d}` is used by both `{prev}` and `{}`",
                        r.key
                    ),
                );
            }
        }
        if let Some(g) = &r.slack_usergroup {
            if !is_slack_id(g, SlackIdKind::Usergroup) {
                v.err(
                    F,
                    ErrorCode::InvalidSlackId,
                    format!(
                        "role `{}` slack_usergroup `{g}` is not a Slack user group id (S...)",
                        r.key
                    ),
                );
            }
            if let Some(prev) = slack_groups.insert(g.as_str(), r.key.as_str()) {
                v.err(
                    F,
                    ErrorCode::DuplicateSlackUsergroup,
                    format!(
                        "slack_usergroup `{g}` is used by both `{prev}` and `{}`",
                        r.key
                    ),
                );
            }
        }
        if let Some(guild) = s.org.discord.as_ref().map(|d| d.guild_id.as_str()) {
            if r.discord_role.as_deref() == Some(guild) {
                v.err(
                    F,
                    ErrorCode::InvalidValue,
                    format!("role `{}` maps to the @everyone role (the guild id)", r.key),
                );
            }
        }
        let mut perms = BTreeSet::new();
        for perm in &r.permissions {
            if !perms.insert(perm) {
                v.err(
                    F,
                    ErrorCode::DuplicatePermission,
                    format!("role `{}` lists `{perm}` twice", r.key),
                );
            }
            match s.permission_class(perm) {
                None => v.err(F, ErrorCode::UnknownPermission, format!("role `{}` permission `{perm}` is not in the catalog or permissions_extra", r.key)),
                Some(PermissionClass::Authority) if r.kind == RoleKind::Tier && !p.promotion_merit_may_confer_authority => {
                    v.err(F, ErrorCode::TierAuthorityPermission, format!("tier `{}` holds authority permission `{perm}`; merit may not confer authority under the effective policy", r.key))
                }
                Some(_) => {}
            }
        }
        for c in &r.compartments {
            if !s.org.compartments.iter().any(|x| &x.key == c) {
                v.err(
                    F,
                    ErrorCode::UnknownCompartment,
                    format!(
                        "role `{}` names compartment `{c}` that org.toml does not declare",
                        r.key
                    ),
                );
            }
        }
        match r.kind {
            RoleKind::Tier => {
                if r.min_merit.is_none() {
                    v.err(
                        F,
                        ErrorCode::TierMissingMinMerit,
                        format!("tier `{}` needs min_merit", r.key),
                    );
                }
                for (name, set) in [
                    ("max_holders", r.max_holders.is_some()),
                    ("term_days", r.term_days.is_some()),
                    ("merit_eligibility", r.merit_eligibility.is_some()),
                ] {
                    if set {
                        v.err(
                            F,
                            ErrorCode::PositionOnlyField,
                            format!(
                                "tier `{}` sets `{name}`, which is for positions only",
                                r.key
                            ),
                        );
                    }
                }
                if r.rank > p.promotion_merit_rank_ceiling {
                    v.err(
                        F,
                        ErrorCode::TierAboveCeiling,
                        format!(
                            "tier `{}` rank {} exceeds promotion.merit_rank_ceiling {}",
                            r.key, r.rank, p.promotion_merit_rank_ceiling
                        ),
                    );
                }
            }
            RoleKind::Position => {
                if r.min_merit.is_some() {
                    v.err(
                        F,
                        ErrorCode::TierOnlyField,
                        format!(
                            "position `{}` sets `min_merit`, which is for tiers only",
                            r.key
                        ),
                    );
                }
                if r.max_holders == Some(0) {
                    v.err(
                        F,
                        ErrorCode::InvalidValue,
                        format!("position `{}` max_holders must be at least 1", r.key),
                    );
                }
                if r.term_days == Some(0) {
                    v.err(
                        F,
                        ErrorCode::InvalidValue,
                        format!("position `{}` term_days must be at least 1", r.key),
                    );
                }
                if let Some(t) = r.term_days {
                    if r.rank >= p.grants_two_person_min_rank && t > p.grants_max_term_days {
                        v.err(
                            F,
                            ErrorCode::TermExceedsPolicy,
                            format!(
                                "position `{}` term_days {t} exceeds grants.max_term_days {}",
                                r.key, p.grants_max_term_days
                            ),
                        );
                    }
                }
            }
        }
    }
}

// ------------------------------------------------------------ members.toml

fn members(v: &mut V) {
    use CraFileKind::Members as F;
    let s = v.s;
    let p = &s.policy;
    let ids: BTreeSet<&str> = s.members.members.iter().map(|m| m.id.as_str()).collect();
    let mut seen_ids = BTreeSet::new();
    let mut identities: BTreeMap<(&str, String), &str> = BTreeMap::new();
    let mut holders: BTreeMap<&str, u32> = BTreeMap::new();

    for m in &s.members.members {
        if !is_valid_key(&m.id) {
            v.err(
                F,
                ErrorCode::InvalidKey,
                format!("member id `{}` must match [a-z0-9-]", m.id),
            );
        }
        if !seen_ids.insert(m.id.as_str()) {
            v.err(
                F,
                ErrorCode::DuplicateMemberId,
                format!("member id `{}` is declared twice", m.id),
            );
        }
        if m.name.trim().is_empty() {
            v.err(
                F,
                ErrorCode::EmptyValue,
                format!("member `{}` has an empty name", m.id),
            );
        }
        for (provider, value) in m.identities.entries() {
            if value.trim().is_empty() {
                v.err(
                    F,
                    ErrorCode::EmptyValue,
                    format!("member `{}` has an empty {provider} identity", m.id),
                );
                continue;
            }
            if provider == "discord" && !is_snowflake(value) {
                v.err(
                    F,
                    ErrorCode::InvalidSnowflake,
                    format!(
                        "member `{}` discord identity `{value}` is not a snowflake",
                        m.id
                    ),
                );
            }
            if provider == "slack" && !is_slack_id(value, SlackIdKind::User) {
                v.err(F, ErrorCode::InvalidSlackId, format!("member `{}` slack identity `{value}` is not a Slack user id (U... or W...)", m.id));
            }
            // GitHub and GitLab logins are case-insensitive; Discord ids are
            // exact; Slack ids are uppercase only, so lowercasing is exact too.
            let norm = value.to_ascii_lowercase();
            if let Some(prev) = identities.insert((provider, norm), m.id.as_str()) {
                v.err(
                    F,
                    ErrorCode::DuplicateIdentity,
                    format!(
                        "{provider} identity `{value}` belongs to both `{prev}` and `{}`",
                        m.id
                    ),
                );
            }
        }

        let mut granted = BTreeSet::new();
        for g in &m.grants {
            if !granted.insert(g.role.as_str()) {
                v.err(
                    F,
                    ErrorCode::DuplicateGrant,
                    format!("member `{}` holds `{}` twice", m.id, g.role),
                );
            }
            if g.granted_by == m.id {
                v.err(
                    F,
                    ErrorCode::SelfGrantedBy,
                    format!(
                        "member `{}` is granted_by themselves for `{}`",
                        m.id, g.role
                    ),
                );
            } else if !ids.contains(g.granted_by.as_str()) {
                v.err(
                    F,
                    ErrorCode::UnknownGranter,
                    format!(
                        "member `{}` grant `{}` names granted_by `{}`, who is not a member",
                        m.id, g.role, g.granted_by
                    ),
                );
            }
            let Some(role) = s.role(&g.role) else {
                v.err(
                    F,
                    ErrorCode::UnknownRole,
                    format!("member `{}` grant names unknown role `{}`", m.id, g.role),
                );
                continue;
            };
            if role.kind == RoleKind::Tier {
                v.err(F, ErrorCode::GrantToTier, format!("member `{}` is granted tier `{}`; tiers are computed from merit, never granted", m.id, g.role));
                continue;
            }
            let high = role.rank >= p.grants_two_person_min_rank;
            // The longest term allowed for this grant, from today.
            let mut limit: Option<u32> = None;
            if high {
                limit = Some(p.grants_max_term_days);
            }
            if let Some(t) = role.term_days {
                limit = Some(limit.map_or(t, |l| l.min(t)));
            }
            match (g.expires, limit) {
                (None, Some(_)) => v.err(F, ErrorCode::GrantMissingExpiry, format!(
                    "member `{}` grant `{}` needs `expires` (rank {} >= grants.two_person_min_rank {}, or the role sets term_days)",
                    m.id, g.role, role.rank, p.grants_two_person_min_rank
                )),
                (Some(exp), Some(days)) if exp > v.today + Duration::days(days as i64) => v.err(F, ErrorCode::GrantTermTooLong, format!(
                    "member `{}` grant `{}` expires {exp}, more than {days} days after {}", m.id, g.role, v.today
                )),
                _ => {}
            }
            let live = m.status != MemberStatus::Removed && g.expires.is_none_or(|e| e >= v.today);
            if live {
                *holders.entry(role.key.as_str()).or_default() += 1;
            }
        }
    }

    for r in &s.roles.roles {
        if let Some(max) = r.max_holders {
            let n = holders.get(r.key.as_str()).copied().unwrap_or(0);
            if n > max {
                v.err(
                    F,
                    ErrorCode::MaxHoldersExceeded,
                    format!("position `{}` has {n} holders, max_holders is {max}", r.key),
                );
            }
        }
    }
}

// -------------------------------------------------------------- merit.toml

fn merit(v: &mut V) {
    use CraFileKind::Merit as F;
    let s = v.s;
    let m = &s.merit;
    if !KNOWN_RUBRICS.contains(&m.rubric.as_str()) {
        v.err(
            F,
            ErrorCode::UnknownRubric,
            format!(
                "rubric `{}` is not known to this engine ({})",
                m.rubric,
                KNOWN_RUBRICS.join(", ")
            ),
        );
    }
    let mut seen = BTreeSet::new();
    for sc in &m.scorers {
        if !seen.insert(*sc) {
            v.err(
                F,
                ErrorCode::DuplicateScorer,
                format!("scorer `{sc:?}` is listed twice"),
            );
        }
    }
    if !m.scorers.is_empty() && (m.min_scorers < 1 || m.min_scorers as usize > m.scorers.len()) {
        v.err(
            F,
            ErrorCode::MinScorers,
            format!(
                "min_scorers {} must be between 1 and the number of scorers ({})",
                m.min_scorers,
                m.scorers.len()
            ),
        );
    }
    let mut seen = BTreeSet::new();
    for e in &m.events {
        if !seen.insert(e.key) {
            v.err(
                F,
                ErrorCode::DuplicateEvent,
                format!("event `{:?}` is declared twice", e.key),
            );
        }
        if e.base_points > e.max_points {
            v.err(
                F,
                ErrorCode::InvalidValue,
                format!(
                    "event `{:?}` base_points {} exceeds max_points {}",
                    e.key, e.base_points, e.max_points
                ),
            );
        }
        if !e.weight.is_finite() || e.weight < 0.0 {
            v.err(
                F,
                ErrorCode::InvalidValue,
                format!("event `{:?}` weight must be a finite number >= 0", e.key),
            );
        }
        if e.external_counts
            && s.policy.merit_external_contributions_earn == crate::cra::policy::ExternalEarn::No
        {
            v.err(F, ErrorCode::ExternalMeritDisallowed, format!("event `{:?}` sets external_counts = true, but the effective policy has merit.external_contributions_earn = false", e.key));
        }
    }
}

// ------------------------------------------------------------ payouts.toml

fn payouts(v: &mut V) {
    use CraFileKind::Payouts as F;
    let s = v.s;
    let p = &s.payouts;
    if !(p.currency.len() == 3 && p.currency.chars().all(|c| c.is_ascii_lowercase())) {
        v.err(
            F,
            ErrorCode::InvalidValue,
            format!(
                "currency `{}` must be a lowercase ISO 4217 code such as usd",
                p.currency
            ),
        );
    }
    // schedule
    let sch = &p.schedule;
    if sch.hour_utc > 23 {
        v.err(
            F,
            ErrorCode::InvalidSchedule,
            format!("schedule.hour_utc {} must be 0..=23", sch.hour_utc),
        );
    }
    match (sch.cadence, &sch.day) {
        (Cadence::Manual, _) => {}
        (Cadence::Monthly, Some(PayoutDay::DayOfMonth(d))) if (1..=28).contains(d) => {}
        (Cadence::Monthly, d) => v.err(
            F,
            ErrorCode::InvalidSchedule,
            format!("monthly schedule.day must be 1..=28, found {d:?}"),
        ),
        (_, Some(PayoutDay::Weekday(w))) if WEEKDAYS.contains(&w.to_ascii_lowercase().as_str()) => {
        }
        (c, d) => v.err(
            F,
            ErrorCode::InvalidSchedule,
            format!("{c:?} schedule.day must be a weekday name, found {d:?}"),
        ),
    }
    // limits
    let l = &p.limits;
    if l.min_payout_cents > l.max_per_payout_cents {
        v.err(
            F,
            ErrorCode::InvalidLimits,
            "limits.min_payout_cents exceeds max_per_payout_cents",
        );
    }
    if l.max_per_payout_cents > l.max_per_member_per_period_cents {
        v.err(
            F,
            ErrorCode::InvalidLimits,
            "limits.max_per_payout_cents exceeds max_per_member_per_period_cents",
        );
    }
    if l.max_per_payout_cents > l.max_per_run_cents {
        v.err(
            F,
            ErrorCode::InvalidLimits,
            "limits.max_per_payout_cents exceeds max_per_run_cents",
        );
    }
    if l.hold_days < s.policy.payouts_hold_days {
        v.err(
            F,
            ErrorCode::HoldBelowPolicy,
            format!(
                "limits.hold_days {} is below the policy floor payouts.hold_days {}",
                l.hold_days, s.policy.payouts_hold_days
            ),
        );
    }
    // approval
    if p.approval.approvals_required < 1 {
        v.err(
            F,
            ErrorCode::ApprovalsRequiredTooLow,
            "approval.approvals_required must be at least 1",
        );
    }
    if p.approval.min_approver_rank > 1000 {
        v.err(
            F,
            ErrorCode::RankOutOfRange,
            "approval.min_approver_rank must be 0..=1000",
        );
    }
    // eligibility
    if p.eligibility.min_rank > 1000 {
        v.err(
            F,
            ErrorCode::RankOutOfRange,
            "eligibility.min_rank must be 0..=1000",
        );
    }
    for key in &p.eligibility.roles {
        match s.role(key) {
            None => v.err(
                F,
                ErrorCode::UnknownRole,
                format!("eligibility.roles names unknown role `{key}`"),
            ),
            Some(r)
                if r.kind == RoleKind::Tier
                    && s.policy.payouts_eligible
                        == crate::cra::policy::PayoutEligible::PositionsOnly =>
            {
                v.err(
                    F,
                    ErrorCode::EligibilityConflictsPolicy,
                    format!(
                        "eligibility.roles names tier `{key}` while the policy pays positions only"
                    ),
                )
            }
            Some(_) => {}
        }
    }
    // rates
    let payable: BTreeSet<&str> = s
        .org
        .foundries
        .iter()
        .filter(|f| f.canonical)
        .flat_map(|f| f.payout_branches.iter().map(|b| b.as_str()))
        .collect();
    let mut seen = BTreeSet::new();
    for r in &p.rates {
        if !seen.insert(r.branch.as_str()) {
            v.err(
                F,
                ErrorCode::DuplicateRate,
                format!("rate for branch `{}` is declared twice", r.branch),
            );
        }
        if !payable.contains(r.branch.as_str()) {
            v.err(
                F,
                ErrorCode::RateBranchNotPayable,
                format!(
                    "rate branch `{}` is not a payout branch of any canonical repository",
                    r.branch
                ),
            );
        }
        if !r.merit_multiplier.is_finite() || r.merit_multiplier < 0.0 {
            v.err(
                F,
                ErrorCode::InvalidValue,
                format!(
                    "rate `{}` merit_multiplier must be a finite number >= 0",
                    r.branch
                ),
            );
        }
    }
}
