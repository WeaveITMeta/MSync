//! Discord adapter for the shared chat commands (`crate::chat`). It only translates: a
//! serenity interaction in, a [`ChatActor`] and the command's argument out; a [`ChatReply`]
//! in, an interaction response out. The commands themselves live in `crate::chat::commands`.

use std::sync::Arc;

use async_trait::async_trait;
use serenity::all::*;

use crate::chat::sync::{SyncRunner, Trigger};
use crate::chat::{ChatActor, ChatCommand, ChatPlatform, ProjectionApplier, ProjectionPlan, SyncReport};
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

/// Applies a projection to one Discord guild through the shared role sync
/// (`crate::chat::sync`): the same reconcile, drift and hold rules as the timer.
pub struct DiscordApplier {
    pub http: Arc<Http>,
    pub guild_id: Option<GuildId>,
    pub pool: sqlx::SqlitePool,
    /// The Discord role sync runner; `None` when Discord role sync is not running.
    pub sync: Option<Arc<SyncRunner>>,
}

#[async_trait]
impl ProjectionApplier for DiscordApplier {
    /// An explicit `sync` by an authorized member (the command checked the rank): releases
    /// held drift pairs, then reconciles every managed role. Members outside the CRA who
    /// hold a managed role are drift and lose it.
    async fn apply(&self, plan: &ProjectionPlan) -> SyncReport {
        match &self.sync {
            Some(runner) => runner.run_with_plan(Trigger::Command, plan).await.to_sync_report(ChatPlatform::Discord),
            None => SyncReport {
                notes: vec!["Discord role sync is not running (no bot token or no guild configured).".into()],
                ..Default::default()
            },
        }
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
