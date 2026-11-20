use crate::models::{Contributor, MeritRole};
use anyhow::Result;
use serenity::all::*;
use sqlx::SqlitePool;
use std::sync::Arc;
use tracing::{error, info, warn};

pub struct DiscordSyncService {
    db_pool: SqlitePool,
    discord_http: Arc<serenity::http::Http>,
}

impl DiscordSyncService {
    pub fn new(db_pool: SqlitePool, discord_http: Arc<serenity::http::Http>) -> Self {
        Self {
            db_pool,
            discord_http,
        }
    }

    pub async fn sync_contributor_role(
        &self,
        guild_id: GuildId,
        contributor: &Contributor,
    ) -> Result<()> {
        if let Some(discord_user_id) = contributor.discord_user_id {
            let user_id = UserId::from(discord_user_id as u64);

            // Get merit roles from database
            let roles = sqlx::query_as::<_, MeritRole>(
                "SELECT * FROM merit_roles ORDER BY required_merit_points DESC",
            )
            .fetch_all(&self.db_pool)
            .await?;

            // Find the appropriate role
            let qualified_role = roles
                .iter()
                .find(|role| contributor.total_merit_points >= role.required_merit_points);

            if let Some(role) = qualified_role {
                // Get guild member
                if let Ok(member) = guild_id.member(&self.discord_http, user_id).await {
                    // Remove all merit roles first
                    for merit_role in &roles {
                        let role_id = RoleId::from(merit_role.discord_role_id as u64);
                        if member.roles.contains(&role_id) {
                            if let Err(e) = member.remove_role(&self.discord_http, role_id).await {
                                warn!("Failed to remove role {}: {}", role_id, e);
                            }
                        }
                    }

                    // Add the new role
                    let new_role_id = RoleId::from(role.discord_role_id as u64);
                    if let Err(e) = member.add_role(&self.discord_http, new_role_id).await {
                        error!("Failed to add role {}: {}", new_role_id, e);
                    } else {
                        info!(
                            "Synced role for {} - now has {}",
                            contributor.github_username, role.role_name
                        );
                    }
                }
            }
        }

        Ok(())
    }

    pub async fn sync_all_roles(&self, guild_id: GuildId) -> Result<(i32, i32)> {
        let contributors = sqlx::query_as::<_, Contributor>(
            "SELECT * FROM contributors WHERE discord_user_id IS NOT NULL",
        )
        .fetch_all(&self.db_pool)
        .await?;

        let mut synced = 0;
        let mut failed = 0;

        for contributor in contributors {
            match self.sync_contributor_role(guild_id, &contributor).await {
                Ok(_) => synced += 1,
                Err(e) => {
                    error!(
                        "Failed to sync role for {}: {}",
                        contributor.github_username, e
                    );
                    failed += 1;
                }
            }
        }

        info!("Role sync complete: {} synced, {} failed", synced, failed);
        Ok((synced, failed))
    }

    pub async fn announce_role_change(
        &self,
        channel_id: ChannelId,
        contributor: &Contributor,
        old_role: &str,
        new_role: &str,
    ) -> Result<()> {
        let message = format!(
            "🎉 Congratulations <@{}>! You've progressed from **{}** to **{}** with {} merit points!",
            contributor.discord_user_id.unwrap_or(0),
            old_role,
            new_role,
            contributor.total_merit_points
        );

        channel_id.say(&self.discord_http, message).await?;
        Ok(())
    }

    pub async fn create_discord_roles(&self, guild_id: GuildId) -> Result<()> {
        let merit_roles: Vec<(String, i64)> =
            sqlx::query_as("SELECT role_name, permissions_level FROM merit_roles")
                .fetch_all(&self.db_pool)
                .await?;

        for (role_name, permissions_level) in merit_roles {
            let color = match permissions_level {
                5 => 0xFF0000, // Red for Maintainer
                4 => 0xFF7F00, // Orange for Core Contributor
                3 => 0xFFFF00, // Yellow for Active Contributor
                2 => 0x00FF00, // Green for Contributor
                _ => 0x0000FF, // Blue for Newcomer
            };

            let new_role = guild_id
                .create_role(
                    &self.discord_http,
                    EditRole::new()
                        .name(&role_name)
                        .colour(color)
                        .mentionable(false)
                        .hoist(true),
                )
                .await?;

            // Update database with Discord role ID
            let new_role_id = new_role.id.get() as i64;
            sqlx::query("UPDATE merit_roles SET discord_role_id = ? WHERE role_name = ?")
                .bind(new_role_id)
                .bind(&role_name)
                .execute(&self.db_pool)
                .await?;

            info!("Created Discord role: {} (ID: {})", role_name, new_role.id);
        }

        Ok(())
    }
}
