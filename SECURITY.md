# Security policy

MSync (MeritSync) handles identities, ranks and money. Please report vulnerabilities privately.

## Reporting a vulnerability

Do not open a public issue or pull request for a security problem.

Send the report to: `REPLACE_WITH_SECURITY_CONTACT` (the repository owner must replace this with a monitored address, or enable GitHub private vulnerability reporting and name it here).

Include:

- What you found and which component it is in.
- The version or commit you tested.
- Steps to reproduce, with the smallest request or input that shows it.
- What an attacker gains (read data, act as another member, move money, forge an audit entry).
- Any logs or proof of concept. Remove real secrets and personal data first.

## What is in scope

- Signature and secret checks: GitHub, GitLab, Slack and Stripe webhooks, the portal HMAC (timestamp, nonce, replay).
- API tokens and scopes: any way to act without a token, with a revoked or expired token, or beyond a token's scopes, including through MCP.
- Payout approvals: self-approval, bypassing the approver rules, limits, hold, blocks or the kill switch, executing a payout without approvals, or enabling MCP payout execution remotely.
- The audit chain: altering, deleting or forging an entry without detection.
- OAuth and OpenID Connect: redirect, PKCE, token, session and linking flaws.
- The CRA: loading a snapshot without the required review, minting an owner from repository content, or reading confidential compartments.
- Disclosure of secrets in logs, responses or errors.

Out of scope: findings that need dev mode (`MSYNC_DEV_MODE`) on a reachable host, which is documented as unsafe; social engineering; denial of service by raw traffic volume; vulnerabilities in third-party services themselves.

## What to expect

These are goals. The owner should confirm or edit them before publishing this file.

- Acknowledgement within 3 working days.
- An initial assessment within 10 working days.
- A fix or a mitigation plan for confirmed issues within 90 days, sooner for critical ones.
- Credit in the release notes if you want it.

MSync is pre-release and has no published binary. Until a release exists, fixes land on the main branch.

## No secrets in the repository

No secret belongs in this repository or its history: no token, key, signing secret or password. `.env.example` is the only env file that is tracked, and it holds names and placeholders only. Real values go in `.env`, which is not committed. If you find a secret in the tree or in history, report it as a vulnerability and rotate it.

For the design behind these controls, see `docs/HIERARCHY_AND_THREAT_MODEL.md`.
