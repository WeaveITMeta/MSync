#!/usr/bin/env bash
# MSync smoke test against a RUNNING server. It starts nothing and stops nothing.
#
# It drives the real HTTP surface with signed requests and asserts on every answer:
# health, auth, the hierarchy, signed GitHub and GitLab webhooks (and their dedupe and
# refusals), rubric scores over REST and MCP, the leaderboard, payout controls (never a
# transfer), a signed Slack command, a signed portal lookup, the audit chain and the
# OpenID Connect discovery document.
#
# It expects the server to have the eustress-engine example as its CRA
# (examples/cra/eustress-engine, loaded with MSYNC_CRA_LOCAL_DIR in dev mode) and
# MSYNC_OWNER_MEMBER_IDS=github:example-founder. scripts/smoke-local.sh sets all of that up.
#
# Environment (all required):
#   MSYNC_SMOKE_URL              base URL, for example http://127.0.0.1:18431
#   MSYNC_SMOKE_ADMIN_TOKEN      the server's MSYNC_ADMIN_TOKEN
#   MSYNC_SMOKE_GH_SECRET        the server's GITHUB_WEBHOOK_SECRET
#   MSYNC_SMOKE_GL_SECRET        the server's GITLAB_WEBHOOK_SECRET
#   MSYNC_SMOKE_SLACK_SECRET     the server's MSYNC_SLACK_SIGNING_SECRET
#   MSYNC_SMOKE_CONSUMER_SECRET  the server's MSYNC_CONSUMER_SECRET_EUSTRESS_SITE_K1
#
# Needs bash, curl, openssl and python (standard library only, for JSON). It talks to
# MSYNC_SMOKE_URL only. One line per check, PASS or FAIL; exit 1 on any failure.
#
# Every run uses fresh ids (pull request numbers, invoice numbers, nonces), so it can be
# run more than once against the same server.

set -u

for v in MSYNC_SMOKE_URL MSYNC_SMOKE_ADMIN_TOKEN MSYNC_SMOKE_GH_SECRET MSYNC_SMOKE_GL_SECRET MSYNC_SMOKE_SLACK_SECRET MSYNC_SMOKE_CONSUMER_SECRET; do
  if [ -z "${!v:-}" ]; then
    echo "smoke.sh: $v is not set" >&2
    exit 2
  fi
done
for tool in curl openssl; do
  command -v "$tool" >/dev/null 2>&1 || { echo "smoke.sh: $tool not found" >&2; exit 2; }
done
PY=""
for c in python3 python; do
  if command -v "$c" >/dev/null 2>&1 && "$c" -c 'import json,sys' >/dev/null 2>&1; then PY="$c"; break; fi
done
[ -n "$PY" ] || { echo "smoke.sh: python (standard library) not found" >&2; exit 2; }

BASE="${MSYNC_SMOKE_URL%/}"
T="$(mktemp -d 2>/dev/null || mktemp -d -t msync-smoke)"
trap 'rm -rf "$T"' EXIT
PASSES=0
FAILS=0
RUN="$(date +%s)$$"
RUN_NUM=$(( (RUN % 900000) + 100000 ))

# ------------------------------------------------------------------ helpers

pass() { PASSES=$((PASSES + 1)); echo "PASS $1"; }
fail() { FAILS=$((FAILS + 1)); echo "FAIL $1"; [ -n "${2:-}" ] && echo "     $2"; return 0; }

# req METHOD PATH [curl args...]: body in $T/body, status in $STATUS.
req() {
  local m="$1" p="$2"; shift 2
  STATUS=$(curl -s -o "$T/body" -w '%{http_code}' -X "$m" "$BASE$p" "$@") || STATUS=000
}

# jq-free JSON: py expression over `d` (the parsed body). Prints the value, or nothing.
j() {
  "$PY" - "$T/body" "$1" <<'EOF' 2>/dev/null
import json, sys
try:
    d = json.load(open(sys.argv[1], encoding="utf-8"))
    v = eval(sys.argv[2], {"d": d, "json_text": json.dumps(d), "any": any, "all": all, "len": len, "sum": sum})
except Exception:
    sys.exit(0)
if isinstance(v, bool):
    print("true" if v else "false")
elif isinstance(v, (dict, list)):
    print(json.dumps(v))
elif v is not None:
    print(v)
EOF
}

body() { head -c 400 "$T/body" 2>/dev/null; }

