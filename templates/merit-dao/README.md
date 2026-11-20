# Merit DAO template

For a merit-driven collective: open work and open contribution, where standing is earned by merit and a small council holds the few powers that need people.

## What this mode means

This template uses `open` mode. Four merit tiers carry most of the structure (vote, run events, moderate), and a small appointed council (Council Member, Convener) holds the authority permissions. The payout file is set up for the council to prepare and approve. Pooled XP payouts are not a file setting: they are pay orders created through the MSync API, described in `docs/PAYROLL.md`.

## Where the `.msync/` files live

**Where the `.msync/` files live.** There are two kinds of repository, and they are not the same thing.

- **The hierarchy repository** holds `.msync/`. It is a small, private repository that the owner manages, usually with an assistant such as Claude. Only the few people allowed to change the hierarchy get write access, so every change arrives as a reviewed pull request. Ordinary members and outside contributors never need access to it.
- **Work repositories** are where people write code. They are listed in `org.toml` under `[[foundry]]`. They can be public or private, on GitHub or GitLab, and they never contain `.msync/`. MSync only receives their events.
- **Compensation is in neither.** Pay orders, hourly rates, shifts, timesheets and invoices live in MSync's own database, behind scoped tokens.

Putting `.msync/` inside a shared work repository is allowed for a small open project, but it is not the recommended layout: everyone with write access to the code could then propose hierarchy changes, and the roster would be as visible as the code.

## What to fill in

Open the five files in `.msync/` and replace every `REPLACE_ME_...` value. Each file has comments saying what the value does and what you must decide.

- Org slug, name, Discord server and Slack workspace ids (keep at least one).
- Your public repository.
- The founder, the convener and a council member: ids, names, logins, chat ids, and an expiry date for each appointed position.
- Tier names, merit thresholds and chat mappings.
- Merit points per event, and the payout amounts, approvers and schedule.

## Check it

Until every placeholder is replaced, `msync check` fails and lists the ones left. That is intended: it keeps an unfinished template from going live.

```
msync check .
msync plan <old-dir> .
```

`msync check .` validates the five files and prints OK or every error at once. `msync plan <old-dir> .` prints a Markdown preview of what would change against an earlier snapshot; for a first snapshot pass an empty directory.

## The CI files

- `.github/workflows/msync-cra.yml` runs on every pull request that touches `.msync/`. It checks the files and posts the plan as one pull request comment. It uses only the default `GITHUB_TOKEN`.
- `.msync/gitlab-ci.yml` does the same for GitLab merge requests. Pull it into your `.gitlab-ci.yml` with `include: { local: '.msync/gitlab-ci.yml' }`. Posting the note needs a masked CI/CD variable `MSYNC_MR_NOTE_TOKEN` (a project access token with the api scope) that the owner creates in GitLab; never commit it.
- Both download the msync binary from the variable `MSYNC_RELEASE_URL`. No MSync release exists yet, so set that variable when one does, or switch to the commented `cargo install` alternative in the file.
- Delete the file for the host you do not use. The two files are the same in every template.

Owners (rank 1000) are never in these files: set them with the engine configuration `MSYNC_OWNER_MEMBER_IDS`, listing the founder as `github:<login>`.
