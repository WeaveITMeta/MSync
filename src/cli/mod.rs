//! The command-line front door (contract 13.6): `msync serve | init | check | plan |
//! doctor | token create | --help`. Hand-rolled argument parsing, no new dependency.
//!
//! Every command except `serve` and `token create` is pure file work and never touches the
//! network or the database. `doctor` reads the environment and prints, for each connector,
//! whether it is configured and which variables are missing. It never prints a secret value.

pub mod templates;

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use chrono::NaiveDate;

use crate::cra::authz::requirements;
use crate::cra::local::read_dir_files;
use crate::cra::standing::active_positions;
use crate::cra::{plan_change, MemberStatus, Snapshot};

pub const USAGE: &str = "\
MSync (MeritSync): a management hierarchy, merit and payroll engine driven by a repository.

Usage:
  msync [serve]                          Start the service (the default)
  msync init --template <name> --dir <path>
                                         Write .msync/ and the CI files from a template;
                                         refuses to overwrite anything
  msync check <dir>                      Parse and validate <dir>/.msync/; every error with
                                         its file and reason; exit 1 on failure
  msync plan <old-dir> <new-dir>         The change preview from old to new and who would
                                         have to approve it (old may have no .msync/)
  msync doctor                           Each connector: configured or not, and every missing
                                         variable. Never prints a secret value
  msync token create --label <text> --member <id> --scopes <a,b,...>
                     [--payee-keys <k1,k2>] [--expires <RFC 3339 time>]
                                         Create an API token in the configured database
                                         (DATABASE_URL) and print it once
  msync templates                        List the templates
  msync --help | -h                      This text
  msync --version | -V                   The version

Templates: open-source, source-available, hybrid, closed-source, merit-dao.
Docs: AGENTS.md, docs/QUICKSTART.md, docs/CRA_CONFIG.md, openapi.yaml.
";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Serve,
    Help,
    Version,
    Templates,
    Init { template: String, dir: PathBuf },
    Check { dir: PathBuf },
    Plan { old: PathBuf, new: PathBuf },
    Doctor,
    TokenCreate { label: String, member: String, scopes: Vec<String>, payee_keys: Vec<String>, expires: Option<String> },
}

/// Split `--flag value` / `--flag=value` options from positional arguments. Every flag
/// must be in `known`; a flag given twice or with no value is an error.
fn options(args: &[String], known: &[&str]) -> Result<(Vec<(String, String)>, Vec<String>), String> {
    let mut opts: Vec<(String, String)> = Vec::new();
    let mut pos = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if let Some(rest) = a.strip_prefix("--") {
            let (name, value) = match rest.split_once('=') {
                Some((n, v)) => (n.to_string(), v.to_string()),
                None => {
                    i += 1;
                    let v = args.get(i).ok_or_else(|| format!("--{rest} needs a value"))?;
                    if v.starts_with("--") {
                        return Err(format!("--{rest} needs a value"));
                    }
                    (rest.to_string(), v.clone())
                }
            };
            if !known.contains(&name.as_str()) {
                return Err(format!("unknown option --{name}"));
            }
            if opts.iter().any(|(n, _)| *n == name) {
                return Err(format!("--{name} given twice"));
            }
            opts.push((name, value));
        } else if a.starts_with('-') && a.len() > 1 {
            return Err(format!("unknown option {a}"));
        } else {
            pos.push(a.clone());
        }
        i += 1;
    }
    Ok((opts, pos))
}

fn take(opts: &[(String, String)], name: &str) -> Option<String> {
    opts.iter().find(|(n, _)| n == name).map(|(_, v)| v.clone())
}

fn required(opts: &[(String, String)], name: &str, cmd: &str) -> Result<String, String> {
    take(opts, name).filter(|v| !v.trim().is_empty()).ok_or_else(|| format!("msync {cmd} needs --{name}"))
}

fn list(v: &str) -> Vec<String> {
    v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
}

fn no_positionals(pos: &[String], cmd: &str) -> Result<(), String> {
    match pos.first() {
        Some(p) => Err(format!("msync {cmd}: unexpected argument '{p}'")),
        None => Ok(()),
    }
}

