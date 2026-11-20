# Hybrid template

For a company or group that runs a public repository for outside contributors and a private one for staff.

## What this mode means

`hybrid` mode shares the community and keeps the rest closed. Outside contributors earn merit only in public repositories; staff earn in both. Roster and leaderboard are members only. You can declare need-to-know compartments, and appointed positions from rank 50 up are two-person.

## What to fill in

Open the five files in `.msync/` and replace every `REPLACE_ME_...` value. Each file has comments saying what the value does and what you must decide.

- Mode (already `hybrid`).
- Org slug, name, Discord server and Slack workspace ids (keep at least one).
- The public and the private repository.
- The compartment (or remove it), and which roles are in it.
- The founder and appointed people: ids, names, logins, chat ids, expiry dates (within 180 days).
- Role titles, ranks and chat mappings.
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