# expect NAME STATUS [PY-CONDITION]: pass when the status matches and the condition holds.
expect() {
  local name="$1" want="$2" cond="${3:-True}"
  if [ "$STATUS" != "$want" ]; then
    fail "$name" "HTTP $STATUS, expected $want: $(body)"
  elif [ "$cond" != "True" ] && [ "$(j "bool($cond)")" != "true" ]; then
    fail "$name" "condition failed: $cond: $(body)"
  else
    pass "$name"
  fi
}

hmac_hex() { # key data -> hex
  printf '%s' "$2" | openssl dgst -sha256 -hmac "$1" | sed 's/^.*= *//'
}
sha256_hex() { printf '%s' "$1" | openssl dgst -sha256 | sed 's/^.*= *//'; }

auth() { echo "Authorization: Bearer $1"; }
json_hdr="Content-Type: application/json"

# token LABEL MEMBER SCOPES -> prints the token (bootstrap admin token)
token() {
  req POST /api/v1/tokens -H "$(auth "$MSYNC_SMOKE_ADMIN_TOKEN")" -H "$json_hdr" \
    -d "{\"label\":\"$1\",\"member_id\":\"$2\",\"scopes\":$3}"
  if [ "$STATUS" = "201" ]; then j "d['token']['token']"; fi
}

github() { # EVENT DELIVERY JSON [SECRET]
  local secret="${4:-$MSYNC_SMOKE_GH_SECRET}"
  local sig; sig="sha256=$(hmac_hex "$secret" "$3")"
  req POST /github/webhook -H "$json_hdr" -H "X-GitHub-Event: $1" -H "X-GitHub-Delivery: $2" \
    -H "X-Hub-Signature-256: $sig" --data-binary "$3"
}

pr_json() { # ACTION MERGED NUMBER ID LOGIN SHA
  printf '{"action":"%s","repository":{"full_name":"example-eustress/eustress-engine"},"pull_request":{"number":%s,"id":%s,"title":"Smoke change %s","body":"Fixes the smoke test path; tested locally.","state":"%s","merged":%s,"user":{"login":"%s"},"merge_commit_sha":%s,"base":{"ref":"main"}}}' \
    "$1" "$3" "$4" "$3" "$( [ "$2" = true ] && echo closed || echo open )" "$2" "$5" "$6"
}

mcp() { # TOKEN JSON-RPC-BODY
  req POST /mcp -H "$(auth "$1")" -H "$json_hdr" -d "$2"
}

criteria='{"impact":{"level":3,"evidence":"fixes a real defect in the payout claim path"},"correctness":{"level":3,"evidence":"tests cover the duplicate claim edge cases"},"difficulty":{"level":2,"evidence":"needs knowledge of the ledger module"},"quality":{"level":3,"evidence":"simplifies the claim query it touches"},"risk":{"level":3,"evidence":"risk identified and contained by the claim guard"},"communication":{"level":3,"evidence":"the description lets a reviewer verify the change"}}'

echo "MSync smoke test against $BASE (run $RUN)"

# ------------------------------------------------------------------ health and auth

req GET /health
expect "health answers healthy" 200 "d['status'] == 'healthy' and 'dev_mode' in d"
req GET /api/v1/hierarchy
expect "an unauthenticated /api/v1 call is 401" 401

# ------------------------------------------------------------------ tokens

FOUNDER=$(token "smoke-founder-$RUN" founder '["hierarchy.read","merit.read","merit.score","payouts.read","payouts.prepare","payouts.approve","payees.read","payees.manage","payorders.read","payorders.manage","payorders.approve","invoices.read","invoices.submit","invoices.approve","audit.read","tokens.manage"]')
if [ -n "$FOUNDER" ]; then pass "bootstrap an API token for the owner with the admin token"; else fail "bootstrap an API token for the owner with the admin token" "HTTP $STATUS: $(body)"; fi
MCKALE=$(token "smoke-mckale-$RUN" mckale '["hierarchy.read","merit.read","merit.score","payouts.read","payorders.read","payorders.approve","invoices.read","invoices.approve"]')
[ -n "$MCKALE" ] && pass "a second token for a second approver (mckale)" || fail "a second token for a second approver (mckale)" "HTTP $STATUS: $(body)"
NARROW=$(token "smoke-narrow-$RUN" ada-example '["hierarchy.read"]')
[ -n "$NARROW" ] && pass "a narrow token with hierarchy.read only" || fail "a narrow token with hierarchy.read only" "HTTP $STATUS: $(body)"

req GET /api/v1/payouts/policy -H "$(auth "$NARROW")"
expect "a token without payouts.read is refused" 403
req POST /api/v1/tokens -H "$(auth "wrong-admin-token")" -H "$json_hdr" -d '{"label":"x","member_id":"founder","scopes":["hierarchy.read"]}'
expect "a wrong bootstrap token is 401" 401

