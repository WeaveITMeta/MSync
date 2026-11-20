# Sign-in and "Sign in with MSync"

MSync has two sides here.

1. **Members sign in to MSync** with GitHub, GitLab, Discord, Slack, Google or a one-time email link. Each person has one root account with any number of linked identities.
2. **Platforms sign their users in with MSync.** MSync is an OAuth 2.0 authorization server and OpenID Connect provider. A platform such as Eustress Engine registers as a client, runs the authorization code flow with PKCE, and receives the member's standing in the CRA as claims.

Contract: `docs/CRA_PLATFORM.md` sections 12.3 and 13.4. Linking rules: `docs/HIERARCHY_AND_THREAT_MODEL.md` section 6.

## 1. The one rule to remember

Rank and permissions never live on an account or an identity. They live in the active CRA snapshot. An account is a member only through identities that a provider proved by OAuth: a GitHub or GitLab login listed in `members.toml`, or else a Discord or Slack id listed there. Linking another identity adds a way in. It never adds authority.

## 2. Enabling a sign-in provider

Every provider is off until both of its variables are set. The redirect URI is always exactly:

```
<MSYNC_PUBLIC_URL>/auth/<provider>/callback
```

With `MSYNC_PUBLIC_URL=https://msync.example.org`, GitHub's is `https://msync.example.org/auth/github/callback`. Register that exact string at the provider. MSync sends exactly that value, so any difference (a trailing slash, `http` instead of `https`) makes the provider refuse.

| Provider | Register at | What to create | Scopes MSync asks for | Variables |
|---|---|---|---|---|
| GitHub | github.com, Settings, Developer settings, OAuth Apps | An OAuth App. Authorization callback URL: the redirect URI above. | `read:user user:email` | `MSYNC_OAUTH_GITHUB_CLIENT_ID`, `MSYNC_OAUTH_GITHUB_CLIENT_SECRET` |
| GitLab | gitlab.com, User settings (or Admin area), Applications | An application, confidential, with the `read_user` scope. Redirect URI: the one above. | `read_user` | `MSYNC_OAUTH_GITLAB_CLIENT_ID`, `MSYNC_OAUTH_GITLAB_CLIENT_SECRET` |
| Discord | discord.com/developers/applications, your application, OAuth2 | Add the redirect URI under Redirects. Copy the client id and reset the client secret. | `identify email` | `MSYNC_OAUTH_DISCORD_CLIENT_ID`, `MSYNC_OAUTH_DISCORD_CLIENT_SECRET` |
| Slack | api.slack.com/apps, your app, OAuth & Permissions | Add the redirect URI under Redirect URLs. Add the user token scopes `openid`, `profile`, `email` ("Sign in with Slack"). The app from `slack/manifest.yaml` can carry this too. | `openid profile email` | `MSYNC_OAUTH_SLACK_CLIENT_ID`, `MSYNC_OAUTH_SLACK_CLIENT_SECRET` |
| Google | console.cloud.google.com, APIs and Services, Credentials | An OAuth client ID of type "Web application", with the redirect URI above as an authorized redirect URI. | `openid email profile` | `MSYNC_OAUTH_GOOGLE_CLIENT_ID`, `MSYNC_OAUTH_GOOGLE_CLIENT_SECRET` |

A self-hosted GitLab sets `MSYNC_OAUTH_GITLAB_AUTHORIZE_URL`, `MSYNC_OAUTH_GITLAB_TOKEN_URL` and `MSYNC_OAUTH_GITLAB_API_BASE` as well (for example `https://git.example.org/oauth/authorize`, `https://git.example.org/oauth/token`, `https://git.example.org/api/v4`). Every provider accepts the same three overrides. A login proven on a self-hosted GitLab is not treated as a gitlab.com login for payouts.

At startup MSync logs which providers are enabled. A request to `/auth/<provider>/start` or `/callback` for a provider that is not enabled answers 404 `provider_disabled`.

What each provider proves:

- **Subject.** The provider's stable user id, never a username: GitHub and GitLab numeric ids, the Discord snowflake, `<team>:<user>` for Slack, Google's `sub`. A renamed login is still the same identity.
- **Email.** Kept only when the provider says it is verified: a GitHub address marked `verified`, a GitLab account with `confirmed_at`, Discord's `verified: true`, Slack's and Google's `email_verified: true`. Anything else is ignored for linking.
- **Login.** For GitHub and GitLab, the login at sign-in is recorded as verified for the account (see section 6).

PKCE (S256) is sent to GitHub, GitLab, Discord and Google. Slack's OpenID Connect flow does not document PKCE, so for Slack the `state` binding below carries the protection.

### Email links

`POST /auth/email/start` with `{"email": "jo@example.org"}` creates a one-time link: stored as a SHA-256 hash, valid for 15 minutes, usable once. Limits: 3 links per address per 15 minutes and 10 per client address per hour. The link opens a confirm page; looking at the page never uses the token (mail scanners fetch links). The button on that page signs in.

