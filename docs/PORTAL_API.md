# MSync portal sync API: reference client

The portal sync API lets a consumer server (a community portal, a website renderer) read one member's standing from MSync. It is server to server, signed with a shared secret, and read only. The rules are contract section 13.2 and the hierarchy and threat model document section 8; the code is `src/portal/mod.rs`.

## Endpoints

```
GET /api/v1/members/by-discord/{discord_user_id}?scopes=<list>
GET /api/v1/members/by-slack/{team_id}/{slack_user_id}?scopes=<list>
GET /api/v1/members/{subject}?scopes=<list>
```

- `scopes` is a comma-separated list of permissions or surfaces (`community.*`, `community.post`). What you get is the intersection of what you ask for and your consumer's `scopes` in the CRA. Asking for more is not an error; it returns nothing more and is recorded.
- `{subject}` is the pairwise id MSync gave you in an earlier answer. It is yours alone: another consumer's subject for the same person is a different string, and presenting it gets 404.
- There is no list or search endpoint.

## Who may call

A consumer is a `[[consumer]]` entry in the active CRA's `org.toml`:

```toml
[[consumer]]
key = "voltec-portal"
label = "Voltec community portal"
scopes = ["community.*"]
fields = ["tier", "positions.title", "merit.vested", "permissions"]
```

Its secret is never in the CRA and never in the database. MSync reads it from the environment, one variable per key id:

```
MSYNC_CONSUMER_SECRET_<KEY>_<KEYID>      key and key id uppercased, "-" becomes "_"
MSYNC_CONSUMER_SECRET_VOLTEC_PORTAL_K1   consumer voltec-portal, key id k1
```

- A key id is 1 to 16 characters of `a-z` and `0-9`.
- A secret shorter than 32 bytes is ignored. Generate 32 random bytes and hex-encode them (64 characters).
- Rotation: set `..._K2` beside `..._K1`, move the consumer to `k2`, then unset `..._K1`. Both work while both are set.
- `MSYNC_SUBJECT_KEY` (at least 32 bytes) derives the pairwise subject ids. Without it every authenticated lookup answers 503. Changing it changes every subject id every consumer holds, so set it once.

## Signing a request

Send five headers:

```
X-MSync-Consumer:  voltec-portal
X-MSync-Key-Id:    k1
X-MSync-Timestamp: 1794470400                 unix seconds, now
X-MSync-Nonce:     AAECAwQFBgcICQoLDA0ODw     22 to 128 base64url characters; 16 random bytes, new every request
X-MSync-Signature: v1=<hex HMAC-SHA256 of the string to sign, keyed with the secret>
```

The string to sign is six lines joined by `\n`, with no trailing newline:

```
v1
GET
<path and query exactly as sent, for example /api/v1/members/by-discord/300000000000000104?scopes=community.*>
<the X-MSync-Timestamp value>
<the X-MSync-Nonce value>
<lowercase hex SHA-256 of the body; for GET, the hash of the empty string>
```

Sign the path and query byte for byte as they go on the wire: do not decode, re-encode or reorder them, and send no trailing slash.

### Recipe (any language)

```
body_hash = hex(sha256(""))                    # e3b0c442...b855 for GET
sts       = "v1\nGET\n" + path_and_query + "\n" + timestamp + "\n" + nonce + "\n" + body_hash
signature = "v1=" + hex(hmac_sha256(secret, sts))
```

Python:

```python
import hashlib, hmac, os, base64, time

def msync_headers(consumer, key_id, secret: bytes, path_and_query: str):
    ts = str(int(time.time()))
    nonce = base64.urlsafe_b64encode(os.urandom(16)).rstrip(b"=").decode()
    sts = "\n".join(["v1", "GET", path_and_query, ts, nonce, hashlib.sha256(b"").hexdigest()])
    sig = "v1=" + hmac.new(secret, sts.encode(), hashlib.sha256).hexdigest()
    return {
        "X-MSync-Consumer": consumer,
        "X-MSync-Key-Id": key_id,
        "X-MSync-Timestamp": ts,
        "X-MSync-Nonce": nonce,
        "X-MSync-Signature": sig,
    }
```

### Worked example (fixed inputs)

The test `portal::tests::the_worked_example_in_the_doc_matches` recomputes this signature and fails if this document and the code disagree.

| Input | Value |
|---|---|
| secret | `voltec-example-secret-0123456789abcdef` (an example only; never use it) |
| consumer, key id | `voltec-portal`, `k1` |
| path and query | `/api/v1/members/by-discord/300000000000000104?scopes=community.*` |
| timestamp | `1794470400` |
| nonce | `AAECAwQFBgcICQoLDA0ODw` (the bytes 0 to 15, base64url) |

String to sign:

```
v1
GET
/api/v1/members/by-discord/300000000000000104?scopes=community.*
1794470400
AAECAwQFBgcICQoLDA0ODw
e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
```

Expected signature header:

```
X-MSync-Signature: v1=9b28b2b50864702de01fb3cc4558942636062e177f64ea5407b23f045600a52a
```

## What the server checks

In this order. Every failure answers the same `401 {"error":"unauthorized"}`, so a caller cannot tell which check failed:

1. The consumer is in the active CRA and the key id has a secret of at least 32 bytes.
2. `|server time - timestamp| <= 300` seconds. Keep your clock synced.
3. The signature, compared in constant time.
4. The nonce has not been seen for this consumer. Nonces are kept 10 minutes, then swept.
5. The per-consumer rate limit: 60 requests a minute by default (`MSYNC_PORTAL_RATE_PER_MINUTE`).