/// Parse the arguments after the program name.
pub fn parse(args: &[String]) -> Result<Command, String> {
    if args.iter().any(|a| a == "--help" || a == "-h") {
        return Ok(Command::Help);
    }
    let Some(first) = args.first() else { return Ok(Command::Serve) };
    let rest = &args[1..];
    match first.as_str() {
        "--version" | "-V" | "version" => Ok(Command::Version),
        "help" => Ok(Command::Help),
        "serve" => {
            let (_, pos) = options(rest, &[])?;
            no_positionals(&pos, "serve")?;
            Ok(Command::Serve)
        }
        "templates" => {
            let (_, pos) = options(rest, &[])?;
            no_positionals(&pos, "templates")?;
            Ok(Command::Templates)
        }
        "init" => {
            let (opts, pos) = options(rest, &["template", "dir"])?;
            no_positionals(&pos, "init")?;
            Ok(Command::Init { template: required(&opts, "template", "init")?, dir: PathBuf::from(required(&opts, "dir", "init")?) })
        }
        "check" => {
            let (_, pos) = options(rest, &[])?;
            match pos.as_slice() {
                [dir] => Ok(Command::Check { dir: PathBuf::from(dir) }),
                _ => Err("msync check needs exactly one directory: msync check <dir>".into()),
            }
        }
        "plan" => {
            let (_, pos) = options(rest, &[])?;
            match pos.as_slice() {
                [old, new] => Ok(Command::Plan { old: PathBuf::from(old), new: PathBuf::from(new) }),
                _ => Err("msync plan needs two directories: msync plan <old-dir> <new-dir>".into()),
            }
        }
        "doctor" => {
            let (_, pos) = options(rest, &[])?;
            no_positionals(&pos, "doctor")?;
            Ok(Command::Doctor)
        }
        "token" => {
            match rest.first().map(String::as_str) {
                Some("create") => {}
                Some(other) => return Err(format!("unknown token command '{other}'; use: msync token create ...")),
                None => return Err("use: msync token create --label <text> --member <id> --scopes <a,b>".into()),
            }
            let (opts, pos) = options(&rest[1..], &["label", "member", "scopes", "payee-keys", "expires"])?;
            no_positionals(&pos, "token create")?;
            let scopes = list(&required(&opts, "scopes", "token create")?);
            if scopes.is_empty() {
                return Err("msync token create needs at least one scope in --scopes".into());
            }
            Ok(Command::TokenCreate {
                label: required(&opts, "label", "token create")?,
                member: required(&opts, "member", "token create")?,
                scopes,
                payee_keys: take(&opts, "payee-keys").map(|v| list(&v)).unwrap_or_default(),
                expires: take(&opts, "expires"),
            })
        }
        other => Err(format!("unknown command '{other}'; run msync --help")),
    }
}

// ------------------------------------------------------------------ init

/// Write the template into `dir`. Refuses (writing nothing) when any target file exists.
/// Returns the paths written.
pub fn init(template: &str, dir: &Path) -> Result<Vec<PathBuf>, String> {
    let t = templates::get(template).ok_or_else(|| {
        format!("unknown template '{template}'; one of: {}", templates::names().join(", "))
    })?;
    let targets: Vec<(PathBuf, &str)> = t.files.iter().map(|(rel, body)| (dir.join(rel), *body)).collect();
    let existing: Vec<String> = targets.iter().filter(|(p, _)| p.exists()).map(|(p, _)| p.display().to_string()).collect();
    if !existing.is_empty() {
        return Err(format!("refusing to overwrite existing files: {}", existing.join(", ")));
    }
    let mut written = Vec::new();
    for (path, body) in targets {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        // create_new: even a file that appeared after the check is never overwritten.
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        f.write_all(body.as_bytes()).map_err(|e| format!("{}: {e}", path.display()))?;
        written.push(path);
    }
    Ok(written)
}

// ------------------------------------------------------------------ check

/// The marker every template placeholder starts with.
pub const PLACEHOLDER: &str = "REPLACE_ME_";

/// Every `REPLACE_ME_...` left in a non-comment line, as `file:line: token`.
pub fn placeholders(files: &crate::cra::CraFiles) -> Vec<String> {
    let mut out = Vec::new();
    for (kind, text) in files.ordered() {
        for (n, line) in text.lines().enumerate() {
            if line.trim_start().starts_with('#') {
                continue;
            }
            let mut rest = line;
            while let Some(i) = rest.find(PLACEHOLDER) {
                let tail = &rest[i..];
                let end = tail.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).unwrap_or(tail.len());
                out.push(format!("{}:{}: placeholder `{}` is not filled in", kind.file_name(), n + 1, &tail[..end]));
                rest = &tail[end..];
            }
        }
    }
    out
}

