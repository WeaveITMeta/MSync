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
}

impl DiscordBot {
    pub fn new(
        db_pool: SqlitePool,
        merit_tracker: Arc<RwLock<MeritTracker>>,
        chat: Arc<crate::chat::ChatContext>,
    ) -> Self {
        Self {
            db_pool,
            merit_tracker,
            chat,
        }
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
                    sqlx::query!(
                        "UPDATE contributors SET current_role = ?, updated_at = CURRENT_TIMESTAMP WHERE id = ?",
                        role.role_name,
                        contributor.id
                    )
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