MSync sends mail through the `MailSender` trait (`src/accounts/mod.rs`). The default implementation, `NoMailer`, sends nothing: it logs that no mailer is configured, with the address masked, and never logs the link. To send real mail, implement `MailSender` for your mail service and put it in `AccountsContext.mailer` in `src/main.rs`.

The per-IP limit uses the client address from `X-Forwarded-For` or `Forwarded`. Run MSync behind a proxy that overwrites those headers (the shipped `nginx.conf` does), or the per-IP limit can be dodged. The per-address limit holds either way.

## 3. Sign-in routes

| Route | What it does |
|---|---|
| `GET /auth/{provider}/start?next=/path` | Starts a sign-in. Sets a short-lived `msync_signin` cookie and redirects to the provider. `next` must be a path on this site. |
| `GET /auth/{provider}/start?link=<link_nonce>` | Starts linking another identity to the signed-in account. `link_nonce` comes from `GET /account`. |
| `GET /auth/{provider}/callback` | The provider returns here. Completes the sign-in and sets the session cookie. |
| `POST /auth/logout` | Ends this session. `?all=true` ends every session of the account. |
| `POST /auth/email/start`, `GET` and `POST /auth/email/verify` | Email links (above). |
| `GET /account` | JSON: the account, its identities (emails masked), its link history, the CRA member it is, its standing in the active CRA, and the `link_nonce`. |
| `POST /account/identities/{id}/unlink` | Unlinks one identity. Refused for the last one. |

How the callback is protected:

- `state` is 256 random bits, stored hashed, single use, valid 10 minutes, and bound to the browser that started the sign-in through the `msync_signin` cookie. A callback with a missing, wrong, used, expired or foreign `state`, or from another browser, is refused and creates nothing.
- A provider error (`error=` on the callback, or a failed code exchange) creates no account and no session.
- A link must finish in the same signed-in account that started it.

Sessions:

- The cookie is `msync_session`: `Secure; HttpOnly; SameSite=Lax; Path=/`, 30 days. The value is 256 random bits. The database keeps only its SHA-256.
- Every request checks the session in the database, so logout, "log out everywhere", suspension and unlinking the identity a session was opened with all take effect on the next request.
- Every state-changing cookie route (`POST /auth/logout`, `POST /auth/email/*`, `POST /account/identities/{id}/unlink`, `POST /oauth/authorize`) refuses a request whose `Origin` is not the origin of `MSYNC_PUBLIC_URL`, and refuses a request with no `Origin`.
- A session cookie never authorizes `/api/v1`. API bearer tokens (`msync_...`) never authorize the browser routes or `/oauth/authorize`.

## 4. Linking rules

`decide` in `src/accounts/rules.rs` is a pure function, tested branch by branch:

1. **Explicit link** from a signed-in session attaches to that account. It is refused if the identity already belongs to another account (accounts are never merged), and refused for a second GitHub, GitLab, Discord or Slack identity on one account.
2. **An existing identity** signs in to its account.
3. **A new identity with a provider-verified email** attaches to the one account that already has an identity holding that verified address. No such account, or more than one, creates a new account.
4. **Nothing is ever linked on an unverified address.** Discord and Slack identities link by email only when Discord or Slack marks the address verified.
5. **Unlinking the last identity is refused.**

Every link, automatic link, unlink and refusal is recorded and shown on `GET /account`.

## 5. Which member an account is

From the account's OAuth-proven identities only, in this order:

1. Its verified GitHub and GitLab logins, matched case-insensitively against `members.toml`.
2. If none of those is listed: its Discord id, and its Slack id when the Slack team is the organization's `[slack]` team.

If two different members match on the same level, the account is nobody (fail closed). An identity of another member linked later never changes who an account is, so it never adds that member's authority.

## 6. Verification flows outward

- A completed OAuth sign-in or link records a GitHub or GitLab login as verified for the account. `accounts::login_is_verified(pool, provider, login)` answers whether a login is proven by an active account; the payout eligibility check reads it.
- A chat binding made by typing `link github:<login>` in Discord or Slack is unverified. It becomes verified when one MSync account has proven both sides by OAuth: the Discord or Slack account, and the GitHub or GitLab login it is bound to. Proving only one side verifies nothing.
- `payout-setup` accepts a CRA-attested member, a verified binding, or a binding whose two sides one account has proven (even if the binding was made after the sign-in).

## 7. Sign in with MSync (for platforms)

### 7.1 Turn the provider on

Set a 32-byte key-encryption key, as 64 hex characters or base64:

```
# any of these prints a fresh key
python -c "import secrets; print(secrets.token_hex(32))"
pwsh -c "[Convert]::ToHexString([Security.Cryptography.RandomNumberGenerator]::GetBytes(32))"
```

