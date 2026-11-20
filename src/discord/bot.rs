use crate::merit::tracker::MeritTracker;
use crate::models::{Contributor, MeritRole};
use anyhow::Result;
use serenity::all::*;
use serenity::async_trait;
use sqlx::SqlitePool;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{error, info, warn};

pub struct DiscordBot {
    db_pool: SqlitePool,
    #[allow(dead_code)]
    merit_tracker: Arc<RwLock<MeritTracker>>,
    /// The shared chat commands (`crate::chat`); this bot only translates.
    chat: Arc<crate::chat::ChatContext>,
    /// Discord role sync (`crate::chat::sync`): member joins and member updates trigger a
    /// reconcile so drift is caught at once, not only on the timer.
    sync: Option<crate::chat::sync::SyncHandle>,
}

impl DiscordBot {
    pub fn new(
        db_pool: SqlitePool,
        merit_tracker: Arc<RwLock<MeritTracker>>,
        chat: Arc<crate::chat::ChatContext>,
        sync: Option<crate::chat::sync::SyncHandle>,
    ) -> Self {
        Self {
            db_pool,
            merit_tracker,
            chat,
            sync,
        }
    }

    /// Queue a role sync run when the event is for the managed guild.
    fn trigger_sync(&self, guild_id: GuildId, t: crate::chat::sync::Trigger) -> bool {
        let Some(h) = &self.sync else { return false };
        if self.chat.cra.active().is_none() {
            return false;
        }
        let managed = h.runner().cra().active().and_then(|a| a.snapshot.org.discord.as_ref().map(|d| d.guild_id.clone()));
        if managed.as_deref().map_or(true, |g| g == guild_id.get().to_string()) {
            h.trigger(t);
        }
        true
    }

    async fn sync_user_roles(
        &self,
        ctx: &Context,
        guild_id: GuildId,
        user_id: UserId,
        contributor: &Contributor,
    ) -> Result<()> {
        // Get all merit roles from database
        let roles = sqlx::query_as::<_, MeritRole>(
            "SELECT * FROM merit_roles ORDER BY required_merit_points DESC"
        )
        .fetch_all(&self.db_pool)
        .await?;

        // Find the highest role the contributor qualifies for
        let qualified_role = roles
            .iter()
            .find(|role| contributor.total_merit_points >= role.required_merit_points);

        if let Some(role) = qualified_role {
            // Get the guild member
            if let Ok(mut member) = guild_id.member(&ctx.http, user_id).await {
                // Remove all merit roles
                for merit_role in &roles {
                    let role_id = RoleId::from(merit_role.discord_role_id as u64);
                    if member.roles.contains(&role_id) {
                        if let Err(e) = member.remove_role(&ctx.http, role_id).await {
                            warn!("Failed to remove role {}: {}", role_id, e);
                        }
                    }
                }

                // Add the appropriate role
                let new_role_id = RoleId::from(role.discord_role_id as u64);
                if let Err(e) = member.add_role(&ctx.http, new_role_id).await {
                    error!("Failed to add role {}: {}", new_role_id, e);
                } else {
                    info!(
                        "Updated roles for {} - now has {}",
                        contributor.github_username, role.role_name
                    );

                    // Update contributor's current role in database
                    sqlx::query("UPDATE contributors SET current_role = ?, updated_at = CURRENT_TIMESTAMP WHERE id = ?")
                        .bind(&role.role_name)
                        .bind(contributor.id)
                    .execute(&self.db_pool)
                    .await?;
                }
            }
        }

        Ok(())
    }
}

#[async_trait]
impl EventHandler for DiscordBot {
    async fn ready(&self, ctx: Context, ready: Ready) {
        info!("MSync bot connected as {}", ready.user.name);
        match std::env::var("DISCORD_GUILD_ID").ok().and_then(|v| v.trim().parse::<u64>().ok()) {
            Some(gid) => {
                if let Err(e) = crate::discord::framework::register_slash_commands(&ctx, GuildId::new(gid)).await {
                    error!("Failed to register slash commands: {}", e);
                } else {
                    info!("Registered slash commands for guild {}", gid);
                }
            }
            None => warn!("DISCORD_GUILD_ID not set; slash commands not registered"),
        }
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        match interaction {
            Interaction::Command(command) => {
                let reply = match crate::discord::commands::command_for(&command) {
                    Some((cmd, arg)) => {
                        let actor = crate::discord::commands::actor_for(&command);
                        let applier = crate::discord::commands::DiscordApplier {
                            http: ctx.http.clone(),
                            guild_id: command.guild_id,
                            pool: self.db_pool.clone(),
                            sync: self.sync.as_ref().map(|h| h.runner().clone()),
                        };
                        crate::chat::commands::run(&self.chat, &actor, cmd, &arg, &applier).await
                    }
                    None => crate::chat::ChatReply::private("Unknown command"),
                };

                if let Err(why) = command
                    .create_response(&ctx.http, CreateInteractionResponse::Message(
                        CreateInteractionResponseMessage::new()
                            .content(reply.text)
                            .ephemeral(reply.private)
                    ))
                    .await
                {
                    error!("Cannot respond to slash command: {}", why);
                }
            }
            _ => {}
        }
    }

    async fn guild_member_addition(&self, ctx: Context, new_member: Member) {
        // With a CRA active, the shared role sync gives the new member their projection.
        if self.trigger_sync(new_member.guild_id, crate::chat::sync::Trigger::MemberJoin) {
            return;
        }
        // No CRA: the pre-CRA merit roles.
        // Is this Discord user bound to a contributor (chat_identities)?
        let actor = crate::chat::ChatActor::discord(new_member.user.id.get().to_string(), None);
        let key = match crate::chat::identity::binding_for(&self.db_pool, &actor).await {
            Ok(Some(b)) => b.contributor,
            _ => return,
        };
        if let Ok(Some(contributor)) = sqlx::query_as::<_, Contributor>(
            "SELECT * FROM contributors WHERE github_username = ?"
        )
        .bind(key)
        .fetch_optional(&self.db_pool)
        .await
        {
            // Sync their roles based on merit points
            if let Err(e) = self
                .sync_user_roles(&ctx, new_member.guild_id, new_member.user.id, &contributor)
                .await
            {
                error!("Failed to sync roles for new member: {}", e);
            }
        }
    }

    /// A member's roles changed (by MSync or by hand): reconcile now, so a hand edit of a
    /// managed role is reverted at once. MSync's own changes produce these events too; the
    /// run they cause finds nothing to do.
    async fn guild_member_update(
        &self,
        _ctx: Context,
        _old_if_available: Option<Member>,
        _new: Option<Member>,
        event: GuildMemberUpdateEvent,
    ) {
        self.trigger_sync(event.guild_id, crate::chat::sync::Trigger::MemberUpdate);
    }

    async fn message(&self, ctx: Context, msg: Message) {
        // Monitor for help requests or community engagement
        if msg.content.contains("!help") && !msg.author.bot {
            if let Err(why) = msg.channel_id.say(&ctx.http, 
                "👋 Welcome to MSync! Use `/link` to connect your GitHub account, \
                `/stats` to see your contributions, and `/leaderboard` to view top contributors!"
            ).await {
                error!("Error sending help message: {:?}", why);
            }
        }
    }
}