/// Parse and validate `<dir>/.msync/`. `Ok(snapshot)` or every error, each naming its file.
pub fn check_dir(dir: &Path, today: NaiveDate) -> Result<Snapshot, Vec<String>> {
    let base = dir.join(".msync");
    let files = read_dir_files(dir)?;
    let mut errors: Vec<String> =
        placeholders(&files).into_iter().map(|e| format!("{}/{e}", base.display())).collect();
    match crate::cra::load(&files, today) {
        Ok(s) if errors.is_empty() => Ok(s),
        Ok(_) => Err(errors),
        Err(es) => {
            errors.extend(es.iter().map(|e| format!("{}/{}: {:?}: {}", base.display(), e.file.file_name(), e.code, e.message)));
            Err(errors)
        }
    }
}

pub fn check_report(dir: &Path, today: NaiveDate) -> (String, bool) {
    match check_dir(dir, today) {
        Ok(s) => (
            format!(
                "OK {}: org `{}` ({}), {} roles, {} members, content hash {}\n",
                dir.join(".msync").display(),
                s.org.org.slug,
                s.mode_label.as_str(),
                s.roles.roles.len(),
                s.members.members.len(),
                s.content_hash
            ),
            true,
        ),
        Err(errors) => {
            let mut out = String::new();
            for e in &errors {
                let _ = writeln!(out, "error: {e}");
            }
            let _ = writeln!(out, "FAILED: {} error(s) in {}", errors.len(), dir.join(".msync").display());
            (out, false)
        }
    }
}

// ------------------------------------------------------------------ plan

/// Members of `old` (active, with a position) who may act on or approve a change that
/// needs `permission` and a rank strictly above `rank`.
fn eligible(old: &Snapshot, permission: &str, rank: u32, today: NaiveDate) -> Vec<String> {
    let mut out = Vec::new();
    for m in &old.members.members {
        if m.status != MemberStatus::Active {
            continue;
        }
        let positions = active_positions(old, &m.id, today);
        let r = positions.iter().map(|(role, _)| role.rank).max().unwrap_or(0);
        let has = positions.iter().any(|(role, _)| role.permissions.iter().any(|p| p == permission));
        if r > rank && has {
            out.push(format!("`{}` (rank {r})", m.id));
        }
    }
    out
}

/// The change preview from `old_dir` (which may have no `.msync/`: the first snapshot) to
/// `new_dir`, with who could act and approve. `Err` when either side does not validate.
pub fn plan_report(old_dir: &Path, new_dir: &Path, today: NaiveDate) -> Result<String, Vec<String>> {
    let old = if old_dir.join(".msync").is_dir() {
        Some(check_dir(old_dir, today).map_err(|es| {
            let mut v = vec![format!("the old side {} does not validate:", old_dir.display())];
            v.extend(es);
            v
        })?)
    } else {
        None
    };
    let new = check_dir(new_dir, today)?;
    let plan = plan_change(old.as_ref(), &new);
    let mut out = String::new();
    if plan.is_empty() {
        let _ = writeln!(out, "## MSync CRA change preview: `{}`\n\nNo change: the new .msync/ files change nothing in the hierarchy.", new.org.org.slug);
        return Ok(out);
    }
    out.push_str(&plan.render_markdown());
    let _ = writeln!(out, "### Who can approve\n");
    match &old {
        None => {
            let _ = writeln!(
                out,
                "This is the first snapshot. Only an owner from engine configuration (`MSYNC_OWNER_MEMBER_IDS`, in the form `github:<login>` or `gitlab:<login>`) may load it."
            );
        }
        Some(old) => {
            let _ = writeln!(
                out,
                "Owners (`MSYNC_OWNER_MEMBER_IDS`, rank 1000) can always act. From the active snapshot, members holding the permission with a rank strictly above the change:\n"
            );
            for r in requirements(&plan) {
                let actors = eligible(old, r.permission, r.rank, today);
                let line = if r.two_person {
                    let approvers = eligible(old, r.approver_permission, r.rank, today);
                    format!(
                        "- {}: merged by {}; approved in review by a second member who is not the author or affected: {}",
                        r.item,
                        or_none(&actors),
                        or_none(&approvers)
                    )
                } else {
                    format!("- {}: merged or pushed by {}", r.item, or_none(&actors))
                };
                let _ = writeln!(out, "{line}");
            }
            let _ = writeln!(out, "\nA two-person change must arrive through a merged pull or merge request; a direct push is refused.");
        }
    }
    Ok(out)
}

fn or_none(v: &[String]) -> String {
    if v.is_empty() {
        "owners only".into()
    } else {
        v.join(", ")
    }
}

