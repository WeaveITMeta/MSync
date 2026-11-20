use super::*;

fn args(s: &str) -> Vec<String> {
    s.split_whitespace().map(String::from).collect()
}

fn today() -> NaiveDate {
    chrono::Utc::now().date_naive()
}

fn temp_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("msync-cli-{tag}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

// ------------------------------------------------------------------ parsing

#[test]
fn bare_msync_and_serve_start_the_service() {
    assert_eq!(parse(&[]).unwrap(), Command::Serve);
    assert_eq!(parse(&args("serve")).unwrap(), Command::Serve);
    assert!(parse(&args("serve extra")).is_err());
}

#[test]
fn help_and_version() {
    assert_eq!(parse(&args("--help")).unwrap(), Command::Help);
    assert_eq!(parse(&args("-h")).unwrap(), Command::Help);
    assert_eq!(parse(&args("check --help")).unwrap(), Command::Help);
    assert_eq!(parse(&args("help")).unwrap(), Command::Help);
    assert_eq!(parse(&args("--version")).unwrap(), Command::Version);
    assert!(USAGE.contains("msync init --template"));
}

#[test]
fn init_needs_a_template_and_a_dir() {
    assert_eq!(
        parse(&args("init --template open-source --dir ./x")).unwrap(),
        Command::Init {
            template: "open-source".into(),
            dir: PathBuf::from("./x")
        }
    );
    assert_eq!(
        parse(&args("init --dir=./x --template=hybrid")).unwrap(),
        Command::Init {
            template: "hybrid".into(),
            dir: PathBuf::from("./x")
        }
    );
    assert!(parse(&args("init --template open-source")).is_err());
    assert!(parse(&args("init --dir x")).is_err());
    assert!(
        parse(&args("init --template")).is_err(),
        "a flag with no value"
    );
    assert!(
        parse(&args("init --template a --template b --dir x")).is_err(),
        "a flag given twice"
    );
    assert!(
        parse(&args("init --template a --dir x --force")).is_err(),
        "no --force: init never overwrites"
    );
}

#[test]
fn check_and_plan_take_directories() {
    assert_eq!(
        parse(&args("check ./org")).unwrap(),
        Command::Check {
            dir: PathBuf::from("./org")
        }
    );
    assert!(parse(&args("check")).is_err());
    assert!(parse(&args("check a b")).is_err());
    assert_eq!(
        parse(&args("plan old new")).unwrap(),
        Command::Plan {
            old: PathBuf::from("old"),
            new: PathBuf::from("new")
        }
    );
    assert!(parse(&args("plan old")).is_err());
}

#[test]
fn token_create_parses_scopes_and_refuses_unknown_options() {
    assert_eq!(
        parse(&args(
            "token create --label first --member founder --scopes hierarchy.read,merit.read"
        ))
        .unwrap(),
        Command::TokenCreate {
            label: "first".into(),
            member: "founder".into(),
            scopes: vec!["hierarchy.read".into(), "merit.read".into()],
            payee_keys: vec![],
            expires: None,
        }
    );
    assert!(
        parse(&args("token create --label a --member b")).is_err(),
        "scopes are required"
    );
    assert!(parse(&args("token create --label a --member b --scopes ,")).is_err());
    assert!(parse(&args(
        "token create --label a --member b --scopes x --secret y"
    ))
    .is_err());
    assert!(parse(&args("token list")).is_err());
    assert!(parse(&args("frobnicate")).is_err());
    assert!(parse(&args("doctor -x")).is_err());
    assert_eq!(parse(&args("doctor")).unwrap(), Command::Doctor);
}

// ------------------------------------------------------------------ templates

/// A sample value for every placeholder a template may use. Dates are relative to today so
/// grant expiry rules hold whenever the test runs.
fn sample(token: &str) -> String {
    let in_days = |d: i64| {
        (today() + chrono::Duration::days(d))
            .format("%Y-%m-%d")
            .to_string()
    };
    let snowflake = |n: u64| (100000000000000000u64 + n).to_string();
    let hash = token
        .bytes()
        .fold(0u64, |a, b| a.wrapping_mul(31).wrapping_add(b as u64))
        % 1_000_000;
    match token {
        t if t.contains("DATE") || t.contains("EXPIRES") => in_days(60),
        t if t.contains("SLACK_TEAM") => "T1000000000".into(),
        t if t.contains("SLACK_CHANNEL") => format!("C{:010}", hash),
        t if t.contains("SLACK_USERGROUP") => format!("S{:010}", hash),
        t if t.contains("SLACK_USER") => format!("U{:010}", hash),
        t if t.contains("DISCORD") => snowflake(hash + 1),
        t if t.contains("GITLAB") => format!("gl-{}", hash),
        t if t.contains("GITHUB") || t.contains("LOGIN") => format!("gh-{}", hash),
        t if t.contains("REPO") => format!("example-org/repo-{}", hash),
        t if t.contains("SLUG") => "example-org".into(),
        t if t.contains("HOST") => "gitlab.example.org".into(),
        t if t.ends_with("_ID") || t.ends_with("_KEY") => format!("member-{}", hash),
        _ => format!("Example {}", hash),
    }
}

/// Replace every placeholder in a template file with [`sample`]. Quoted values only, so
/// the substitution never changes the TOML shape.
fn fill(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find(PLACEHOLDER) {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        let end = tail
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(tail.len());
        out.push_str(&sample(&tail[..end]));
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

fn write_filled(t: &templates::Template, dir: &Path) {
    for (rel, body) in t.files {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let text = if rel.ends_with(".toml") {
            fill(body)
        } else {
            body.to_string()
        };
        std::fs::write(&p, text).unwrap();
    }
}

#[test]
fn there_are_five_templates_with_every_file() {
    assert_eq!(
        templates::names(),
        vec![
            "open-source",
            "source-available",
            "hybrid",
            "closed-source",
            "merit-dao"
        ]
    );
    for t in templates::TEMPLATES {
        assert_eq!(t.files.len(), 8, "{}", t.name);
        for (rel, body) in t.files {
            assert!(body.len() > 200, "{}/{rel} looks like a stub", t.name);
            assert!(
                !body.contains('\u{2014}'),
                "{}/{rel} contains an em dash",
                t.name
            );
        }
    }
}

#[test]
fn every_template_loads_after_placeholder_substitution() {
    for t in templates::TEMPLATES {
        let dir = temp_dir(t.name);
        write_filled(t, &dir);
        let (text, ok) = check_report(&dir, today());
        assert!(
            ok,
            "template {} does not validate after substitution:\n{text}",
            t.name
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn an_unfilled_template_fails_check_and_names_each_placeholder() {
    for t in templates::TEMPLATES {
        let dir = temp_dir(&format!("raw-{}", t.name));
        init(t.name, &dir).unwrap();
        let (text, ok) = check_report(&dir, today());
        assert!(
            !ok,
            "template {} passes check with its placeholders still in",
            t.name
        );
        assert!(text.contains("placeholder `REPLACE_ME_"), "{text}");
        assert!(
            text.contains("org.toml:"),
            "errors name the file and line: {text}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn every_template_mode_matches_its_name() {
    let want = [
        ("open-source", "open"),
        ("source-available", "source_available"),
        ("hybrid", "hybrid"),
        ("closed-source", "closed"),
        ("merit-dao", "open"),
    ];
    for (name, mode) in want {
        let t = templates::get(name).unwrap();
        let dir = temp_dir(&format!("mode-{name}"));
        write_filled(t, &dir);
        let s = check_dir(&dir, today()).unwrap();
        assert_eq!(s.mode_label.as_str(), mode, "{name}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn the_ci_files_are_identical_in_every_template() {
    let first = templates::TEMPLATES[0].files;
    for t in templates::TEMPLATES {
        for (rel, body) in t.files.iter().filter(|(r, _)| r.ends_with(".yml")) {
            let same = first.iter().find(|(r, _)| r == rel).unwrap().1;
            assert_eq!(
                *body,
                same,
                "{}/{rel} differs from {}",
                t.name,
                templates::TEMPLATES[0].name
            );
        }
    }
}

// ------------------------------------------------------------------ init

#[test]
fn init_writes_every_file_and_refuses_to_overwrite() {
    let dir = temp_dir("init");
    let written = init("open-source", &dir).unwrap();
    assert_eq!(written.len(), 8);
    assert!(dir.join(".msync/org.toml").is_file());
    let before = std::fs::read_to_string(dir.join(".msync/org.toml")).unwrap();

    // A second init over the same directory refuses and changes nothing.
    std::fs::write(dir.join(".msync/org.toml"), "mine").unwrap();
    let err = init("hybrid", &dir).unwrap_err();
    assert!(err.contains("refusing to overwrite"), "{err}");
    assert_eq!(
        std::fs::read_to_string(dir.join(".msync/org.toml")).unwrap(),
        "mine"
    );
    assert_ne!(before, "mine");

    // One existing file anywhere in the set is enough to refuse, and nothing else is written.
    let dir2 = temp_dir("init-one");
    std::fs::create_dir_all(dir2.join(".msync")).unwrap();
    std::fs::write(dir2.join(".msync/README.md"), "keep").unwrap();
    assert!(init("open-source", &dir2).is_err());
    assert!(
        !dir2.join(".msync/org.toml").exists(),
        "nothing is written when init refuses"
    );

    assert!(init("no-such-template", &temp_dir("init-unknown"))
        .unwrap_err()
        .contains("unknown template"));
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&dir2);
}

// ------------------------------------------------------------------ check

#[test]
fn check_passes_the_examples_and_names_file_and_reason_on_failure() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/cra");
    for ex in ["eustress-engine", "weave-solutions", "voltec-industries"] {
        let (text, ok) = check_report(&root.join(ex), today());
        assert!(ok, "{ex}: {text}");
    }
    let dir = temp_dir("check-bad");
    std::fs::create_dir_all(dir.join(".msync")).unwrap();
    for f in ["org", "roles", "members", "merit", "payouts"] {
        let src = root
            .join("eustress-engine/.msync")
            .join(format!("{f}.toml"));
        std::fs::copy(src, dir.join(".msync").join(format!("{f}.toml"))).unwrap();
    }
    let roles = std::fs::read_to_string(dir.join(".msync/roles.toml")).unwrap();
    std::fs::write(
        dir.join(".msync/roles.toml"),
        roles.replacen("rank = ", "rank = 1000 #", 1),
    )
    .unwrap();
    let (text, ok) = check_report(&dir, today());
    assert!(!ok);
    assert!(
        text.contains("roles.toml") && text.contains("RankOutOfRange"),
        "{text}"
    );

    let missing = temp_dir("check-missing");
    let (text, ok) = check_report(&missing, today());
    assert!(!ok && text.contains("org.toml"), "{text}");
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&missing);
}

// ------------------------------------------------------------------ plan

#[test]
fn plan_from_nothing_is_a_bootstrap_by_an_owner() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/cra");
    let empty = temp_dir("plan-empty");
    let text = plan_report(&empty, &root.join("eustress-engine"), today()).unwrap();
    assert!(text.contains("MSync CRA change preview"), "{text}");
    assert!(text.contains("Only an owner"), "{text}");
    let _ = std::fs::remove_dir_all(&empty);
}

#[test]
fn plan_names_who_can_approve_a_change() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/cra/eustress-engine");
    let new = temp_dir("plan-new");
    std::fs::create_dir_all(new.join(".msync")).unwrap();
    for f in ["org", "roles", "members", "merit", "payouts"] {
        std::fs::copy(
            root.join(".msync").join(format!("{f}.toml")),
            new.join(".msync").join(format!("{f}.toml")),
        )
        .unwrap();
    }
    let payouts = std::fs::read_to_string(new.join(".msync/payouts.toml")).unwrap();
    std::fs::write(
        new.join(".msync/payouts.toml"),
        payouts.replace("min_payout_cents = 500", "min_payout_cents = 600"),
    )
    .unwrap();
    let text = plan_report(&root, &new, today()).unwrap();
    assert!(text.contains("payouts.toml"), "{text}");
    assert!(text.contains("### Who can approve"), "{text}");
    assert!(
        text.contains("approved in review by a second member"),
        "a payout change is two-person: {text}"
    );
    // The same files on both sides: no change.
    let same = plan_report(&root, &root, today()).unwrap();
    assert!(same.contains("No change"), "{same}");
    // An invalid new side is an error, not a preview.
    std::fs::write(new.join(".msync/merit.toml"), "schema = 1\nnot valid").unwrap();
    assert!(plan_report(&root, &new, today()).is_err());
    let _ = std::fs::remove_dir_all(&new);
}

// ------------------------------------------------------------------ doctor

fn vars(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

#[test]
fn doctor_names_every_missing_variable() {
    let (text, problems) = doctor_report(&vars(&[("MSYNC_DEV_MODE", "true")]));
    assert!(
        !problems,
        "dev mode with nothing configured is not a problem:\n{text}"
    );
    for name in [
        "DISCORD_TOKEN",
        "DISCORD_APPLICATION_ID",
        "MSYNC_SLACK_SIGNING_SECRET",
        "GITHUB_WEBHOOK_SECRET",
        "GITLAB_WEBHOOK_SECRET",
        "STRIPE_SECRET_KEY",
        "MSYNC_SUBJECT_KEY",
        "MSYNC_KEY_ENCRYPTION_KEY",
        "MSYNC_ADMIN_TOKEN",
        "MSYNC_CRA_REPO",
    ] {
        assert!(text.contains(name), "doctor does not name {name}:\n{text}");
    }
    // Outside dev mode, no chat platform is a problem; half a connector is a problem.
    let (text, problems) = doctor_report(&vars(&[("DISCORD_TOKEN", "x")]));
    assert!(problems);
    assert!(
        text.contains("partly configured: missing DISCORD_APPLICATION_ID"),
        "{text}"
    );
    let (_, problems) = doctor_report(&vars(&[]));
    assert!(problems, "no chat platform outside dev mode");
    // Dev mode with a live Stripe key is a problem.
    let (text, problems) = doctor_report(&vars(&[
        ("MSYNC_DEV_MODE", "true"),
        ("STRIPE_SECRET_KEY", "sk_live_zzz"),
    ]));
    assert!(problems && text.contains("refuses to start"), "{text}");
}

#[test]
fn doctor_never_prints_a_secret_value() {
    let secrets: Vec<(String, String)> = [
        "MSYNC_ADMIN_TOKEN",
        "DISCORD_TOKEN",
        "MSYNC_SLACK_SIGNING_SECRET",
        "MSYNC_SLACK_BOT_TOKEN",
        "GITHUB_TOKEN",
        "GITHUB_WEBHOOK_SECRET",
        "GITLAB_WEBHOOK_SECRET",
        "GITLAB_TOKEN",
        "STRIPE_SECRET_KEY",
        "STRIPE_WEBHOOK_SECRET",
        "MSYNC_SUBJECT_KEY",
        "MSYNC_KEY_ENCRYPTION_KEY",
        "MSYNC_OAUTH_GITHUB_CLIENT_SECRET",
        "MSYNC_OAUTH_SLACK_CLIENT_SECRET",
        "MSYNC_CONSUMER_SECRET_EUSTRESS_SITE_K1",
        "DSYNC_OAUTH_GOOGLE_CLIENT_SECRET",
    ]
    .iter()
    .enumerate()
    .map(|(i, k)| (k.to_string(), format!("sEcReT{i:02}value{}", "q".repeat(i))))
    .chain([
        ("DISCORD_APPLICATION_ID".to_string(), "123".to_string()),
        (
            "DATABASE_URL".to_string(),
            "postgres://admin:dbPaSsWoRd@db.example.org/msync".to_string(),
        ),
    ])
    .collect();
    let (text, _) = doctor_report(&secrets);
    for (k, v) in &secrets {
        if k == "DISCORD_APPLICATION_ID" {
            continue;
        }
        if k == "DATABASE_URL" {
            assert!(
                !text.contains("dbPaSsWoRd"),
                "doctor printed the database password:\n{text}"
            );
            continue;
        }
        assert!(
            !text.contains(v.as_str()),
            "doctor printed the value of {k}:\n{text}"
        );
    }
    assert!(!text.contains("sEcReT"), "{text}");
    assert_eq!(redact_url("sqlite://dsync.db"), "sqlite://dsync.db");
    assert_eq!(redact_url("postgres://u:p@h/db"), "postgres://***@h/db");
}