# ------------------------------------------------------------------ hierarchy

req GET /api/v1/hierarchy -H "$(auth "$FOUNDER")"
expect "the hierarchy shows the Eustress roles" 200 \
  "d['hierarchy']['org_slug'] == 'eustress-engine' and {'lead-maintainer','maintainer','reviewer','contributor'} <= {r['key'] for r in d['hierarchy']['roles']}"
req GET /api/v1/hierarchy/members/ada-example -H "$(auth "$NARROW")"
expect "a member's standing (ada-example holds maintainer)" 200 \
  "any(p['key'] == 'maintainer' for p in d['member']['standing']['positions'])"

# ------------------------------------------------------------------ GitHub webhooks

N1=$RUN_NUM; ID1=$((RUN_NUM * 10 + 1))
N2=$((RUN_NUM + 1)); ID2=$((RUN_NUM * 10 + 2))
SHA1=$(sha256_hex "smoke-$RUN-1" | cut -c1-40)
SHA2=$(sha256_hex "smoke-$RUN-2" | cut -c1-40)

github pull_request "smoke-$RUN-open-1" "$(pr_json opened false "$N1" "$ID1" ada-example null)"
expect "signed GitHub pull_request opened is accepted" 200 "d['status'] == 'success'"
MERGE1="$(pr_json closed true "$N1" "$ID1" ada-example "\"$SHA1\"")"
github pull_request "smoke-$RUN-merge-1" "$MERGE1"
expect "signed GitHub pull_request merged is accepted" 200 "d['status'] == 'success'"
github pull_request "smoke-$RUN-merge-1" "$MERGE1"
expect "the same delivery again is deduped" 200 "d['status'] == 'duplicate'"
github pull_request "smoke-$RUN-bad" "$MERGE1" "not-the-secret"
expect "a wrongly signed GitHub webhook is 401" 401
github pull_request "smoke-$RUN-open-2" "$(pr_json opened false "$N2" "$ID2" bo-example null)"
github pull_request "smoke-$RUN-merge-2" "$(pr_json closed true "$N2" "$ID2" bo-example "\"$SHA2\"")"
expect "a second merged pull request (bo-example)" 200 "d['status'] == 'success'"

# ------------------------------------------------------------------ GitLab webhook

GL="{\"object_kind\":\"merge_request\",\"user\":{\"id\":7,\"username\":\"ada-example\"},\"project\":{\"path_with_namespace\":\"example-eustress/eustress-mirror\",\"web_url\":\"https://gitlab.com/example-eustress/eustress-mirror\"},\"object_attributes\":{\"id\":$ID1,\"iid\":$N1,\"title\":\"Smoke MR\",\"description\":\"why\",\"action\":\"open\",\"state\":\"opened\",\"target_branch\":\"main\",\"author_id\":7}}"
req POST /gitlab/webhook -H "$json_hdr" -H "X-Gitlab-Event: Merge Request Hook" -H "X-Gitlab-Event-UUID: smoke-$RUN-gl-1" \
  -H "X-Gitlab-Token: $MSYNC_SMOKE_GL_SECRET" --data-binary "$GL"
expect "a GitLab merge request hook with the token header is verified (unlisted repository earns nothing)" 200
req POST /gitlab/webhook -H "$json_hdr" -H "X-Gitlab-Event: Merge Request Hook" -H "X-Gitlab-Event-UUID: smoke-$RUN-gl-2" \
  -H "X-Gitlab-Token: wrong" --data-binary "$GL"
expect "a GitLab hook with a wrong token is 401" 401

# ------------------------------------------------------------------ rubric and scores

req GET /api/v1/merit/rubric
expect "the rubric is public" 200 "d['rubric']['id'] == 'msync-rubric-v1' if 'rubric' in d else d['id'] == 'msync-rubric-v1'"
RUBRIC_HASH=$(j "d['rubric']['hash'] if 'rubric' in d else d['hash']")

SCORE="{\"subject\":{\"provider\":\"github\",\"repo\":\"example-eustress/eustress-engine\",\"subject_id\":\"$ID1\"},\"rubric_id\":\"msync-rubric-v1\",\"rubric_hash\":\"$RUBRIC_HASH\",\"ci_passed\":true,\"reverted\":false,\"criteria\":$criteria}"
req POST /api/v1/merit/scores -H "$(auth "$FOUNDER")" -H "$json_hdr" -d "$SCORE"
expect "a score submitted through REST" 201 "'msync-rubric-v1' in json_text and 'score' in json_text"