// ------------------------------------------------------------------ doctor

struct Connector {
    name: &'static str,
    /// Every one of these must be set for the connector to be on.
    required: &'static [&'static str],
    /// Set, these add a capability; their absence is named but is not a problem.
    optional: &'static [&'static str],
    note: &'static str,
}

const CONNECTORS: &[Connector] = &[
    Connector { name: "Bootstrap admin token", required: &["MSYNC_ADMIN_TOKEN"], optional: &[], note: "creates the first API token over REST (or use `msync token create`)" },
    Connector { name: "Owners", required: &["MSYNC_OWNER_MEMBER_IDS"], optional: &[], note: "rank 1000; the github:<login> form bootstraps the first CRA snapshot" },
    Connector { name: "Discord bot", required: &["DISCORD_TOKEN", "DISCORD_APPLICATION_ID"], optional: &["DISCORD_GUILD_ID"], note: "docs/DISCORD.md" },
    Connector { name: "Slack app", required: &["MSYNC_SLACK_SIGNING_SECRET"], optional: &["MSYNC_SLACK_BOT_TOKEN", "MSYNC_SLACK_TEAM_ID"], note: "docs/SLACK.md; the bot token is needed for user groups and admin checks" },
    Connector { name: "GitHub webhooks (Foundry API)", required: &["GITHUB_WEBHOOK_SECRET"], optional: &["GITHUB_TOKEN"], note: "POST /github/webhook; the token reads a GitHub CRA repository" },
    Connector { name: "GitLab webhooks (Foundry API)", required: &["GITLAB_WEBHOOK_SECRET"], optional: &["GITLAB_TOKEN"], note: "POST /gitlab/webhook; the token reads a GitLab CRA repository" },
    Connector { name: "Stripe Connect payouts", required: &["STRIPE_SECRET_KEY", "STRIPE_WEBHOOK_SECRET"], optional: &["MSYNC_PUBLIC_URL"], note: "test keys only unless MSYNC_STRIPE_ALLOW_LIVE=true" },
    Connector { name: "Portal sync API", required: &["MSYNC_SUBJECT_KEY"], optional: &[], note: "docs/PORTAL_API.md; plus MSYNC_CONSUMER_SECRET_<KEY>_<KEYID> per consumer" },
    Connector { name: "MSync OAuth/OIDC provider", required: &["MSYNC_KEY_ENCRYPTION_KEY"], optional: &[], note: "docs/SIGN_IN.md; /oauth/* answers 503 without it" },
    Connector { name: "Sign in with GitHub", required: &["MSYNC_OAUTH_GITHUB_CLIENT_ID", "MSYNC_OAUTH_GITHUB_CLIENT_SECRET"], optional: &[], note: "" },
    Connector { name: "Sign in with GitLab", required: &["MSYNC_OAUTH_GITLAB_CLIENT_ID", "MSYNC_OAUTH_GITLAB_CLIENT_SECRET"], optional: &[], note: "" },
    Connector { name: "Sign in with Discord", required: &["MSYNC_OAUTH_DISCORD_CLIENT_ID", "MSYNC_OAUTH_DISCORD_CLIENT_SECRET"], optional: &[], note: "" },
    Connector { name: "Sign in with Slack", required: &["MSYNC_OAUTH_SLACK_CLIENT_ID", "MSYNC_OAUTH_SLACK_CLIENT_SECRET"], optional: &[], note: "" },
    Connector { name: "Sign in with Google", required: &["MSYNC_OAUTH_GOOGLE_CLIENT_ID", "MSYNC_OAUTH_GOOGLE_CLIENT_SECRET"], optional: &[], note: "" },
];

