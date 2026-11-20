use dotenv::dotenv;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use msync::cli::{self, Command};

#[actix_web::main]
async fn main() {
    // Load environment variables
    dotenv().ok();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = match cli::parse(&args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: {e}\n");
            eprint!("{}", cli::USAGE);
            std::process::exit(2);
        }
    };

    if cmd != Command::Serve {
        std::process::exit(cli::run(cmd).await);
    }

    // Initialize tracing
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "msync=debug,actix_web=info".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    msync::routes::mark_started();
    if let Err(e) = msync::server::serve().await {
        tracing::error!("MSync failed: {e:#}");
        eprintln!("Error: {e:#}");
        std::process::exit(1);
    }
}