mcp "$FOUNDER" '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"smoke","version":"1"}}}'
expect "MCP initialize" 200 "'result' in d"
mcp "$FOUNDER" '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}'
expect "MCP tools/list lists the scoring tool" 200 "any(t['name'] == 'merit_score_submit' for t in d['result']['tools'])"
expect "the MCP payout execute tool is absent" 200 "not any(t['name'] == 'payouts_run_execute' for t in d['result']['tools'])"
MCP_SCORE="{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/call\",\"params\":{\"name\":\"merit_score_submit\",\"arguments\":{\"subject\":{\"provider\":\"github\",\"repo\":\"example-eustress/eustress-engine\",\"subject_id\":\"$ID2\"},\"rubric_id\":\"msync-rubric-v1\",\"rubric_hash\":\"$RUBRIC_HASH\",\"ci_passed\":true,\"reverted\":false,\"criteria\":$criteria}}}"
mcp "$MCKALE" "$MCP_SCORE"
expect "a score submitted through MCP tools/call" 200 "'result' in d and not d['result'].get('isError', False)"
mcp "$FOUNDER" '{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"payouts_run_execute","arguments":{"run_id":"x"}}}'
expect "calling the MCP execute tool is refused" 200 "'error' in d or d['result'].get('isError', False)"

req GET /api/v1/leaderboard -H "$(auth "$FOUNDER")"
expect "the leaderboard shows ada-example with points from the merge and the score" 200 \
  "any(r['github_username'] == 'ada-example' and r['total_points'] > 15 for r in d['result']['leaderboard'])"
expect "each row drills down by repository (example-eustress/eustress-engine)" 200 \
  "any(r['github_username'] == 'ada-example' and any(b.get('repo') == 'example-eustress/eustress-engine' and b['points'] > 0 for b in r['by_repo']) for r in d['result']['leaderboard'])"
expect "per-repository amounts add up to each total" 200 \
  "all(sum(b['points'] for b in r['by_repo']) == r['total_points'] for r in d['result']['leaderboard'])"
req GET "/api/v1/leaderboard?provider=github&repo=example-eustress/eustress-engine" -H "$(auth "$FOUNDER")"
expect "the per-repository leaderboard ranks by that repository" 200 \
  "any(r['github_username'] == 'ada-example' for r in d['result']['leaderboard'])"

# ------------------------------------------------------------------ payouts (no transfer, ever)

req GET /api/v1/payouts/policy -H "$(auth "$FOUNDER")"
expect "the payout policy is readable (from the CRA: 2 approvals, rank 60)" 200 \
  "d['policy']['approval']['approvals_required'] == 2 and d['policy']['approval']['min_approver_rank'] == 60"
req POST /api/v1/payouts/runs -H "$(auth "$FOUNDER")" -H "$json_hdr" -d '{"dry_run":true}'
expect "a dry-run payout run persists nothing and returns a plan" 200 "d['dry_run'] == True and 'plan' in d"

req POST /api/v1/payees -H "$(auth "$FOUNDER")" -H "$json_hdr" \
  -d "{\"display_name\":\"Bo Example $RUN\",\"kind\":\"individual\",\"email\":\"bo-$RUN@example.com\",\"member_id\":\"bo-example\",\"payee_keys\":[\"bo-example-$RUN\"]}"
expect "a payee is created" 201 "d['payee']['id']"
PAYEE=$(j "d['payee']['id']")

TODAY=$(date -u +%Y-%m-%d)
req POST /api/v1/payorders -H "$(auth "$FOUNDER")" -H "$json_hdr" \
  -d "{\"name\":\"Smoke stipend $RUN\",\"rule\":{\"kind\":\"fixed\",\"payee_id\":\"$PAYEE\",\"amount_cents\":5000},\"schedule\":{\"kind\":\"once\",\"date\":\"$TODAY\"}}"
expect "a fixed pay order is created" 201 "d['payorder']['id']"
ORDER=$(j "d['payorder']['id']")
req GET "/api/v1/payorders/$ORDER/preview?periods=2" -H "$(auth "$FOUNDER")"
expect "the pay order preview shows what it would pay" 200 "d['status'] == 'success' and 'preview' in d"
req POST "/api/v1/payorders/$ORDER/approve" -H "$(auth "$FOUNDER")"
expect "an approval by the order's own creator is refused" 403
req POST "/api/v1/payorders/$ORDER/approve" -H "$(auth "$MCKALE")"
expect "an approval by a second member is recorded" 200 "d['status'] == 'success'"