/// The doctor report over an environment snapshot. Values are only ever tested for
/// presence (and, for a few non-secret switches, compared to `true`); no value is printed
/// except `DATABASE_URL` with any password removed, `MSYNC_PUBLIC_URL` and the CRA repository spec.
/// Returns the text and whether anything is wrong (a problem, not a missing optional).
pub fn doctor_report(vars: &[(String, String)]) -> (String, bool) {
    let get = |k: &str| -> Option<String> {
        let v = vars.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone()).or_else(|| {
            crate::config::legacy_env_name(k).and_then(|old| vars.iter().find(|(n, _)| *n == old).map(|(_, v)| v.clone()))
        })?;
        if v.trim().is_empty() {
            None
        } else {
            Some(v)
        }
    };
    let set = |k: &str| get(k).is_some();
    let mut out = String::new();
    let mut problems = 0usize;
    let _ = writeln!(out, "MSync doctor (values are never printed)\n");

    let dev = crate::config::dev_mode_from(|k| get(k));
    let _ = writeln!(out, "[{}] Dev mode: {}", if dev { "!!" } else { "ok" }, if dev { "ON (MSYNC_DEV_MODE=true): never on a reachable host" } else { "off" });
    let db = get("DATABASE_URL").unwrap_or_else(|| format!("{} (default)", crate::config::DEFAULT_DATABASE_URL));
    let _ = writeln!(out, "[ok] Database: {}", redact_url(&db));
    match get("MSYNC_PUBLIC_URL") {
        Some(u) => {
            let _ = writeln!(out, "[ok] Public URL: {}", redact_url(&u));
        }
        None => {
            let _ = writeln!(out, "[--] Public URL: not set (MSYNC_PUBLIC_URL; default http://localhost:8080)");
        }
    }

    // CRA source
    match (get("MSYNC_CRA_REPO"), get("MSYNC_CRA_LOCAL_DIR")) {
        (Some(_), Some(_)) => {
            problems += 1;
            let _ = writeln!(out, "[XX] CRA: both MSYNC_CRA_REPO and MSYNC_CRA_LOCAL_DIR are set; set one");
        }
        (Some(r), None) => {
            let _ = writeln!(out, "[ok] CRA repository: {r} (branch {})", get("MSYNC_CRA_BRANCH").unwrap_or_else(|| "main".into()));
        }
        (None, Some(d)) if dev => {
            let _ = writeln!(out, "[!!] CRA: local directory {d} (dev mode only)");
        }
        (None, Some(_)) => {
            problems += 1;
            let _ = writeln!(out, "[XX] CRA: MSYNC_CRA_LOCAL_DIR is set without MSYNC_DEV_MODE=true; the service refuses to start");
        }
        (None, None) => {
            let _ = writeln!(out, "[--] CRA: not configured: missing MSYNC_CRA_REPO (or MSYNC_CRA_LOCAL_DIR in dev mode)");
        }
    }

    for c in CONNECTORS {
        let missing: Vec<&str> = c.required.iter().copied().filter(|k| !set(k)).collect();
        let opt_missing: Vec<&str> = c.optional.iter().copied().filter(|k| !set(k)).collect();
        let note = if c.note.is_empty() { String::new() } else { format!(" ({})", c.note) };
        if missing.is_empty() {
            let extra = if opt_missing.is_empty() { String::new() } else { format!("; optional, not set: {}", opt_missing.join(", ")) };
            let _ = writeln!(out, "[ok] {}: configured{extra}{note}", c.name);
        } else if missing.len() == c.required.len() {
            let _ = writeln!(out, "[--] {}: not configured: missing {}{note}", c.name, missing.join(", "));
        } else {
            problems += 1;
            let _ = writeln!(out, "[XX] {}: partly configured: missing {}{note}", c.name, missing.join(", "));
        }
    }
    let consumers = vars.iter().filter(|(k, v)| k.starts_with("MSYNC_CONSUMER_SECRET_") && !v.trim().is_empty()).count();
    let _ = writeln!(out, "[{}] Portal consumer secrets: {consumers} set (MSYNC_CONSUMER_SECRET_<KEY>_<KEYID>)", if consumers > 0 { "ok" } else { "--" });

    // Chat platforms: at least one outside dev mode.
    let discord = set("DISCORD_TOKEN");
    let slack = set("MSYNC_SLACK_SIGNING_SECRET");
    if !discord && !slack {
        if dev {
            let _ = writeln!(out, "[!!] Chat platforms: none (allowed in dev mode only)");
        } else {
            problems += 1;
            let _ = writeln!(out, "[XX] Chat platforms: none; set Discord or Slack, or MSYNC_DEV_MODE=true for local use");
        }
    }

    // Stripe safety.
    let key = get("STRIPE_SECRET_KEY").unwrap_or_default();
    let live_key = key.contains("_live_");
    let allow_live = get("MSYNC_STRIPE_ALLOW_LIVE").map_or(false, |v| v.trim() == "true");
    if !key.is_empty() {
        let kind = if live_key { "a LIVE key" } else { "a test key" };
        let _ = writeln!(out, "[{}] Stripe key kind: {kind}{}", if live_key { "!!" } else { "ok" }, if live_key && !allow_live { "; refused until MSYNC_STRIPE_ALLOW_LIVE=true" } else { "" });
    }
    if dev && (live_key || allow_live) {
        problems += 1;
        let _ = writeln!(out, "[XX] Dev mode with a live Stripe configuration: the service refuses to start");
    }

    // MCP execution switch.
    if get("MSYNC_MCP_ALLOW_EXECUTE").map_or(false, |v| v.trim() == "true") {
        let _ = writeln!(out, "[!!] MSYNC_MCP_ALLOW_EXECUTE=true: agents can execute payout runs over MCP");
    } else {
        let _ = writeln!(out, "[ok] MCP payout execution: off");
    }

    let _ = writeln!(out, "\n{}", if problems == 0 { "No problems found.".to_string() } else { format!("{problems} problem(s) found.") });
    (out, problems > 0)
}

