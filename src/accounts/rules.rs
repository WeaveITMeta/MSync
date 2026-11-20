//! The pure rules of accounts and sign-in (hierarchy doc 6.2), ported from the Weave
//! reference as rules, not code: the linking decision, unlinking, email normalization, one-time
//! token states, rate decisions and token hashing. Every branch is unit tested here.

use sha2::{Digest, Sha256};

use super::mutant;

/// Where an identity comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Github,
    Gitlab,
    Discord,
    Slack,
    Google,
    Email,
}

impl Provider {
    pub const ALL: [Provider; 6] =
        [Provider::Github, Provider::Gitlab, Provider::Discord, Provider::Slack, Provider::Google, Provider::Email];

    /// The OAuth providers (everything but email links).
    pub const OAUTH: [Provider; 5] = [Provider::Github, Provider::Gitlab, Provider::Discord, Provider::Slack, Provider::Google];

    pub fn as_str(self) -> &'static str {
        match self {
            Provider::Github => "github",
            Provider::Gitlab => "gitlab",
            Provider::Discord => "discord",
            Provider::Slack => "slack",
            Provider::Google => "google",
            Provider::Email => "email",
        }
    }

    pub fn parse(s: &str) -> Option<Provider> {
        Provider::ALL.into_iter().find(|p| p.as_str() == s)
    }

    pub fn label(self) -> &'static str {
        match self {
            Provider::Github => "GitHub",
            Provider::Gitlab => "GitLab",
            Provider::Discord => "Discord",
            Provider::Slack => "Slack",
            Provider::Google => "Google",
            Provider::Email => "email",
        }
    }

    /// At most one identity of this provider per account (these carry merit, payout and
    /// chat attribution). Google and email may have several.
    pub fn one_per_account(self) -> bool {
        matches!(self, Provider::Github | Provider::Gitlab | Provider::Discord | Provider::Slack)
    }
}

// ------------------------------------------------------------------ linking

/// Everything the linking rule looks at, gathered by the store first.
#[derive(Clone, Copy, Debug)]
pub struct Facts<'a> {
    pub provider: Provider,
    /// The address the identity carries, if any.
    pub email: Option<&'a str>,
    /// True only when the provider itself says the address is verified (or an email link was
    /// completed for it). Discord and Slack send `verified` / `email_verified`; when they do
    /// not say so, this is false.
    pub email_verified: bool,
    /// The account that already holds this exact identity (provider and subject), if any.
    pub owner: Option<&'a str>,
    /// An explicit link from a signed-in session: that account's id. Never from a query string.
    pub link_to: Option<&'a str>,
    /// `link_to` already holds an identity of this provider.
    pub link_to_has_provider: bool,
    /// Distinct accounts with an identity holding the verified address. Empty without one.
    pub email_owners: &'a [String],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    /// The identity is already this account's: sign in (on an explicit link, nothing to do).
    SignIn(String),
    /// Add the identity to this account. `auto` is a match on a verified email.
    AttachTo { account: String, auto: bool },
    /// A new account with this identity as its first.
    CreateAccount,
    Refuse(Refusal),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The identity belongs to a different account. Never merged.
    OtherAccount,
    /// The account already has an identity of this provider.
    SecondOfProvider,
}

impl Refusal {
    pub fn code(self) -> &'static str {
        match self {
            Refusal::OtherAccount => "other_account",
            Refusal::SecondOfProvider => "second_of_provider",
        }
    }
}