req POST /api/v1/payorders -H "$(auth "$FOUNDER")" -H "$json_hdr" \
  -d "{\"name\":\"Smoke XP pool $RUN\",\"rule\":{\"kind\":\"pool_by_xp\",\"pool_cents\":100000},\"schedule\":{\"kind\":\"repeat\",\"cadence\":\"monthly\",\"start\":\"$TODAY\"},\"draft\":true}"
expect "an XP pool pay order is created" 201 "d['payorder']['id']"
POOL=$(j "d['payorder']['id']")
req GET "/api/v1/payorders/$POOL/preview?periods=1" -H "$(auth "$FOUNDER")"
expect "the XP pool preview splits by XP" 200 "d['status'] == 'success' and 'preview' in d"

req POST /api/v1/invoices -H "$(auth "$FOUNDER")" -H "$json_hdr" \
  -d "{\"payee_id\":\"$PAYEE\",\"number\":\"SMOKE-$RUN\",\"issue_date\":\"$TODAY\",\"due_date\":\"$TODAY\",\"lines\":[{\"description\":\"Smoke work, 2 hours\",\"quantity\":2,\"unit_amount_cents\":2500}],\"total_cents\":5000}"
expect "an invoice is submitted" 201 "d['invoice']['id']"
INVOICE=$(j "d['invoice']['id']")
req POST "/api/v1/invoices/$INVOICE/approve" -H "$(auth "$MCKALE")"
expect "the invoice is approved by a second token" 200 "d['status'] == 'success'"

# ------------------------------------------------------------------ Slack

slack() { # BODY SECRET
  local ts; ts=$(date +%s)
  local sig; sig="v0=$(hmac_hex "$2" "v0:$ts:$1")"
  req POST /slack/commands -H "Content-Type: application/x-www-form-urlencoded" \
    -H "X-Slack-Request-Timestamp: $ts" -H "X-Slack-Signature: $sig" --data-binary "$1"
}
SLACK_BODY="token=x&team_id=T1000000000&team_domain=eustress&channel_id=C1000000009&channel_name=general&user_id=U1000000103&user_name=ada&command=%2Fmsync-stats&text=&response_url=http%3A%2F%2F127.0.0.1%3A9%2Fnowhere&trigger_id=1"
slack "$SLACK_BODY" "$MSYNC_SMOKE_SLACK_SECRET"
expect "a signed Slack /msync-stats command is answered" 200 "'text' in d or 'blocks' in d"
slack "$SLACK_BODY" "not-the-secret"
expect "a wrongly signed Slack command is 401" 401

# ------------------------------------------------------------------ portal

portal() { # PATH_AND_QUERY NONCE
  local ts; ts=$(date +%s)
  local bh; bh=$(sha256_hex "")
  local sts; sts=$(printf 'v1\nGET\n%s\n%s\n%s\n%s' "$1" "$ts" "$2" "$bh")
  local sig; sig="v1=$(hmac_hex "$MSYNC_SMOKE_CONSUMER_SECRET" "$sts")"
  req GET "$1" -H "X-MSync-Consumer: eustress-site" -H "X-MSync-Key-Id: k1" -H "X-MSync-Timestamp: $ts" \
    -H "X-MSync-Nonce: $2" -H "X-MSync-Signature: $sig"
}
NONCE=$(sha256_hex "nonce-$RUN" | cut -c1-32)
portal "/api/v1/members/by-discord/100000000000000103?scopes=community.*" "$NONCE"
expect "a portal lookup with a correct HMAC answers the member" 200 "d.get('subject', '').startswith('sub_')"
portal "/api/v1/members/by-discord/100000000000000103?scopes=community.*" "$NONCE"
expect "a replayed portal nonce is refused" 401

# ------------------------------------------------------------------ audit

req GET /api/v1/audit/verify -H "$(auth "$FOUNDER")"
expect "the audit chain verifies intact" 200 "d['verify']['ok'] == True and d['verify']['first_broken'] is None"
AUDITN=$(j "d['verify']['checked']")
if [ "${AUDITN:-0}" -gt 0 ] 2>/dev/null; then pass "the audit chain has more than zero entries ($AUDITN)"; else fail "the audit chain has more than zero entries" "$(body)"; fi

# ------------------------------------------------------------------ OpenID Connect

req GET /.well-known/openid-configuration
expect "OpenID Connect discovery" 200 "d['issuer'] and d['jwks_uri'].endswith('/oauth/jwks')"
req GET /oauth/jwks
expect "the JWKS lists a signing key" 200 "len(d['keys']) >= 1 and d['keys'][0]['kty'] == 'EC'"

echo
echo "$PASSES passed, $FAILS failed"
[ "$FAILS" -eq 0 ]
