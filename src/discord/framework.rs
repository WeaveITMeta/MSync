use serenity::all::*;
use serenity::framework::standard::{
    macros::{command, group},
    CommandResult, StandardFramework,
};

#[group]
struct General;

pub fn get_framework() -> StandardFramework {
    StandardFramework::new().group(&GENERAL_GROUP)
}

pub async fn register_slash_commands(ctx: &Context, guild_id: GuildId) -> Result<(), serenity::Error> {
    guild_id
        .set_commands(
            &ctx.http,
            vec![
                CreateCommand::new("link")
                    .description("Link your GitHub or GitLab account (shows your CRA identity when a CRA is active)")
                    .add_option(
                        CreateCommandOption::new(
                            CommandOptionType::String,
                            "github",
                            "Your GitHub username, or gitlab:<username>",
                        )
                        .required(true),
                    ),
                CreateCommand::new("stats")
                    .description("View your contribution statistics"),
                CreateCommand::new("leaderboard")
                    .description("View the top contributors leaderboard"),
                CreateCommand::new("sync")
                    .description("Sync Discord roles with merit points (Admin only)"),
                CreateCommand::new("payout-setup")
                    .description("Get your private Stripe link to receive contribution payouts"),
                CreateCommand::new("payouts")
                    .description("List contribution payouts pending approval (Admin only)"),
            ],
        )
        .await?;

    Ok(())
}
