//! Discord as a [`SyncPlatform`], over serenity HTTP (contract 13.3, hierarchy doc 7.3).
//!
//! The managed roles are the `discord_role` ids in `roles.toml`. Discord has no "members of
//! a role" endpoint, so [`SyncPlatform::holders`] pages through the guild member list (the
//! `GUILD_MEMBERS` intent and the Server Members privileged intent are required) and keeps
//! only the requested role ids; every other role id is dropped on the spot and never
//! compared or written.
//!
//! A mapped role is refused (not managed at all) when it carries an administrator-class
//! permission ([`DANGEROUS`]) or sits at or above the bot's own highest role. Discord would
//! reject the second anyway; MSync says so instead of failing on every run.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use async_trait::async_trait;
use serenity::all::{ChannelId, GuildId, Http, Permissions, RoleId, UserId};

use crate::chat::sync::{PlatformError, RoleSafety, SyncPlatform};
use crate::chat::ChatPlatform;
use crate::cra::loader::CraHandle;

/// Permissions that make a role administrator-class. A CRA role mapped to a Discord role
/// with any of these is never managed (hierarchy doc 7.3: merit never confers authority).
pub const DANGEROUS: [(Permissions, &str); 6] = [
    (Permissions::ADMINISTRATOR, "Administrator"),
    (Permissions::MANAGE_ROLES, "Manage Roles"),
    (Permissions::MANAGE_GUILD, "Manage Server"),
    (Permissions::BAN_MEMBERS, "Ban Members"),
    (Permissions::KICK_MEMBERS, "Kick Members"),
    (Permissions::MANAGE_WEBHOOKS, "Manage Webhooks"),
];

/// Discord's `GET /guilds/{id}/members` page size limit.
const MEMBER_PAGE: u64 = 1000;
/// Discord's message length limit.
const MESSAGE_LIMIT: usize = 2000;
const AUDIT_REASON: &str = "MSync role sync: CRA projection";

/// Whether a Discord role may be managed: no dangerous permission, and strictly below the
/// bot's highest role.
pub fn classify_discord_role(perms: Permissions, position: u16, bot_top: u16) -> RoleSafety {
    let found: Vec<String> = DANGEROUS
        .iter()
        .filter(|(p, _)| perms.contains(*p))
        .map(|(_, n)| n.to_string())
        .collect();
    if !found.is_empty() && !crate::chat::sync::mutant("discord_allows_dangerous") {
        return RoleSafety::Dangerous(found);
    }
    if position >= bot_top && !crate::chat::sync::mutant("discord_ignores_bot_position") {
        return RoleSafety::AboveBot;
    }
    RoleSafety::Safe
}

/// Fold one page of guild members into `out`, keeping only the requested role ids. Every
/// other role a member holds is dropped here and never seen by the reconcile.
pub fn collect_holders<'a>(
    out: &mut BTreeMap<String, BTreeSet<String>>,
    wanted: &BTreeMap<RoleId, String>,
    members: impl IntoIterator<Item = (u64, &'a [RoleId])>,
) {
    for (user, roles) in members {
        for rid in roles {
            if let Some(key) = wanted.get(rid) {
                out.entry(key.clone()).or_default().insert(user.to_string());
            } else if crate::chat::sync::mutant("discord_keeps_unmanaged") {
                out.entry(rid.get().to_string())
                    .or_default()
                    .insert(user.to_string());
            }
        }
    }
}

fn map_err(e: serenity::Error) -> PlatformError {
    if let serenity::Error::Http(serenity::http::HttpError::UnsuccessfulRequest(r)) = &e {
        if r.status_code.as_u16() == 429 {
            return PlatformError::RateLimited {
                retry_after_secs: None,
            };
        }
    }
    PlatformError::Api(e.to_string())
}

fn parse_id(s: &str, what: &str) -> Result<u64, PlatformError> {
    s.trim()
        .parse::<u64>()
        .ok()
        .filter(|v| *v != 0)
        .ok_or_else(|| PlatformError::Api(format!("{what} `{s}` is not a Discord id")))
}

pub struct DiscordPlatform {
    pub http: Arc<Http>,
    pub cra: CraHandle,
    /// `DISCORD_GUILD_ID`, used when the CRA has no `[discord]` section.
    pub fallback_guild: Option<u64>,
    /// A bot token is configured.
    pub token_set: bool,
}

impl DiscordPlatform {
    /// The guild: the CRA's `[discord] guild_id`, else `DISCORD_GUILD_ID`.
    pub fn guild(&self) -> Option<GuildId> {
        let from_cra = self.cra.active().and_then(|a| {
            a.snapshot
                .org
                .discord
                .as_ref()
                .and_then(|d| d.guild_id.trim().parse::<u64>().ok())
        });
        from_cra
            .or(self.fallback_guild)
            .filter(|g| *g != 0)
            .map(GuildId::new)
    }