A request that passes all five is recorded in `consumer_lookups` and written to the audit log (`portal.lookup`: consumer, key id, what was looked up, the result, the scopes asked for, granted and refused). If that record cannot be written the answer is 503: MSync does not answer a lookup it cannot record. More than 500 distinct lookups by one consumer in an hour (`MSYNC_PORTAL_ALERT_LOOKUPS_PER_HOUR`) raises an alert in the log and the audit log (`portal.lookup_alert`), once an hour. Refused requests are logged as warnings, not audited.

## Answers

- `200`: the member, as below.
- `404 {"error":"not_found"}`: an unknown member, a member you may not see (another consumer's subject id, a Slack team the CRA does not name), a member without that Discord or Slack identity, and every lookup while no CRA snapshot is active. All identical.
- `401 {"error":"unauthorized"}`: any authentication failure.
- `503 {"error":"unavailable"}`: `MSYNC_SUBJECT_KEY` is not configured, or the lookup could not be recorded.

A `200` body:

```json
{
  "subject": "sub_508b135828168375d957ebd223172547",
  "status": "active",
  "tier": { "key": "newcomer", "label": "Newcomer", "rank": 0 },
  "positions": [ { "title": "Engineer" } ],
  "merit": { "vested": 0 },
  "permissions": ["community.post", "community.read", "community.vote"],
  "scopes_granted": ["community.*"],
  "policy_version": 1,
  "issued_at": "2026-11-21T19:00:00Z",
  "cache_ttl_seconds": 60
}
```

- `subject`, `status`, `scopes_granted`, `policy_version` (the active CRA snapshot id), `issued_at` and `cache_ttl_seconds` are always present. `status` is always present because a consumer must learn of a suspension to revoke what it granted; listing `status` in `fields` changes nothing.
- `tier` (field `tier`), `positions` (any of `positions.title`, `positions.rank`, `positions.expires_at`; each position object carries only the allowed keys; `expires_at` is the last day the position counts), `merit` (field `merit.vested`) and `permissions` (field `permissions`) appear only when the consumer's `fields` list them. A field you may not read is omitted, never `null`.
- `permissions` is the member's effective permissions filtered to `scopes_granted`. A member who holds `payouts.approve` in MSync shows nothing of it to a consumer allowed `community.*`.
- A suspended or removed member answers `200` with that `status`, `permissions: []`, and no tier, positions or merit.
- Cache an answer for at most `cache_ttl_seconds`.

The subject is `sub_` plus the first 32 hex characters of `HMAC-SHA256(consumer_key, member_id)`, where `consumer_key = HMAC-SHA256(MSYNC_SUBJECT_KEY, "msync-subject-v1:default:" + consumer)`. You never learn the MSync member id, and two consumers cannot match their users by id.

## The same person under the three example CRAs

McKale Olson is a member of all three example CRAs in `examples/cra/`. Here is what each organization's consumer sees for him, with `MSYNC_SUBJECT_KEY = msync-example-subject-key-0123456789abcdef`, no merit yet (0 vested), a request for every scope the consumer may hold, and the CRA as snapshot 1. The test `portal::tests::the_walkthrough_in_the_doc_matches` computes these bodies.

### Voltec Industries (hybrid mode, Discord only)

Consumer `voltec-portal`: scopes `community.*`; fields `tier`, `positions.title`, `merit.vested`, `permissions`. In the CRA McKale is an Engineer (rank 20), whose permissions include `roster.read_members` and `payouts.read_own`; neither is community, so neither leaves MSync.

`GET /api/v1/members/by-discord/300000000000000104?scopes=community.*`

```json
{
  "subject": "sub_508b135828168375d957ebd223172547",
  "status": "active",
  "tier": { "key": "newcomer", "label": "Newcomer", "rank": 0 },
  "positions": [ { "title": "Engineer" } ],
  "merit": { "vested": 0 },
  "permissions": ["community.post", "community.read", "community.vote"],
  "scopes_granted": ["community.*"],
  "policy_version": 1,
  "issued_at": "2026-11-21T19:00:00Z",
  "cache_ttl_seconds": 60
}
```

Voltec's own code still applies its fixed `community.*` allowlist to whatever MSync returns (hierarchy document 8.5).

### Eustress Engine (source available, Discord and Slack)

Consumer `eustress-site`: scopes `community.*` and `profile.*`; the same four fields. McKale is Lead Maintainer (rank 80) until 2027-05-01. That position carries authority (`members.grant`, `payouts.approve`, `policy.approve_change`, `audit.read_all` and more); none of it is community or profile, so the website sees only what the Newcomer tier gives.

`GET /api/v1/members/by-slack/T1000000000/U1000000102?scopes=community.*,profile.*` (or `by-discord/100000000000000102`, same answer)

```json
{
  "subject": "sub_940b7c756e9c611a646c72f653dce73d",
  "status": "active",
  "tier": { "key": "newcomer", "label": "Newcomer", "rank": 0 },
  "positions": [ { "title": "Lead Maintainer" } ],
  "merit": { "vested": 0 },
  "permissions": ["community.read", "profile.self"],
  "scopes_granted": ["community.*", "profile.*"],
  "policy_version": 1,
  "issued_at": "2026-11-21T19:00:00Z",
  "cache_ttl_seconds": 60
}
```

The subject differs from Voltec's: the two organizations cannot tell they are looking at the same person.

### Weave Solutions (closed mode, Slack only)

Weave's CRA declares no `[[consumer]]`. No server can call the portal API for Weave, so every request, for McKale or anyone, fails check 1 and answers `401 {"error":"unauthorized"}`. A closed organization opens this channel only by adding a consumer through a reviewed CRA commit.
