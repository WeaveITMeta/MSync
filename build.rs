use std::env;

fn main() {
    // Set build-time environment variables
    println!(
        "cargo:rustc-env=BUILD_TIMESTAMP={}",
        chrono::Utc::now().to_rfc3339()
    );

    // Re-run build script if these files change
    println!("cargo:rerun-if-changed=migrations/");
    println!("cargo:rerun-if-changed=.env");
    println!("cargo:rerun-if-env-changed=SQLX_OFFLINE");

    // SQLx offline mode support
    if env::var("SQLX_OFFLINE").is_ok() {
        println!("cargo:rustc-env=SQLX_OFFLINE=true");
    }
}