/// The linking rule. Pure, so every branch is tested.
///
/// - Explicit link (signed in): attach to that account; refuse an identity of a different
///   account, and a second identity of a one-per-account provider.
/// - Otherwise an identity that exists signs in to its account.
/// - A new identity with a provider-verified address attaches to the one account that already
///   has an identity with that verified address. None, or more than one: a new account.
/// - Nothing is ever linked on an unverified address, whatever the provider.
pub fn decide(f: &Facts<'_>) -> Decision {
    if let Some(me) = f.link_to {
        return match f.owner {
            Some(o) if o == me => Decision::SignIn(me.to_string()),
            Some(_) if !mutant::on("decide_link_takes_owned") => Decision::Refuse(Refusal::OtherAccount),
            _ if f.provider.one_per_account() && f.link_to_has_provider && !mutant::on("decide_second_provider") => {
                Decision::Refuse(Refusal::SecondOfProvider)
            }
            _ => Decision::AttachTo { account: me.to_string(), auto: false },
        };
    }
    if let Some(o) = f.owner {
        if !mutant::on("decide_existing_creates") {
            return Decision::SignIn(o.to_string());
        }
    }
    let verified = f.email_verified || mutant::on("decide_ignore_verified");
    match (f.email, verified, f.email_owners) {
        (Some(_), true, [only]) => Decision::AttachTo { account: only.clone(), auto: true },
        (Some(_), true, [first, ..]) if mutant::on("decide_ambiguous_first") => {
            Decision::AttachTo { account: first.clone(), auto: true }
        }
        _ => Decision::CreateAccount,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnlinkRefusal {
    /// Not one of this account's identities.
    NotYours,
    /// The last way in: unlinking it would lock the account.
    LastOne,
}

impl UnlinkRefusal {
    pub fn code(self) -> &'static str {
        match self {
            UnlinkRefusal::NotYours => "unknown",
            UnlinkRefusal::LastOne => "last",
        }
    }
}

/// `owned` lists the account's identity ids.
pub fn can_unlink(owned: &[String], target: &str) -> Result<(), UnlinkRefusal> {
    if !owned.iter().any(|id| id == target) {
        return Err(UnlinkRefusal::NotYours);
    }
    let min = if mutant::on("unlink_last") { 1 } else { 2 };
    if owned.len() < min {
        return Err(UnlinkRefusal::LastOne);
    }
    Ok(())
}

// ------------------------------------------------------------------- email

/// RFC 5321's path limit.
pub const EMAIL_MAX_LEN: usize = 254;

/// Trim, lowercase, and refuse anything that is not one plain address. The result is an email
/// identity's `subject`.
pub fn normalize_email(raw: &str) -> Option<String> {
    let t = raw.trim();
    if t.is_empty() || t.len() > EMAIL_MAX_LEN {
        return None;
    }
    let (local, domain) = t.rsplit_once('@')?;
    let local = local.to_ascii_lowercase();
    let domain = domain.to_ascii_lowercase();
    let atext = |c: char| c.is_ascii_alphanumeric() || "!#$%&'*+/=?^_`{|}~-.".contains(c);
    if local.is_empty()
        || local.len() > 64
        || !local.chars().all(atext)
        || local.starts_with('.')
        || local.ends_with('.')
        || local.contains("..")
    {
        return None;
    }
    let labels: Vec<&str> = domain.split('.').collect();
    if labels.len() < 2 {
        return None;
    }
    for l in &labels {
        if l.is_empty()
            || l.len() > 63
            || l.starts_with('-')
            || l.ends_with('-')
            || !l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        {
            return None;
        }
    }
    Some(format!("{local}@{domain}"))
}

/// `jane.doe@example.com` -> `j***e@example.com`; `jo@x.io` -> `j***@x.io`.
pub fn mask_email(email: &str) -> String {
    let Some((local, domain)) = email.rsplit_once('@') else {
        return "***".to_string();
    };
    let chars: Vec<char> = local.chars().collect();
    match chars.len() {
        0 => format!("***@{domain}"),
        1 | 2 => format!("{}***@{domain}", chars[0]),
        n => format!("{}***{}@{domain}", chars[0], chars[n - 1]),
    }
}

// ------------------------------------------------------- one-time tokens

/// An email link lasts this long.
pub const EMAIL_TOKEN_TTL_SECS: i64 = 15 * 60;
/// An OAuth sign-in attempt lasts this long.
pub const LOGIN_STATE_TTL_SECS: i64 = 10 * 60;
/// A browser session lasts this long.
pub const SESSION_TTL_SECS: i64 = 30 * 24 * 3600;
/// base64url of 32 bytes, unpadded.
pub const TOKEN_LEN: usize = 43;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenState {
    Valid,
    Expired,
    Used,
}

/// Used beats expired: a link that was used says so even after it lapses. Expiry is exclusive.
pub fn token_state(expires_at: i64, used_at: Option<i64>, now: i64) -> TokenState {
    if used_at.is_some() {
        TokenState::Used
    } else if expires_at <= now {
        TokenState::Expired
    } else {
        TokenState::Valid
    }
}

/// 43 base64url characters, or it never reaches the database.
pub fn token_shape_ok(t: &str) -> bool {
    t.len() == TOKEN_LEN && t.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

pub fn sha256_hex(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}

/// What the database stores for a session, email or state token.
pub fn token_hash(token: &str) -> String {
    sha256_hex(token)
}

/// 32 random bytes as unpadded base64url (43 characters).
pub fn random_token() -> Result<String, getrandom::Error> {
    use base64::Engine;
    let mut b = [0u8; 32];
    getrandom::fill(&mut b)?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b))
}

