use sqlx::SqlitePool;

/// Atomically claim a key. Returns true the first time, false if it was already processed.
pub async fn claim(pool: &SqlitePool, key: &str) -> Result<bool, sqlx::Error> {
    let res = sqlx::query("INSERT OR IGNORE INTO processed_webhook_events (event_key) VALUES (?)")
        .bind(key)
        .execute(pool)
        .await?;
    Ok(res.rows_affected() == 1)
}

/// Release a claim (used when processing failed, so a redelivery can retry).
pub async fn release(pool: &SqlitePool, key: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM processed_webhook_events WHERE event_key = ?")
        .bind(key)
        .execute(pool)
        .await?;
    Ok(())
}