/// A URL with any `user:password@` removed, so a database password is never printed.
pub fn redact_url(u: &str) -> String {
    if let Some((scheme, rest)) = u.split_once("://") {
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, ""),
        };
        if let Some((_, host)) = authority.rsplit_once('@') {
            return format!("{scheme}://***@{host}{path}");
        }
    }
    u.to_string()
}

// ------------------------------------------------------------------ run

/// Run a command that is not `serve`. Returns the process exit code.
pub async fn run(cmd: Command) -> i32 {
    let today = chrono::Utc::now().date_naive();
    match cmd {
        Command::Serve => unreachable!("serve is run by main"),
        Command::Help => {
            print!("{USAGE}");
            0
        }
        Command::Version => {
            println!("msync {}", env!("CARGO_PKG_VERSION"));
            0
        }
        Command::Templates => {
            for t in templates::TEMPLATES {
                println!("{:<18} {}", t.name, t.summary);
            }
            0
        }
        Command::Init { template, dir } => match init(&template, &dir) {
            Ok(paths) => {
                for p in &paths {
                    println!("wrote {}", p.display());
                }
                println!(
                    "\nNext: replace every {PLACEHOLDER}... value in {}/.msync/ (ids, names, owners), then run:\n  msync check {}",
                    dir.display(),
                    dir.display()
                );
                0
            }
            Err(e) => {
                eprintln!("error: {e}");
                1
            }
        },
        Command::Check { dir } => {
            let (text, ok) = check_report(&dir, today);
            if ok {
                print!("{text}");
                0
            } else {
                eprint!("{text}");
                1
            }
        }
        Command::Plan { old, new } => match plan_report(&old, &new, today) {
            Ok(text) => {
                print!("{text}");
                0
            }
            Err(errors) => {
                for e in errors {
                    eprintln!("error: {e}");
                }
                1
            }
        },
        Command::Doctor => {
            let vars: Vec<(String, String)> = std::env::vars().collect();
            let (text, problems) = doctor_report(&vars);
            print!("{text}");
            // The full startup configuration check, as `msync serve` would run it.
            match crate::config::Config::from_env() {
                Ok(_) => println!("Startup configuration: OK"),
                Err(e) => println!("Startup configuration: FAILS: {e}"),
            }
            i32::from(problems)
        }
        Command::TokenCreate { label, member, scopes, payee_keys, expires } => {
            token_create(label, member, scopes, payee_keys, expires).await
        }
    }
}

async fn token_create(label: String, member: String, scopes: Vec<String>, payee_keys: Vec<String>, expires: Option<String>) -> i32 {
    let expires_at = match expires.as_deref().map(chrono::DateTime::parse_from_rfc3339) {
        None => None,
        Some(Ok(t)) => Some(t.with_timezone(&chrono::Utc)),
        Some(Err(e)) => {
            eprintln!("error: --expires is not an RFC 3339 time: {e}");
            return 2;
        }
    };
    let url = crate::config::env_var("DATABASE_URL").unwrap_or_else(|| crate::config::DEFAULT_DATABASE_URL.to_string());
    let pool = match crate::server::open_database(&url).await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {e:#}");
            return 1;
        }
    };
    let directory = crate::auth::EnvDirectory::from_env();
    let new = crate::auth::tokens::NewToken { label, member_id: member, scopes, payee_keys, expires_at };
    match crate::auth::tokens::create_token(&pool, &directory, crate::auth::tokens::TokenCreator::Bootstrap, new, chrono::Utc::now()).await {
        Ok(t) => {
            eprintln!("Created token {} for member {} with scopes {}. It is shown once; store it now.", t.id, t.member_id, t.scopes.join(","));
            println!("{}", t.token);
            0
        }
        Err(e) => {
            eprintln!("error: {e}");
            1
        }
    }
}

#[cfg(test)]
mod tests;