```
MSYNC_PUBLIC_URL=https://msync.example.org
MSYNC_KEY_ENCRYPTION_KEY=<64 hex characters>
```

Without `MSYNC_KEY_ENCRYPTION_KEY` every `/oauth/*` endpoint and the discovery document answer 503. The key seals the signing key in the database (AES-256-GCM, with the key id as associated data). Keep it out of the database backups' reach: whoever holds both can sign tokens. Losing it means generating a new signing key, which logs every platform's users out at their next refresh.

`MSYNC_PUBLIC_URL` is the issuer (`iss`). It must be the exact public HTTPS origin platforms reach MSync on.

### 7.2 Register a platform

An owner registers each platform. Use the bootstrap `MSYNC_ADMIN_TOKEN`, or an API token with scope `oauth.clients` bound to an owner (rank 1000, from `MSYNC_OWNER_MEMBER_IDS`):

```
curl -s https://msync.example.org/api/v1/tokens \
  -H "Authorization: Bearer $MSYNC_ADMIN_TOKEN" -H "Content-Type: application/json" \
  -d '{"label": "client registration", "member_id": "github:example-founder", "scopes": ["oauth.clients"]}'
```

```
curl -s https://msync.example.org/api/v1/oauth/clients \
  -H "Authorization: Bearer $OWNER_TOKEN" -H "Content-Type: application/json" \
  -d '{
        "name": "Eustress Engine",
        "redirect_uris": ["https://eustress.example/auth/msync/callback"],
        "allowed_scopes": ["openid", "profile", "groups", "rank", "merit"],
        "allowed_claims": ["name", "handle", "groups", "titles", "rank", "merit"],
        "consumer_key": "eustress-site"
      }'
```

The answer holds `client_id` (`msc_...`) and `client_secret` (`mss_...`). The secret is shown once and stored as a hash. Store it in the platform's server configuration.

- `redirect_uris`: exact strings, compared byte for byte. `https://` only, except `http://localhost`, `http://127.0.0.1` and `http://[::1]` for development. No wildcards, no fragments. An authorization request with any other redirect URI gets an error page and is never redirected.
- `"public": true` registers a browser or desktop client with no secret.
- `allowed_scopes` must include `openid`. `allowed_claims` is the allowlist: a claim not listed is never sent to this client, whatever scopes it asks for.
- `consumer_key` ties the client to a CRA `[[consumer]]` entry. With a snapshot active it must name one.

`POST /api/v1/oauth/keys/rotate` (same authorization) starts a new signing key. The old key stays in the JWKS for an hour, longer than any token it signed lives.

### 7.3 Endpoints

| Endpoint | Purpose |
|---|---|
| `GET /.well-known/openid-configuration` | Discovery. |
| `GET /oauth/jwks` | Public signing keys (ES256, P-256). |
| `GET /oauth/authorize` | Authorization request. Needs a signed-in MSync browser session; shows a consent page the first time; consent is remembered per account and platform. |
| `POST /oauth/token` | `authorization_code` (PKCE S256 required) and `refresh_token`. Client authentication: `client_secret_basic`, `client_secret_post`, or `none` for a public client. |
| `GET` or `POST /oauth/userinfo` | Claims for a bearer access token. |
| `POST /oauth/revoke` | RFC 7009 revocation of a refresh or access token. Revokes the whole token family. |

Lifetimes: authorization codes 60 seconds and single use; access tokens and ID tokens 5 minutes; refresh tokens 30 days, rotated on every use.

### 7.4 A worked exchange with curl

1. Make a PKCE verifier and challenge:

```
VERIFIER=$(python -c "import secrets; print(secrets.token_urlsafe(32))")
CHALLENGE=$(python -c "import sys,hashlib,base64; print(base64.urlsafe_b64encode(hashlib.sha256(sys.argv[1].encode()).digest()).rstrip(b'=').decode())" "$VERIFIER")
```

2. Send the member's browser to:

```
https://msync.example.org/oauth/authorize?response_type=code
  &client_id=msc_0123456789abcdef01234567
  &redirect_uri=https%3A%2F%2Feustress.example%2Fauth%2Fmsync%2Fcallback
  &scope=openid%20profile%20groups%20rank
  &state=<random, kept in the platform's session>
  &nonce=<random, kept in the platform's session>
  &code_challenge=$CHALLENGE&code_challenge_method=S256
```

(one line in practice). If the member is not signed in to MSync, the page offers the enabled sign-in providers and returns here afterwards. After consent MSync redirects to:

```
https://eustress.example/auth/msync/callback?code=<code>&iss=https%3A%2F%2Fmsync.example.org&state=<state>
```

The platform checks that `state` is the one it sent and that `iss` is MSync.

3. Exchange the code within 60 seconds:

```
curl -s https://msync.example.org/oauth/token \
  -u "$CLIENT_ID:$CLIENT_SECRET" \
  -d grant_type=authorization_code \
  -d code="$CODE" \
  -d redirect_uri=https://eustress.example/auth/msync/callback \
  -d code_verifier="$VERIFIER"
```

```
{"access_token": "eyJ...", "token_type": "Bearer", "expires_in": 300,
 "refresh_token": "msr_...", "scope": "openid profile groups rank", "id_token": "eyJ..."}
```

4. Verify the ID token: the signature against `GET /oauth/jwks` (the `kid` in the header picks the key, algorithm ES256), `iss` is `MSYNC_PUBLIC_URL`, `aud` is your client id, `exp` is in the future, and `nonce` is the one you sent. Its payload looks like:

```
{"iss": "https://msync.example.org", "sub": "6f1c...e2", "aud": "msc_...", "azp": "msc_...",
 "iat": 1763600000, "exp": 1763600300, "auth_time": 1763600000, "nonce": "<nonce>",
 "name": "Bo Example", "handle": "bo-example",
 "groups": ["reviewer", "newcomer"], "titles": ["Reviewer", "Newcomer"], "rank": 40}
```

5. Read current claims, refresh, and revoke:

```
curl -s https://msync.example.org/oauth/userinfo -H "Authorization: Bearer $ACCESS_TOKEN"
curl -s https://msync.example.org/oauth/token -u "$CLIENT_ID:$CLIENT_SECRET" \
  -d grant_type=refresh_token -d refresh_token="$REFRESH_TOKEN"
curl -s https://msync.example.org/oauth/revoke -u "$CLIENT_ID:$CLIENT_SECRET" -d token="$REFRESH_TOKEN"
```

Every refresh returns a new refresh token; store it and drop the old one. Presenting an old refresh token again is treated as theft: the whole family is revoked and the member must sign in again. A code presented twice is refused and every token issued from it is revoked.

### 7.5 Scopes and claims

| Scope | Claims | Meaning |
|---|---|---|
| `openid` | `sub` | Required. `sub` is pairwise: the same member has a different `sub` at each platform, so two platforms cannot correlate people. It is stable for one platform. |
| `profile` | `name`, `handle` | The member's name and member id from `members.toml`. |
| `groups` | `groups`, `titles` | `groups`: the CRA role keys the member holds today, appointed positions first (highest rank first), then the earned tier. `titles`: the matching job titles. |
| `rank` | `rank` | The member's rank in the CRA. |
| `merit` | `merit` | The member's merit points. |

A claim is sent only when its scope was granted and the client's `allowed_claims` lists it.

### 7.6 How groups map to CRA roles

A group is a role key from `roles.toml`. In the Eustress Engine example CRA:

- A member granted `lead-maintainer` with 0 merit has `groups = ["lead-maintainer", "newcomer"]`.
- Bo Example (a `reviewer` grant) has `["reviewer", "newcomer"]`.
- Expired grants confer nothing and drop out of `groups` on the next token.
- A suspended or removed member has no groups and gets no token at all.

Only an active member of the active CRA gets tokens. A signed-in account that is not a member is refused at `/oauth/authorize` with `error=access_denied`.

### 7.7 What a platform does with them

Eustress Engine (source available) signs its users in with MSync instead of keeping its own passwords:

- It keys its user record on `sub`. It never sees the member's GitHub, Discord or Slack ids.
- It maps `groups` to its own permissions: for example `reviewer` may moderate the asset store, `maintainer` and `lead-maintainer` may publish releases. The mapping lives in the platform; the membership lives in the CRA.
- It reads fresh claims from `GET /oauth/userinfo` or by refreshing, because a CRA change takes effect at the next token. Access tokens last 5 minutes, so a demotion reaches the platform within minutes.
- When a member is suspended or removed in a newly activated snapshot, MSync revokes their refresh tokens at activation. The platform's next refresh fails with `invalid_grant`, and it should end the member's session there.

## 8. Variables

| Variable | Purpose |
|---|---|
| `MSYNC_PUBLIC_URL` | Public base URL: redirect URIs, the `Origin` check, the issuer. |
| `MSYNC_OAUTH_<PROVIDER>_CLIENT_ID`, `MSYNC_OAUTH_<PROVIDER>_CLIENT_SECRET` | Enable a sign-in provider (`GITHUB`, `GITLAB`, `DISCORD`, `SLACK`, `GOOGLE`). |
| `MSYNC_OAUTH_<PROVIDER>_AUTHORIZE_URL`, `_TOKEN_URL`, `_API_BASE` | Optional endpoint overrides (self-hosted GitLab). |
| `MSYNC_KEY_ENCRYPTION_KEY` | 32 bytes, hex or base64. Turns the OAuth provider on and seals its signing key. |
