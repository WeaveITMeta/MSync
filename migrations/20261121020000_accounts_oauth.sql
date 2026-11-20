-- Accounts, sign-in, and MSync as an OAuth 2.0 / OpenID Connect provider
-- (contract 12.3 and 13.4, hierarchy doc section 6).
-- Additive only: every statement creates a new table or index. No existing table or column
-- is changed. Times are Unix seconds (UTC).

-- ------------------------------------------------------------------ accounts

-- The root account: one per person. Rank and permissions never live here; they come from
-- the active CRA snapshot through the account's verified identities.
CREATE TABLE IF NOT EXISTS accounts (
    id TEXT PRIMARY KEY,                       -- 'acc_' + 24 hex
    display_name TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'suspended', 'banned')),
    created_at INTEGER NOT NULL,
    suspended_at INTEGER,
    suspended_reason TEXT
);

-- Each way into an account. `subject` is the provider's stable user id, never a username:
-- GitHub and GitLab numeric ids, the Discord snowflake, '<team>:<user>' for Slack, Google's
-- `sub`, and the normalized address for email. `email` is set only when the provider says it
-- is verified (or an email link was completed for it). `handle` is for display only.
CREATE TABLE IF NOT EXISTS identities (
    id TEXT PRIMARY KEY,                       -- 'idn_' + 24 hex
    account_id TEXT NOT NULL REFERENCES accounts (id) ON DELETE CASCADE,
    provider TEXT NOT NULL CHECK (provider IN ('github', 'gitlab', 'discord', 'slack', 'google', 'email')),
    subject TEXT NOT NULL,
    email TEXT,
    handle TEXT,
    verified_at INTEGER,
    created_at INTEGER NOT NULL,
    last_login_at INTEGER NOT NULL,
    UNIQUE (provider, subject)
);
CREATE INDEX IF NOT EXISTS idx_identities_account ON identities (account_id);
CREATE INDEX IF NOT EXISTS idx_identities_email ON identities (email) WHERE email IS NOT NULL;
-- At most one GitHub, GitLab, Discord and Slack identity per account: these carry merit,
-- payout and chat attribution.
CREATE UNIQUE INDEX IF NOT EXISTS idx_identities_one_github ON identities (account_id) WHERE provider = 'github';
CREATE UNIQUE INDEX IF NOT EXISTS idx_identities_one_gitlab ON identities (account_id) WHERE provider = 'gitlab';
CREATE UNIQUE INDEX IF NOT EXISTS idx_identities_one_discord ON identities (account_id) WHERE provider = 'discord';
CREATE UNIQUE INDEX IF NOT EXISTS idx_identities_one_slack ON identities (account_id) WHERE provider = 'slack';

-- Every link, unlink and refusal, shown to the member on GET /account.
CREATE TABLE IF NOT EXISTS identity_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    account_id TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('created', 'linked', 'auto_linked', 'unlinked', 'refused')),
    provider TEXT NOT NULL,
    label TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_identity_events_account ON identity_events (account_id, created_at);

-- GitHub and GitLab logins proven by OAuth, for payout eligibility and the CRA member match.
-- One row per login: the identity that most recently proved it. A login renamed away and
-- reclaimed by someone else moves to whoever proves it next.
CREATE TABLE IF NOT EXISTS account_verified_logins (
    provider TEXT NOT NULL CHECK (provider IN ('github', 'gitlab')),
    login TEXT NOT NULL,                       -- lowercased
    account_id TEXT NOT NULL,
    identity_id TEXT NOT NULL,
    verified_at INTEGER NOT NULL,
    PRIMARY KEY (provider, login)
);
CREATE INDEX IF NOT EXISTS idx_account_verified_logins_identity ON account_verified_logins (identity_id);

-- Browser sessions. `id` is the SHA-256 hex of the cookie token; the token itself is never
-- stored. Checked against this table on every request, so revocation is immediate.
CREATE TABLE IF NOT EXISTS account_sessions (
    id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL,
    identity_id TEXT,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    last_used_at INTEGER NOT NULL,
    revoked_at INTEGER,
    ip TEXT,
    user_agent TEXT
);
CREATE INDEX IF NOT EXISTS idx_account_sessions_account ON account_sessions (account_id);

