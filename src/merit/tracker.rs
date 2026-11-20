use crate::models::{Contributor, MeritRole};
use anyhow::Result;
use sqlx::SqlitePool;
use std::collections::HashMap;
use tracing::info;
use uuid::Uuid;

pub struct MeritTracker {
    db_pool: SqlitePool,
    role_thresholds: HashMap<String, i32>,
}

impl MeritTracker {
    pub fn new(db_pool: SqlitePool) -> Self {
        let mut role_thresholds = HashMap::new();

        // Default role thresholds
        role_thresholds.insert("Newcomer".to_string(), 0);
        role_thresholds.insert("Contributor".to_string(), 50);
        role_thresholds.insert("Active Contributor".to_string(), 150);
        role_thresholds.insert("Core Contributor".to_string(), 500);
        role_thresholds.insert("Maintainer".to_string(), 1000);

        Self {
            db_pool,
            role_thresholds,
        }
    }

    pub async fn initialize_default_roles(&self) -> Result<()> {
        for (role_name, required_points) in &self.role_thresholds {
            let permissions_level = match role_name.as_str() {
                "Newcomer" => 1,
                "Contributor" => 2,
                "Active Contributor" => 3,
                "Core Contributor" => 4,
                "Maintainer" => 5,
                _ => 1,
            };

            let description = match role_name.as_str() {
                "Newcomer" => "New member of the community",
                "Contributor" => "Has made meaningful contributions",
                "Active Contributor" => "Regularly contributes high-quality code",
                "Core Contributor" => "Key member driving project forward",
                "Maintainer" => "Project maintainer with full permissions",
                _ => "Community member",
            };

            let role_id = Uuid::new_v4();
            let discord_role_id = match crate::config::discord_role_id_for(role_name) {
                Some(id) => id,
                None => {
                    tracing::warn!(
                        "{} not set; role '{}' will not be synced to Discord",
                        crate::config::discord_role_env_var(role_name),
                        role_name
                    );
                    0
                }
            };
            sqlx::query(
                "INSERT OR IGNORE INTO merit_roles (id, role_name, discord_role_id, 
                 required_merit_points, permissions_level, description, created_at) 
                 VALUES (?, ?, ?, ?, ?, ?, CURRENT_TIMESTAMP)",
            )
            .bind(role_id)
            .bind(role_name)
            .bind(discord_role_id)
            .bind(required_points)
            .bind(permissions_level)
            .bind(description)
            .execute(&self.db_pool)
            .await?;

            // Keep existing rows in sync with configured Discord role IDs
            if discord_role_id != 0 {
                sqlx::query("UPDATE merit_roles SET discord_role_id = ? WHERE role_name = ?")
                    .bind(discord_role_id)
                    .bind(role_name)
                    .execute(&self.db_pool)
                    .await?;
            }
        }

        info!("Initialized default merit roles");
        Ok(())
    }

    pub async fn get_contributor_role(&self, contributor_id: Uuid) -> Result<Option<MeritRole>> {
        let contributor =
            sqlx::query_as::<_, Contributor>("SELECT * FROM contributors WHERE id = ?")
                .bind(contributor_id)
                .fetch_optional(&self.db_pool)
                .await?;

        if let Some(contributor) = contributor {
            let role = sqlx::query_as::<_, MeritRole>(
                "SELECT * FROM merit_roles WHERE required_merit_points <= ? 
                 ORDER BY required_merit_points DESC LIMIT 1",
            )
            .bind(contributor.total_merit_points)
            .fetch_optional(&self.db_pool)
            .await?;

            Ok(role)
        } else {
            Ok(None)
        }
    }

    pub async fn check_role_progression(&self, contributor_id: Uuid) -> Result<bool> {
        let contributor =
            sqlx::query_as::<_, Contributor>("SELECT * FROM contributors WHERE id = ?")
                .bind(contributor_id)
                .fetch_optional(&self.db_pool)
                .await?;

        if let Some(contributor) = contributor {
            let new_role = self.get_contributor_role(contributor_id).await?;

            if let Some(role) = new_role {
                if role.role_name != contributor.current_role {
                    // Update contributor's role
                    sqlx::query("UPDATE contributors SET current_role = ?, updated_at = CURRENT_TIMESTAMP WHERE id = ?")
                        .bind(&role.role_name)
                        .bind(contributor_id)
                    .execute(&self.db_pool)
                    .await?;

                    info!(
                        "Contributor {} progressed from {} to {}",
                        contributor.github_username, contributor.current_role, role.role_name
                    );

                    return Ok(true);
                }
            }
        }

        Ok(false)
    }

    pub async fn get_role_distribution(&self) -> Result<HashMap<String, i32>> {
        let rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT current_role, COUNT(*) as count FROM contributors GROUP BY current_role",
        )
        .fetch_all(&self.db_pool)
        .await?;

        let mut distribution = HashMap::new();
        for (role, count) in rows {
            distribution.insert(role, count as i32);
        }

        Ok(distribution)
    }

    pub async fn recalculate_all_roles(&self) -> Result<i32> {
        let contributors = sqlx::query_as::<_, Contributor>("SELECT * FROM contributors")
            .fetch_all(&self.db_pool)
            .await?;

        let mut updated_count = 0;

        for contributor in contributors {
            if self.check_role_progression(contributor.id).await? {
                updated_count += 1;
            }
        }

        info!(
            "Recalculated roles for all contributors. {} roles updated",
            updated_count
        );
        Ok(updated_count)
    }
}
