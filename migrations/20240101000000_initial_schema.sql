-- Create contributors table
CREATE TABLE IF NOT EXISTS contributors (
    id TEXT PRIMARY KEY NOT NULL,
    github_username TEXT UNIQUE NOT NULL,
    discord_user_id INTEGER,
    total_merit_points INTEGER NOT NULL DEFAULT 0,
    current_role TEXT NOT NULL DEFAULT 'Newcomer',
    contributions_count INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);

-- Create index on discord_user_id for quick lookups
CREATE INDEX IF NOT EXISTS idx_contributors_discord_id ON contributors(discord_user_id);
CREATE INDEX IF NOT EXISTS idx_contributors_github_username ON contributors(github_username);

-- Create contributions table
CREATE TABLE IF NOT EXISTS contributions (
    id TEXT PRIMARY KEY NOT NULL,
    contributor_id TEXT NOT NULL,
    github_pr_number INTEGER NOT NULL,
    title TEXT NOT NULL,
    description TEXT,
    merit_points INTEGER NOT NULL DEFAULT 0,
    status INTEGER NOT NULL DEFAULT 0, -- 0: Pending, 1: UnderReview, 2: Approved, 3: Merged, 4: Rejected
    coderabbit_score REAL,
    created_at TEXT NOT NULL,
    merged_at TEXT,
    FOREIGN KEY (contributor_id) REFERENCES contributors(id) ON DELETE CASCADE
);

-- Create index on contributor_id for quick lookups
CREATE INDEX IF NOT EXISTS idx_contributions_contributor ON contributions(contributor_id);
CREATE INDEX IF NOT EXISTS idx_contributions_pr_number ON contributions(github_pr_number);

-- Create merit_roles table
CREATE TABLE IF NOT EXISTS merit_roles (
    id TEXT PRIMARY KEY NOT NULL,
    role_name TEXT UNIQUE NOT NULL,
    discord_role_id INTEGER NOT NULL DEFAULT 0,
    required_merit_points INTEGER NOT NULL,
    permissions_level INTEGER NOT NULL,
    description TEXT NOT NULL,
    created_at TEXT NOT NULL
);

-- Create merit_activities table
CREATE TABLE IF NOT EXISTS merit_activities (
    id TEXT PRIMARY KEY NOT NULL,
    contributor_id TEXT NOT NULL,
    activity_type INTEGER NOT NULL, -- Enum stored as integer
    merit_change INTEGER NOT NULL,
    description TEXT NOT NULL,
    metadata TEXT, -- JSON data
    created_at TEXT NOT NULL,
    FOREIGN KEY (contributor_id) REFERENCES contributors(id) ON DELETE CASCADE
);

-- Create index on contributor_id and created_at for activity queries
CREATE INDEX IF NOT EXISTS idx_activities_contributor ON merit_activities(contributor_id);
CREATE INDEX IF NOT EXISTS idx_activities_created_at ON merit_activities(created_at);

-- Insert default roles
INSERT OR IGNORE INTO merit_roles (id, role_name, discord_role_id, required_merit_points, permissions_level, description, created_at)
VALUES 
    ('550e8400-e29b-41d4-a716-446655440001', 'Newcomer', 0, 0, 1, 'New member of the community', datetime('now')),
    ('550e8400-e29b-41d4-a716-446655440002', 'Contributor', 0, 50, 2, 'Has made meaningful contributions', datetime('now')),
    ('550e8400-e29b-41d4-a716-446655440003', 'Active Contributor', 0, 150, 3, 'Regularly contributes high-quality code', datetime('now')),
    ('550e8400-e29b-41d4-a716-446655440004', 'Core Contributor', 0, 500, 4, 'Key member driving project forward', datetime('now')),
    ('550e8400-e29b-41d4-a716-446655440005', 'Maintainer', 0, 1000, 5, 'Project maintainer with full permissions', datetime('now'));
