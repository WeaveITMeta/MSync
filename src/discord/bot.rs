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
    merit_tracker: Arc<RwLock<MeritTracker>>,
}

impl DiscordBot {
    pub fn new(db_pool: SqlitePool, merit_tracker: Arc<RwLock<MeritTracker>>) -> Self {
        Self {
            db_pool,
            merit_tracker,
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
    async fn ready(&self, _ctx: Context, ready: Ready) {
        info!("DSync bot connected as {}", ready.user.name);
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        match interaction {
            Interaction::Command(command) => {
                let content = match command.data.name.as_str() {
                    "link" => crate::discord::commands::link_account(&self.db_pool, &command).await,
                    "stats" => crate::discord::commands::show_stats(&self.db_pool, &command).await,
                    "leaderboard" => {
                        crate::discord::commands::show_leaderboard(&self.db_pool, &command).await
                    }
                    "sync" => crate::discord::commands::sync_roles(&self.db_pool, &ctx, &command).await,
                    _ => "Unknown command".to_string(),
                };

                if let Err(why) = command
                    .create_response(&ctx.http, CreateInteractionResponse::Message(
                        CreateInteractionResponseMessage::new().content(content)
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
        // Check if this Discord user is linked to a GitHub account
        let discord_user_id = new_member.user.id.get() as i64;
        
        if let Ok(Some(contributor)) = sqlx::query_as::<_, Contributor>(
            "SELECT * FROM contributors WHERE discord_user_id = ?"
        )
        .bind(discord_user_id)
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
                "👋 Welcome to DSync! Use `/link` to connect your GitHub account, \
                `/stats` to see your contributions, and `/leaderboard` to view top contributors!"
            ).await {
                error!("Error sending help message: {:?}", why);
            }
        }
    }
}