-- One OAuth sign-in attempt. `state_hash` is SHA-256 of the `state` sent to the provider;
-- `browser_hash` is SHA-256 of the random value in the browser's sign-in cookie, so a callback
-- only completes in the browser that started it. Single use.
CREATE TABLE IF NOT EXISTS oauth_login_states (
    state_hash TEXT PRIMARY KEY,
    browser_hash TEXT NOT NULL,
    provider TEXT NOT NULL,
    pkce_verifier TEXT NOT NULL,
    link_account_id TEXT,
    next_path TEXT,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    used_at INTEGER
);

-- One-time email sign-in links: SHA-256 of the token, 15 minutes, single use.
CREATE TABLE IF NOT EXISTS email_tokens (
    token_hash TEXT PRIMARY KEY,
    email TEXT NOT NULL,
    link_account_id TEXT,
    ip TEXT,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    used_at INTEGER
);
CREATE INDEX IF NOT EXISTS idx_email_tokens_expires ON email_tokens (expires_at);

-- Fixed-window counters: 'e:' + hash of an address, 'ip:' + client address.
CREATE TABLE IF NOT EXISTS auth_rate (
    key TEXT PRIMARY KEY,
    count INTEGER NOT NULL,
    reset_at INTEGER NOT NULL
);

-- ------------------------------------------------------- authorization server

-- Signing keys (ES256). The private key is PKCS#8 sealed with AES-256-GCM under
-- MSYNC_KEY_ENCRYPTION_KEY (nonce || ciphertext || tag, base64), with the key id as AAD.
-- The newest unretired key signs; a retired key stays in the JWKS until `publish_until`.
CREATE TABLE IF NOT EXISTS oauth_signing_keys (
    kid TEXT PRIMARY KEY,
    alg TEXT NOT NULL,
    public_key TEXT NOT NULL,                  -- base64url uncompressed P-256 point
    private_key_sealed TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    retired_at INTEGER,
    publish_until INTEGER
);

-- Registered platforms (relying parties). `secret_hash` is NULL for a public client.
CREATE TABLE IF NOT EXISTS oauth_clients (
    client_id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    secret_hash TEXT,
    redirect_uris TEXT NOT NULL,               -- JSON array, exact match
    allowed_scopes TEXT NOT NULL,              -- JSON array
    allowed_claims TEXT NOT NULL,              -- JSON array
    consumer_key TEXT,                         -- the CRA [[consumer]] key, if any
    pairwise_salt TEXT NOT NULL,               -- per client, never shown
    created_by TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    revoked_at INTEGER
);

-- Consent remembered per account and client.
CREATE TABLE IF NOT EXISTS oauth_consents (
    account_id TEXT NOT NULL,
    client_id TEXT NOT NULL,
    scopes TEXT NOT NULL,                      -- JSON array
    granted_at INTEGER NOT NULL,
    PRIMARY KEY (account_id, client_id)
);

-- Authorization codes: SHA-256 of the code, 60 seconds, single use, bound to the client,
-- the redirect URI and the PKCE challenge. `grant_id` is the token family it was redeemed for.
CREATE TABLE IF NOT EXISTS oauth_codes (
    code_hash TEXT PRIMARY KEY,
    client_id TEXT NOT NULL,
    account_id TEXT NOT NULL,
    redirect_uri TEXT NOT NULL,
    scopes TEXT NOT NULL,
    nonce TEXT,
    code_challenge TEXT NOT NULL,
    auth_time INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    used_at INTEGER,
    grant_id TEXT
);

-- A token family: every access and refresh token issued from one authorization code.
CREATE TABLE IF NOT EXISTS oauth_grants (
    id TEXT PRIMARY KEY,                       -- 'grt_' + 24 hex
    client_id TEXT NOT NULL,
    account_id TEXT NOT NULL,
    scopes TEXT NOT NULL,
    auth_time INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    revoked_at INTEGER,
    revoked_reason TEXT
);
CREATE INDEX IF NOT EXISTS idx_oauth_grants_account ON oauth_grants (account_id);

-- Refresh tokens, SHA-256 hashed. Rotated on every use; presenting a used one revokes the family.
CREATE TABLE IF NOT EXISTS oauth_refresh_tokens (
    token_hash TEXT PRIMARY KEY,
    grant_id TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    used_at INTEGER,
    revoked_at INTEGER
);
CREATE INDEX IF NOT EXISTS idx_oauth_refresh_tokens_grant ON oauth_refresh_tokens (grant_id);
