# Source available template

For a project whose code is public to read but whose license is not an open-source license. Eustress Engine is the model.

## What this mode means

`source_available` mode keeps the code readable but treats the community as members: the roster, leaderboard and profiles are for members only. Outside contributors still earn merit in every listed repository. Appointed positions from rank 50 up are two-person and serve at most 180 days.

## Where the `.msync/` files live

**Where the `.msync/` files live.** There are two kinds of repository, and they are not the same thing.

- **The hierarchy repository** holds `.msync/`. It is a small, private repository that the owner manages, usually with an assistant such as Claude. Only the few people allowed to change the hierarchy get write access, so every change arrives as a reviewed pull request. Ordinary members and outside contributors never need access to it.
- **Work repositories** are where people write code. They are listed in `org.toml` under `[[foundry]]`. They can be public or private, on GitHub or GitLab, and they never contain `.msync/`. MSync only receives their events.
- **Compensation is in neither.** Pay orders, hourly rates, shifts, timesheets and invoices live in MSync's own database, behind scoped tokens.

Putting `.msync/` inside a shared work repository is allowed for a small open project, but it is not the recommended layout: everyone with write access to the code could then propose hierarchy changes, and the roster would be as visible as the code.

## What to fill in

Open the five files in `.msync/` and replace every `REPLACE_ME_...` value. Each file has comments saying what the value does and what you must decide.

- Mode (already `source_available`).
- Org slug, name, Discord server and Slack workspace ids (keep at least one).
- Your repository.
- The founder and appointed people: ids, names, logins, chat ids, and an expiry date for each appointed position (within 180 days).
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
