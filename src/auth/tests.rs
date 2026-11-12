use super::tokens::{format_db_time, hash_token, parse_token};
use super::*;
use chrono::{Duration, Utc};
use sqlx::sqlite::{SqlitePool, SqlitePoolOptions};

async fn pool() -> SqlitePool {
    let pool = SqlitePoolOptions::new().max_connections(1).connect("sqlite::memory:").await.unwrap();
    sqlx::migrate!("./migrations").run(&pool).await.unwrap();
    pool
}

fn owners() -> EnvDirectory {
    EnvDirectory::new(vec!["owner1".to_string()])
}

fn new_token(member: &str, scopes: &[&str], payee: &[&str]) -> NewToken {
    NewToken {
        label: format!("{member} token"),
        member_id: member.into(),
        scopes: scopes.iter().map(|s| s.to_string()).collect(),
        payee_keys: payee.iter().map(|s| s.to_string()).collect(),
        expires_at: None,
    }
}

async fn mint(pool: &SqlitePool, member: &str, scopes: &[&str], payee: &[&str]) -> CreatedToken {
    create_token(pool, &owners(), TokenCreator::Bootstrap, new_token(member, scopes, payee), Utc::now())
        .await
        .unwrap()
}

#[tokio::test]
async fn token_is_shown_once_stored_hashed_and_authenticates() {
    let pool = pool().await;
    let t = mint(&pool, "bob", &[scopes::PAYOUTS_READ], &["Bob-GH"]).await;
    let (id, secret) = parse_token(&t.token).expect("dsync_<id>_<secret>");
    assert_eq!(id, t.id);
    assert_eq!(secret.len(), 64, "32 random bytes, hex");

    let stored: (String, String) = sqlx::query_as("SELECT token_hash, payee_keys FROM api_tokens WHERE id = ?")
        .bind(&t.id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(stored.0, hash_token(&t.token));
    assert!(!stored.0.contains(secret), "secret never stored");
    assert_eq!(stored.1, r#"["bob-gh"]"#, "payee keys lowercased");

    let p = authenticate_token(&pool, &t.token, Utc::now()).await.unwrap();
    assert_eq!(p.member_id, "bob");
    assert_eq!(p.scopes, vec![scopes::PAYOUTS_READ.to_string()]);
    assert_eq!(p.payee_keys, vec!["bob-gh".to_string()]);
    assert!(p.require(scopes::PAYOUTS_READ).is_ok());
}

#[tokio::test]
async fn wrong_secret_is_refused() {
    let pool = pool().await;
    let t = mint(&pool, "bob", &[scopes::PAYOUTS_READ], &[]).await;
    let (id, _) = parse_token(&t.token).unwrap();
    let forged = format!("dsync_{id}_{}", "0".repeat(64));
    assert!(matches!(authenticate_token(&pool, &forged, Utc::now()).await, Err(AuthError::BadSecret)));
    // Malformed and unknown ids are refused too.
    assert!(matches!(authenticate_token(&pool, "dsync_nope", Utc::now()).await, Err(AuthError::Malformed)));
    let unknown = format!("dsync_{}_{}", "a".repeat(16), "0".repeat(64));
    assert!(matches!(authenticate_token(&pool, &unknown, Utc::now()).await, Err(AuthError::UnknownToken)));
}

#[tokio::test]
async fn revoked_token_is_refused() {
    let pool = pool().await;
    let t = mint(&pool, "bob", &[scopes::PAYOUTS_READ], &[]).await;
    assert!(authenticate_token(&pool, &t.token, Utc::now()).await.is_ok());
    revoke_token(&pool, &owners(), TokenCreator::Bootstrap, &t.id, Utc::now()).await.unwrap();
    assert!(matches!(authenticate_token(&pool, &t.token, Utc::now()).await, Err(AuthError::Revoked)));
}

#[tokio::test]
async fn expired_token_is_refused() {
    let pool = pool().await;
    let mut req = new_token("bob", &[scopes::PAYOUTS_READ], &[]);
    let now = Utc::now();
    req.expires_at = Some(now + Duration::hours(1));
    let t = create_token(&pool, &owners(), TokenCreator::Bootstrap, req, now).await.unwrap();
    assert!(authenticate_token(&pool, &t.token, now).await.is_ok());
    assert!(matches!(
        authenticate_token(&pool, &t.token, now + Duration::hours(2)).await,
        Err(AuthError::Expired)
    ));
    // An expiry already in the past at creation is refused.
    let mut past = new_token("bob", &[scopes::PAYOUTS_READ], &[]);
    past.expires_at = Some(now - Duration::seconds(1));
    assert!(matches!(
        create_token(&pool, &owners(), TokenCreator::Bootstrap, past, now).await,
        Err(TokenError::Invalid(_))
    ));
    // Unparseable expiry fails closed.
    sqlx::query("UPDATE api_tokens SET expires_at = 'garbage' WHERE id = ?").bind(&t.id).execute(&pool).await.unwrap();
    assert!(matches!(authenticate_token(&pool, &t.token, now).await, Err(AuthError::Expired)));
    let _ = format_db_time(now);
}

#[tokio::test]
async fn missing_scope_is_refused() {
    let pool = pool().await;
    let t = mint(&pool, "bob", &[scopes::PAYOUTS_READ], &[]).await;
    let p = authenticate_token(&pool, &t.token, Utc::now()).await.unwrap();
    assert!(matches!(p.require(scopes::PAYOUTS_APPROVE), Err(AuthError::MissingScope(_))));
    assert!(matches!(p.require(scopes::PAYOUTS_EXECUTE), Err(AuthError::MissingScope(_))));
    assert!(p.require(scopes::PAYOUTS_READ).is_ok());
}

#[tokio::test]
async fn token_creation_cannot_escalate() {
    let pool = pool().await;
    let dir = owners();
    // bob (unranked) holds tokens.manage + payouts.read
    let bob_t = mint(&pool, "bob", &[scopes::TOKENS_MANAGE, scopes::PAYOUTS_READ], &[]).await;
    let bob = authenticate_token(&pool, &bob_t.token, Utc::now()).await.unwrap();

    // own token with a subset of bob's scopes: allowed
    assert!(create_token(&pool, &dir, TokenCreator::Principal(&bob), new_token("bob", &[scopes::PAYOUTS_READ], &[]), Utc::now()).await.is_ok());
    // a scope bob does not hold: refused
    assert!(matches!(
        create_token(&pool, &dir, TokenCreator::Principal(&bob), new_token("bob", &[scopes::PAYOUTS_APPROVE], &[]), Utc::now()).await,
        Err(TokenError::Forbidden(_))
    ));
    // a token for another member (impersonation): refused for an unranked creator
    assert!(matches!(
        create_token(&pool, &dir, TokenCreator::Principal(&bob), new_token("owner1", &[scopes::PAYOUTS_READ], &[]), Utc::now()).await,
        Err(TokenError::Forbidden(_))
    ));
    // the owner outranks bob and may mint for him
    let own_t = mint(&pool, "owner1", &[scopes::TOKENS_MANAGE, scopes::PAYOUTS_READ], &[]).await;
    let owner = authenticate_token(&pool, &own_t.token, Utc::now()).await.unwrap();
    assert!(create_token(&pool, &dir, TokenCreator::Principal(&owner), new_token("bob", &[scopes::PAYOUTS_READ], &[]), Utc::now()).await.is_ok());
    // without tokens.manage: refused
    let carol_t = mint(&pool, "carol", &[scopes::PAYOUTS_READ], &[]).await;
    let carol = authenticate_token(&pool, &carol_t.token, Utc::now()).await.unwrap();
    assert!(matches!(
        create_token(&pool, &dir, TokenCreator::Principal(&carol), new_token("carol", &[scopes::PAYOUTS_READ], &[]), Utc::now()).await,
        Err(TokenError::Auth(AuthError::MissingScope(_)))
    ));
    // reserved and unknown
    for m in [LEGACY_ADMIN_MEMBER, "system:scheduler"] {
        assert!(matches!(
            create_token(&pool, &dir, TokenCreator::Bootstrap, new_token(m, &[scopes::PAYOUTS_READ], &[]), Utc::now()).await,
            Err(TokenError::Invalid(_))
        ));
    }
    assert!(matches!(
        create_token(&pool, &dir, TokenCreator::Bootstrap, new_token("x", &["payouts.everything"], &[]), Utc::now()).await,
        Err(TokenError::Invalid(_))
    ));
}

#[tokio::test]
async fn payee_keys_are_the_union_over_all_member_tokens() {
    let pool = pool().await;
    mint(&pool, "alice-m", &[scopes::PAYOUTS_READ], &["alice"]).await;
    let second = mint(&pool, "alice-m", &[scopes::PAYOUTS_APPROVE], &[]).await;
    revoke_token(&pool, &owners(), TokenCreator::Bootstrap, &second.id, Utc::now()).await.unwrap();
    assert_eq!(member_payee_keys(&pool, "alice-m").await.unwrap(), vec!["alice".to_string()]);
    assert!(is_payee(&pool, &owners(), "alice-m", "ALICE").await.unwrap());
    assert!(!is_payee(&pool, &owners(), "bob", "alice").await.unwrap());
}

#[test]
fn env_directory_ranks_owners_only() {
    let d = EnvDirectory::from_owner_list(" owner1 , ,owner2");
    assert_eq!(d.rank_of("owner1"), Some(OWNER_RANK));
    assert_eq!(d.rank_of("owner2"), Some(OWNER_RANK));
    assert_eq!(d.rank_of("bob"), None);
    assert_eq!(d.owner_count(), 2);
}