/// `<prefix>` + 24 random hex characters.
pub fn random_id(prefix: &str) -> Result<String, getrandom::Error> {
    let mut b = [0u8; 12];
    getrandom::fill(&mut b)?;
    Ok(format!("{prefix}{}", hex::encode(b)))
}

/// The per-session secret a "link another identity" URL must carry. Derived from the raw session
/// token, which only the browser's cookie holds (the database has its plain SHA-256, a different
/// value), so no other site can start a link flow for someone else's session.
pub fn link_nonce(session_token: &str) -> String {
    sha256_hex(&format!("msync-link-v1:{session_token}"))[..32].to_string()
}

/// The consent form's anti-forgery value, derived the same way.
pub fn consent_nonce(session_token: &str) -> String {
    sha256_hex(&format!("msync-consent-v1:{session_token}"))[..32].to_string()
}

/// Rate-limit key for an address: its hash, so the counter table holds no addresses.
pub fn email_rate_key(normalized: &str) -> String {
    format!("e:{}", &sha256_hex(normalized)[..32])
}

// -------------------------------------------------------------- rate limits

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RateLimit {
    pub max: u32,
    pub window_secs: i64,
}

pub const PER_EMAIL: RateLimit = RateLimit { max: 3, window_secs: 15 * 60 };
pub const PER_IP: RateLimit = RateLimit { max: 10, window_secs: 60 * 60 };

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RateDecision {
    Allowed,
    TooManyFromIp,
    TooManyForEmail,
}

/// The counts INCLUDE this request (the store increments first, then asks).
pub fn rate_decision(ip_count: u32, email_count: u32) -> RateDecision {
    let (ip_max, email_max) = if mutant::on("rate_unlimited") { (u32::MAX, u32::MAX) } else { (PER_IP.max, PER_EMAIL.max) };
    if ip_count > ip_max {
        RateDecision::TooManyFromIp
    } else if email_count > email_max {
        RateDecision::TooManyForEmail
    } else {
        RateDecision::Allowed
    }
}

