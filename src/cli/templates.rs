//! The CRA templates (`templates/<name>/`), embedded in the binary so `msync init` works
//! from a released binary with no checkout.

pub struct Template {
    pub name: &'static str,
    pub summary: &'static str,
    /// (path relative to the target directory, contents)
    pub files: &'static [(&'static str, &'static str)],
}

macro_rules! template {
    ($name:literal, $summary:literal) => {
        Template {
            name: $name,
            summary: $summary,
            files: &[
                (".msync/org.toml", include_str!(concat!("../../templates/", $name, "/.msync/org.toml"))),
                (".msync/roles.toml", include_str!(concat!("../../templates/", $name, "/.msync/roles.toml"))),
                (".msync/members.toml", include_str!(concat!("../../templates/", $name, "/.msync/members.toml"))),
                (".msync/merit.toml", include_str!(concat!("../../templates/", $name, "/.msync/merit.toml"))),
                (".msync/payouts.toml", include_str!(concat!("../../templates/", $name, "/.msync/payouts.toml"))),
                (".msync/README.md", include_str!(concat!("../../templates/", $name, "/README.md"))),
                (".msync/gitlab-ci.yml", include_str!(concat!("../../templates/", $name, "/.msync/gitlab-ci.yml"))),
                (
                    ".github/workflows/msync-cra.yml",
                    include_str!(concat!("../../templates/", $name, "/.github/workflows/msync-cra.yml")),
                ),
            ],
        }
    };
}

pub const TEMPLATES: &[Template] = &[
    template!("open-source", "Open-source project: everything public, outside contributions earn"),
    template!("source-available", "Public code under a non-open-source license: members-only roster"),
    template!("hybrid", "Public and private repositories: only public repositories earn for outsiders"),
    template!("closed-source", "Private company: internal work only, need-to-know visibility"),
    template!("merit-dao", "Merit-driven collective: tiers earned by merit, pooled payouts by XP"),
];

pub fn get(name: &str) -> Option<&'static Template> {
    TEMPLATES.iter().find(|t| t.name == name)
}

pub fn names() -> Vec<&'static str> {
    TEMPLATES.iter().map(|t| t.name).collect()
}
