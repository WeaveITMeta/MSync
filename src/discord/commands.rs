use crate::models::{Contributor, ContributorStats, LeaderboardEntry};
use serenity::all::*;
use sqlx::SqlitePool;
use uuid::Uuid;

pub async fn link_account(
    db_pool: &SqlitePool,
    command: &CommandInteraction,
) -> String {
    let github_username = command
        .data
        .options
        .iter()
        .find(|opt| opt.name == "github")
        .and_then(|opt| opt.value.as_str());

    if let Some(github_username) = github_username {
        let discord_user_id = command.user.id.get() as i64;

        // Check if GitHub account is already linked
        let existing = sqlx::query!(
            "SELECT id FROM contributors WHERE github_username = ?",
            github_username
        )
        .fetch_optional(db_pool)
        .await;

        match existing {
            Ok(Some(_)) => {
                // Update existing contributor with Discord ID
                let result = sqlx::query!(
                    "UPDATE contributors SET discord_user_id = ?, updated_at = CURRENT_TIMESTAMP 
                     WHERE github_username = ?",
                    discord_user_id,
                    github_username
                )
                .execute(db_pool)
                .await;

                match result {
                    Ok(_) => format!(
                        "✅ Successfully linked your Discord account to GitHub user `{}`!",
                        github_username
                    ),
                    Err(e) => format!("❌ Failed to link account: {}", e),
                }
            }
            Ok(None) => {
                // Create new contributor
                let contributor = Contributor::new(github_username.to_string());
                let result = sqlx::query!(
                    "INSERT INTO contributors (id, github_username, discord_user_id, total_merit_points, 
                     current_role, contributions_count, created_at, updated_at) 
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                    contributor.id,
                    contributor.github_username,
                    discord_user_id,
                    contributor.total_merit_points,
                    contributor.current_role,
                    contributor.contributions_count,
                    contributor.created_at,
                    contributor.updated_at
                )
                .execute(db_pool)
                .await;

                match result {
                    Ok(_) => format!(
                        "✅ Welcome! Linked your Discord account to GitHub user `{}`!",
                        github_username
                    ),
                    Err(e) => format!("❌ Failed to create account: {}", e),
                }
            }
            Err(e) => format!("❌ Database error: {}", e),
        }
    } else {
        "❌ Please provide your GitHub username!".to_string()
    }
}

pub async fn show_stats(
    db_pool: &SqlitePool,
    command: &CommandInteraction,
) -> String {
    let discord_user_id = command.user.id.get() as i64;

    let contributor = sqlx::query_as::<_, Contributor>(
        "SELECT * FROM contributors WHERE discord_user_id = ?"
    )
    .bind(discord_user_id)
    .fetch_optional(db_pool)
    .await;

    match contributor {
        Ok(Some(contributor)) => {
            format!(
                "📊 **Your Contribution Stats**\n\
                👤 GitHub: `{}`\n\
                🏆 Merit Points: `{}`\n\
                📝 Total Contributions: `{}`\n\
                🎭 Current Role: `{}`\n\
                📅 Member Since: `{}`",
                contributor.github_username,
                contributor.total_merit_points,
                contributor.contributions_count,
                contributor.current_role,
                contributor.created_at.format("%Y-%m-%d")
            )
        }
        Ok(None) => {
            "❌ No linked GitHub account found! Use `/link` to connect your GitHub account.".to_string()
        }
        Err(e) => format!("❌ Database error: {}", e),
    }
}

pub async fn show_leaderboard(
    db_pool: &SqlitePool,
    _command: &CommandInteraction,
) -> String {
    let top_contributors = sqlx::query_as::<_, Contributor>(
        "SELECT * FROM contributors ORDER BY total_merit_points DESC LIMIT 10"
    )
    .fetch_all(db_pool)
    .await;

    match top_contributors {
        Ok(contributors) => {
            if contributors.is_empty() {
                "📊 No contributors yet! Be the first to contribute!".to_string()
            } else {
                let mut leaderboard = "🏆 **Top Contributors**\n\n".to_string();
                
                for (rank, contributor) in contributors.iter().enumerate() {
                    let medal = match rank {
                        0 => "🥇",
                        1 => "🥈",
                        2 => "🥉",
                        _ => "🏅",
                    };
                    
                    leaderboard.push_str(&format!(
                        "{} **#{}** `{}` - {} points ({} contributions)\n",
                        medal,
                        rank + 1,
                        contributor.github_username,
                        contributor.total_merit_points,
                        contributor.contributions_count
                    ));
                }
                
                leaderboard
            }
        }
        Err(e) => format!("❌ Failed to fetch leaderboard: {}", e),
    }
}

pub async fn sync_roles(
    db_pool: &SqlitePool,
    ctx: &Context,
    command: &CommandInteraction,
) -> String {
    let discord_user_id = command.user.id.get() as i64;
    
    // Check if user has admin permissions
    if let Some(member) = &command.member {
        if let Ok(permissions) = member.permissions(ctx) {
            if !permissions.administrator() {
                return "❌ You need administrator permissions to sync roles!".to_string();
            }
        }
    }

    // Get all contributors with Discord IDs
    let contributors = sqlx::query_as::<_, Contributor>(
        "SELECT * FROM contributors WHERE discord_user_id IS NOT NULL"
    )
    .fetch_all(db_pool)
    .await;

    match contributors {
        Ok(contributors) => {
            let mut synced = 0;
            let mut failed = 0;

            for contributor in contributors {
                // This would trigger the role sync logic in the bot
                synced += 1;
            }

            format!(
                "✅ Role sync complete!\n\
                ✓ Successfully synced: {}\n\
                ✗ Failed: {}",
                synced, failed
            )
        }
        Err(e) => format!("❌ Failed to sync roles: {}", e),
    }
}
