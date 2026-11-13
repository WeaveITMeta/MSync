-- Foundry API normalization (GitHub and GitLab), rubric score submissions, multi-repository XP.
-- Additive only: no existing table or column is changed or dropped.

-- One contributor can hold several GitHub and GitLab identities. Keyed by (provider, host, login).
-- contributor_id holds the same value (and storage type) as contributors.id.
CREATE TABLE IF NOT EXISTS foundry_identities (
    provider TEXT NOT NULL,            -- github | gitlab
    host TEXT NOT NULL,                -- lowercase, e.g. github.com, gitlab.com
    login TEXT NOT NULL,
    contributor_id TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE (provider, host, login)
);
CREATE INDEX IF NOT EXISTS idx_foundry_identities_contributor ON foundry_identities(contributor_id);

-- Contributions are found by subject, not by PR number alone (two repos can share a number).
-- Legacy rows keep NULL here and are matched by github_pr_number as before.
ALTER TABLE contributions ADD COLUMN provider TEXT;
ALTER TABLE contributions ADD COLUMN host TEXT;
ALTER TABLE contributions ADD COLUMN repo TEXT;
ALTER TABLE contributions ADD COLUMN subject_id TEXT;
ALTER TABLE contributions ADD COLUMN author_login TEXT;
ALTER TABLE contributions ADD COLUMN rubric_score REAL;
CREATE INDEX IF NOT EXISTS idx_contributions_subject ON contributions(provider, host, repo, subject_id);

-- Where each merit entry was earned. Legacy rows stay NULL (the "unattributed" bucket).
ALTER TABLE merit_activities ADD COLUMN provider TEXT;
ALTER TABLE merit_activities ADD COLUMN host TEXT;
ALTER TABLE merit_activities ADD COLUMN repo TEXT;
CREATE INDEX IF NOT EXISTS idx_activities_repo ON merit_activities(provider, host, repo);

-- Rubric score submissions (dsync-rubric-v1). One per scorer per subject.
CREATE TABLE IF NOT EXISTS merit_scores (
    id TEXT PRIMARY KEY NOT NULL,
    provider TEXT NOT NULL,
    host TEXT NOT NULL,
    repo TEXT NOT NULL,
    subject_id TEXT NOT NULL,
    scorer_id TEXT NOT NULL,
    scorer_source TEXT NOT NULL,       -- agent | coderabbit
    rubric_id TEXT NOT NULL,
    rubric_hash TEXT NOT NULL,
    score REAL NOT NULL,               -- 0..100 after gates
    ci_passed INTEGER NOT NULL,
    reverted INTEGER NOT NULL,
    detail TEXT NOT NULL,              -- JSON: criteria levels + evidence, or total + evidence
    created_at TEXT NOT NULL,
    UNIQUE (provider, host, repo, subject_id, scorer_id)
);
CREATE INDEX IF NOT EXISTS idx_merit_scores_subject ON merit_scores(provider, host, repo, subject_id);
