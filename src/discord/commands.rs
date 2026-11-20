//! Discord adapter for the shared chat commands (`crate::chat`). It only translates: a
//! serenity interaction in, a [`ChatActor`] and the command's argument out; a [`ChatReply`]
//! in, an interaction response out. The commands themselves live in `crate::chat::commands`.

use std::sync::Arc;

use async_trait::async_trait;
use serenity::all::*;
use tracing::warn;

use crate::chat::{ChatActor, ChatCommand, ProjectionApplier, ProjectionPlan, SyncReport};
use crate::services::discord_sync::DiscordSyncService;

/// The caller as the shared commands see it. The admin flag is Discord's resolved
/// administrator permission for the interaction; when Discord sends none it is unknown,
/// which fails closed.
pub fn actor_for(command: &CommandInteraction) -> ChatActor {
    let is_admin = command.member.as_ref().and_then(|m| m.permissions).map(|p| p.administrator());
    ChatActor::discord(command.user.id.get().to_string(), is_admin)
}

/// The command and its text argument (`link`'s `github` option).
pub fn command_for(command: &CommandInteraction) -> Option<(ChatCommand, String)> {
    let cmd = ChatCommand::from_name(command.data.name.as_str())?;
    let arg = command
        .data
        .options
        .iter()
        .find(|opt| opt.name == "github")
        .and_then(|opt| opt.value.as_str())
        .unwrap_or("")
        .to_string();
    Some((cmd, arg))
}

/// Applies a projection to one Discord guild.
pub struct DiscordApplier {
    pub http: Arc<Http>,
    pub guild_id: Option<GuildId>,
    pub pool: sqlx::SqlitePool,
}

#[async_trait]
impl ProjectionApplier for DiscordApplier {
    /// For every CRA member with a Discord id: add the managed roles they should hold and
    /// remove the managed roles they should not. Members outside the CRA are left alone
    /// (drift reconciliation is a later round).
    async fn apply(&self, plan: &ProjectionPlan) -> SyncReport {
        let mut report = SyncReport::default();
        let Some(guild_id) = self.guild_id else {
            report.notes.push("Run this command inside the server.".into());
            return report;
        };
        let managed: Vec<RoleId> = plan.managed.iter().filter_map(|r| r.parse::<u64>().ok()).map(RoleId::new).collect();
        for user in &plan.users {
            let Ok(uid) = user.parse::<u64>() else {
                report.failed += 1;
                continue;
            };
            let desired: Vec<RoleId> = plan.desired_for(user).iter().filter_map(|r| r.parse::<u64>().ok()).map(RoleId::new).collect();
            let member = match guild_id.member(&self.http, UserId::new(uid)).await {
                Ok(m) => m,
                Err(e) => {
                    warn!("sync: member {} not readable: {}", user, e);
                    report.failed += 1;
                    continue;
                }
            };
            let mut ok = true;
            for role in &managed {
                let has = member.roles.contains(role);
                let want = desired.contains(role);
                let res = match (has, want) {
                    (false, true) => member.add_role(&self.http, *role).await,
                    (true, false) => member.remove_role(&self.http, *role).await,
                    _ => Ok(()),
                };
                if let Err(e) = res {
                    warn!("sync: role {} for {} failed: {}", role, user, e);
                    ok = false;
                }
            }
            if ok {
                report.synced += 1;
            } else {
                report.failed += 1;
            }
        }
        report
    }

    /// No CRA: the pre-CRA merit roles from `merit_roles`.
    async fn apply_legacy(&self) -> SyncReport {
        let mut report = SyncReport::default();
        let Some(guild_id) = self.guild_id else {
            report.notes.push("Run this command inside the server.".into());
            return report;
        };
        match DiscordSyncService::new(self.pool.clone(), self.http.clone()).sync_all_roles(guild_id).await {
            Ok((synced, failed)) => {
                report.synced = synced.max(0) as u32;
                report.failed = failed.max(0) as u32;
            }
            Err(e) => {
                report.failed += 1;
                report.notes.push(format!("❌ Failed to sync roles: {}", e));
            }
        }
        report
    }
}