/// A same-site path to return to after sign-in: starts with one `/`, no scheme, no `//`, no
/// backslash, no control characters. Anything else becomes `/account`.
pub fn safe_next(next: Option<&str>) -> String {
    match next {
        Some(n)
            if n.starts_with('/')
                && !n.starts_with("//")
                && !n.contains('\\')
                && !n.chars().any(|c| c.is_control())
                && n.len() <= 2048 =>
        {
            n.to_string()
        }
        _ => "/account".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Provider::*;

    fn facts<'a>(provider: Provider, owners: &'a [String]) -> Facts<'a> {
        Facts {
            provider,
            email: None,
            email_verified: false,
            owner: None,
            link_to: None,
            link_to_has_provider: false,
            email_owners: owners,
        }
    }

    fn verified<'a>(f: Facts<'a>, email: &'a str) -> Facts<'a> {
        Facts { email: Some(email), email_verified: true, ..f }
    }

    fn one(a: &str) -> Vec<String> {
        vec![a.to_string()]
    }

    #[test]
    fn existing_identity_signs_in_to_its_account() {
        for p in Provider::ALL {
            let f = Facts { owner: Some("acc_a"), ..facts(p, &[]) };
            assert_eq!(decide(&f), Decision::SignIn("acc_a".into()), "{p:?}");
            // Even when a verified email points elsewhere: the identity wins.
            let others = one("acc_b");
            let f = verified(Facts { owner: Some("acc_a"), ..facts(p, &others) }, "a@x.io");
            assert_eq!(decide(&f), Decision::SignIn("acc_a".into()), "{p:?}");
        }
    }

    #[test]
    fn a_verified_email_attaches_to_exactly_one_account() {
        let owners = one("acc_a");
        for p in Provider::ALL {
            let f = verified(facts(p, &owners), "jo@x.io");
            assert_eq!(decide(&f), Decision::AttachTo { account: "acc_a".into(), auto: true }, "{p:?}");
        }
    }

    #[test]
    fn never_auto_links_on_an_unverified_email() {
        let owners = one("acc_a");
        for p in Provider::ALL {
            // The provider sent an address but did not call it verified.
            let f = Facts { email: Some("jo@x.io"), email_verified: false, ..facts(p, &owners) };
            assert_eq!(decide(&f), Decision::CreateAccount, "{p:?}");
            // No address at all.
            assert_eq!(decide(&facts(p, &owners)), Decision::CreateAccount, "{p:?}");
        }
    }

    #[test]
    fn discord_and_slack_link_by_email_only_when_the_provider_marks_it_verified() {
        let owners = one("acc_a");
        for p in [Discord, Slack] {
            let unverified = Facts { email: Some("jo@x.io"), email_verified: false, ..facts(p, &owners) };
            assert_eq!(decide(&unverified), Decision::CreateAccount, "{p:?}");
            let marked = verified(facts(p, &owners), "jo@x.io");
            assert_eq!(decide(&marked), Decision::AttachTo { account: "acc_a".into(), auto: true }, "{p:?}");
        }
    }

    #[test]
    fn ambiguous_or_unknown_email_creates_an_account() {
        let two = vec!["acc_a".to_string(), "acc_b".to_string()];
        for p in Provider::ALL {
            assert_eq!(decide(&verified(facts(p, &two), "jo@x.io")), Decision::CreateAccount, "{p:?}");
            assert_eq!(decide(&verified(facts(p, &[]), "jo@x.io")), Decision::CreateAccount, "{p:?}");
        }
    }

    #[test]
    fn explicit_link_attaches_to_the_signed_in_account() {
        for p in Provider::ALL {
            let f = Facts { link_to: Some("acc_me"), ..facts(p, &[]) };
            assert_eq!(decide(&f), Decision::AttachTo { account: "acc_me".into(), auto: false }, "{p:?}");
            // An explicit link ignores email matches elsewhere.
            let others = one("acc_other");
            let f = verified(Facts { link_to: Some("acc_me"), ..facts(p, &others) }, "jo@x.io");
            assert_eq!(decide(&f), Decision::AttachTo { account: "acc_me".into(), auto: false }, "{p:?}");
        }
    }

    #[test]
    fn explicit_link_refuses_an_identity_of_another_account() {
        for p in Provider::ALL {
            let f = Facts { link_to: Some("acc_me"), owner: Some("acc_other"), ..facts(p, &[]) };
            assert_eq!(decide(&f), Decision::Refuse(Refusal::OtherAccount), "{p:?}");
        }
    }

    #[test]
    fn explicit_link_of_an_own_identity_is_a_no_op() {
        let f = Facts { link_to: Some("acc_me"), owner: Some("acc_me"), link_to_has_provider: true, ..facts(Github, &[]) };
        assert_eq!(decide(&f), Decision::SignIn("acc_me".into()));
    }

    #[test]
    fn one_code_host_and_chat_identity_per_provider_per_account() {
        for p in [Github, Gitlab, Discord, Slack] {
            let f = Facts { link_to: Some("acc_me"), link_to_has_provider: true, ..facts(p, &[]) };
            assert_eq!(decide(&f), Decision::Refuse(Refusal::SecondOfProvider), "{p:?}");
        }
        // A second Google or email address is fine.
        for p in [Google, Email] {
            let f = Facts { link_to: Some("acc_me"), link_to_has_provider: true, ..facts(p, &[]) };
            assert_eq!(decide(&f), Decision::AttachTo { account: "acc_me".into(), auto: false }, "{p:?}");
        }
    }

    #[test]
    fn unlink_needs_another_way_in() {
        let two = vec!["idn_a".to_string(), "idn_b".to_string()];
        assert_eq!(can_unlink(&two, "idn_a"), Ok(()));
        assert_eq!(can_unlink(&two, "idn_zzz"), Err(UnlinkRefusal::NotYours));
        let one = vec!["idn_a".to_string()];
        assert_eq!(can_unlink(&one, "idn_a"), Err(UnlinkRefusal::LastOne));
        assert_eq!(can_unlink(&[], "idn_a"), Err(UnlinkRefusal::NotYours));
    }

    #[test]
    fn normalizes_addresses() {
        assert_eq!(normalize_email("  Jo.Doe@Example.COM \n").as_deref(), Some("jo.doe@example.com"));
        assert_eq!(normalize_email("a+tag@sub.example.io").as_deref(), Some("a+tag@sub.example.io"));
        for bad in [
            "",
            "   ",
            "no-at-sign",
            "a@b",
            "a@@b.io",
            "a b@c.io",
            "a@b.io\r\nBcc: x@y.io",
            "\u{e9}@b.io",
            ".a@b.io",
            "a..b@c.io",
            "a@-b.io",
            "a@b..io",
            "Jo <jo@x.io>",
        ] {
            assert_eq!(normalize_email(bad), None, "{bad:?}");
        }
        let long = format!("{}@example.com", "a".repeat(250));
        assert_eq!(normalize_email(&long), None, "over 254");
    }

    #[test]
    fn masks_addresses() {
        assert_eq!(mask_email("jane.doe@example.com"), "j***e@example.com");
        assert_eq!(mask_email("jo@x.io"), "j***@x.io");
        assert_eq!(mask_email("j@x.io"), "j***@x.io");
        assert_eq!(mask_email("nonsense"), "***");
    }

    #[test]
    fn tokens_hash_and_expire() {
        assert_eq!(token_hash("abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert_ne!(token_hash("abc"), token_hash("abd"));
        let now = 1_000_000;
        assert_eq!(token_state(now + 1, None, now), TokenState::Valid);
        assert_eq!(token_state(now, None, now), TokenState::Expired, "expiry is exclusive");
        assert_eq!(token_state(now - 1, None, now), TokenState::Expired);
        assert_eq!(token_state(now + 1, Some(now - 5), now), TokenState::Used);
        assert_eq!(token_state(now - 1, Some(now - 5), now), TokenState::Used);
        assert_eq!(EMAIL_TOKEN_TTL_SECS, 900);

        let t = random_token().unwrap();
        assert!(token_shape_ok(&t), "{t}");
        assert_ne!(t, random_token().unwrap());
        assert!(!token_shape_ok(&"A".repeat(42)));
        assert!(!token_shape_ok(&"A".repeat(44)));
        assert!(!token_shape_ok(&format!("{}=", "A".repeat(42))));
        let id = random_id("acc_").unwrap();
        assert!(id.starts_with("acc_") && id.len() == 28);
    }

    #[test]
    fn link_nonce_is_per_session_and_not_the_stored_hash() {
        let a = link_nonce("token-a");
        assert_eq!(a.len(), 32);
        assert_eq!(a, link_nonce("token-a"));
        assert_ne!(a, link_nonce("token-b"));
        assert!(!sha256_hex("token-a").starts_with(&a));
        assert_ne!(a, consent_nonce("token-a"));
    }

    #[test]
    fn rate_limits_refuse_past_the_limit() {
        assert_eq!(rate_decision(1, 1), RateDecision::Allowed);
        assert_eq!(rate_decision(10, 3), RateDecision::Allowed);
        assert_eq!(rate_decision(10, 4), RateDecision::TooManyForEmail);
        assert_eq!(rate_decision(11, 1), RateDecision::TooManyFromIp);
        assert_eq!(rate_decision(11, 4), RateDecision::TooManyFromIp);
    }

    #[test]
    fn next_paths_stay_on_this_site() {
        assert_eq!(safe_next(Some("/oauth/authorize?client_id=x")), "/oauth/authorize?client_id=x");
        for bad in ["https://evil.example", "//evil.example", "/\\evil", "evil", "/a\nb", ""] {
            assert_eq!(safe_next(Some(bad)), "/account", "{bad:?}");
        }
        assert_eq!(safe_next(None), "/account");
    }
}