    fn alert_channel(&self) -> Option<ChannelId> {
        let a = self.cra.active()?;
        let id = a
            .snapshot
            .org
            .discord
            .as_ref()?
            .alert_channel_id
            .as_ref()?
            .trim()
            .parse::<u64>()
            .ok()?;
        (id != 0).then(|| ChannelId::new(id))
    }

    fn guild_or_err(&self) -> Result<GuildId, PlatformError> {
        self.guild()
            .ok_or_else(|| PlatformError::Api("no Discord guild configured".into()))
    }
}

#[async_trait]
impl SyncPlatform for DiscordPlatform {
    fn platform(&self) -> ChatPlatform {
        ChatPlatform::Discord
    }

    fn configured(&self) -> bool {
        self.token_set && self.guild().is_some()
    }

    async fn role_safety(
        &self,
        roles: &[String],
    ) -> Result<BTreeMap<String, RoleSafety>, PlatformError> {
        let guild = self.guild_or_err()?;
        let all = self.http.get_guild_roles(guild).await.map_err(map_err)?;
        let me = self
            .http
            .get_current_user_guild_member(guild)
            .await
            .map_err(map_err)?;
        let bot_top = all
            .iter()
            .filter(|r| me.roles.contains(&r.id))
            .map(|r| r.position)
            .max()
            .unwrap_or(0);
        Ok(roles
            .iter()
            .map(|id| {
                let safety = match id
                    .parse::<u64>()
                    .ok()
                    .and_then(|n| all.iter().find(|r| r.id.get() == n))
                {
                    // @everyone (id = guild id) is held by everyone and cannot be granted.
                    Some(r) if r.id.get() == guild.get() => {
                        RoleSafety::Dangerous(vec!["@everyone".into()])
                    }
                    Some(r) => classify_discord_role(r.permissions, r.position, bot_top),
                    None => RoleSafety::Missing,
                };
                (id.clone(), safety)
            })
            .collect())
    }

    async fn holders(
        &self,
        roles: &[String],
    ) -> Result<BTreeMap<String, BTreeSet<String>>, PlatformError> {
        let guild = self.guild_or_err()?;
        let wanted: BTreeMap<RoleId, String> = roles
            .iter()
            .filter_map(|r| {
                r.parse::<u64>()
                    .ok()
                    .filter(|n| *n != 0)
                    .map(|n| (RoleId::new(n), r.clone()))
            })
            .collect();
        let mut out: BTreeMap<String, BTreeSet<String>> =
            roles.iter().map(|r| (r.clone(), BTreeSet::new())).collect();
        let mut after: Option<u64> = None;
        loop {
            let page = self
                .http
                .get_guild_members(guild, Some(MEMBER_PAGE), after)
                .await
                .map_err(map_err)?;
            collect_holders(
                &mut out,
                &wanted,
                page.iter().map(|m| (m.user.id.get(), m.roles.as_slice())),
            );
            if (page.len() as u64) < MEMBER_PAGE {
                break;
            }
            after = page.iter().map(|m| m.user.id.get()).max();
        }
        Ok(out)
    }

    async fn add_member(&self, role: &str, user: &str) -> Result<(), PlatformError> {
        let guild = self.guild_or_err()?;
        let (r, u) = (parse_id(role, "role")?, parse_id(user, "user")?);
        self.http
            .add_member_role(guild, UserId::new(u), RoleId::new(r), Some(AUDIT_REASON))
            .await
            .map_err(map_err)
    }

    async fn remove_member(&self, role: &str, user: &str) -> Result<(), PlatformError> {
        let guild = self.guild_or_err()?;
        let (r, u) = (parse_id(role, "role")?, parse_id(user, "user")?);
        self.http
            .remove_member_role(guild, UserId::new(u), RoleId::new(r), Some(AUDIT_REASON))
            .await
            .map_err(map_err)
    }

    async fn alert(&self, text: &str) -> Result<(), PlatformError> {
        let channel = self.alert_channel().ok_or_else(|| {
            PlatformError::Refused("no [discord] alert_channel_id in org.toml".into())
        })?;
        let mut t = text.to_string();
        if t.chars().count() > MESSAGE_LIMIT {
            t = t.chars().take(MESSAGE_LIMIT - 1).collect::<String>() + "…";
        }
        channel
            .say(&self.http, t)
            .await
            .map(|_| ())
            .map_err(map_err)
    }
}
