//! `openapi.yaml` cannot drift from the routes the service registers (contract 13.6).
//!
//! Two directions:
//!
//! 1. Every route declared in the source (`#[get("...")]` and friends, and
//!    `web::resource("...")` with its method) that the app built by
//!    [`crate::server::configure_routes`] actually routes must be in `openapi.yaml`.
//! 2. Every path and method in `openapi.yaml` must be routed by that same app.
//!
//! "Routed" is decided by sending the request to the real app with a default service that
//! answers a sentinel status: anything but the sentinel (or a 405 without `Allow` from a
//! resource that has the path but not the method) is a registered route. Handlers run without their app
//! data and answer 500 or 401; that still proves the route exists.

use std::collections::BTreeSet;
use std::path::Path;

use actix_web::http::{Method, StatusCode};
use actix_web::{test as atest, web, App, HttpResponse};

const SENTINEL: u16 = 599;
const METHODS: &[&str] = &["get", "post", "put", "patch", "delete"];

type Route = (String, String); // (METHOD, path)

/// Routes declared in the source, skipping test files.
fn declared_routes() -> BTreeSet<Route> {
    let mut out = BTreeSet::new();
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut stack = vec![root];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let p = entry.unwrap().path();
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            let name = p.file_name().unwrap().to_string_lossy().to_string();
            if !name.ends_with(".rs") || name == "tests.rs" || name.ends_with("_tests.rs") {
                continue;
            }
            let text = std::fs::read_to_string(&p).unwrap();
            scan(&text, &mut out);
        }
    }
    out
}

fn scan(text: &str, out: &mut BTreeSet<Route>) {
    for line in text.lines() {
        let t = line.trim();
        for m in METHODS {
            let prefix = format!("#[{m}(\"");
            if let Some(rest) = t.strip_prefix(&prefix) {
                let path = rest.split('"').next().unwrap();
                out.insert((m.to_ascii_uppercase(), path.to_string()));
            }
        }
    }
    // web::resource("path") ... up to the next resource or the end of the statement.
    let mut rest = text;
    while let Some(i) = rest.find("web::resource(\"") {
        let after = &rest[i + "web::resource(\"".len()..];
        let path = after.split('"').next().unwrap().to_string();
        let end = after[1..].find("web::resource(\"").map(|j| j + 1).unwrap_or(after.len());
        let body = &after[..end];
        for m in METHODS {
            let cap = format!("{}{}", m[..1].to_ascii_uppercase(), &m[1..]);
            if body.contains(&format!("web::{m}()")) || body.contains(&format!("guard::{cap}()")) {
                out.insert((m.to_ascii_uppercase(), path.clone()));
            }
        }
        rest = &after[end..];
    }
}

/// Paths and methods in `openapi.yaml`. The file keeps one strict layout: a path is a
/// two-space-indented key starting with `/` under `paths:`, a method a four-space key.
fn documented_routes() -> BTreeSet<Route> {
    let text = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("openapi.yaml")).expect("openapi.yaml at the repository root");
    let mut out = BTreeSet::new();
    let mut in_paths = false;
    let mut current: Option<String> = None;
    for line in text.lines() {
        if !line.starts_with(' ') && !line.trim().is_empty() && !line.starts_with('#') {
            in_paths = line.trim_end() == "paths:";
            current = None;
            continue;
        }
        if !in_paths {
            continue;
        }
        if let Some(k) = line.strip_prefix("  ").filter(|l| !l.starts_with(' ')) {
            let k = k.trim_end();
            if let Some(p) = k.strip_suffix(':') {
                let p = p.trim_matches(|c| c == '"' || c == '\'');
                current = p.starts_with('/').then(|| p.to_string());
            }
            continue;
        }
        if let (Some(path), Some(k)) = (&current, line.strip_prefix("    ").filter(|l| !l.starts_with(' '))) {
            let k = k.trim_end().trim_end_matches(':');
            if METHODS.contains(&k) {
                out.insert((k.to_ascii_uppercase(), path.clone()));
            }
        }
    }
    out
}

/// `/a/{id}/b` -> `/a/probe/b`.
fn concrete(path: &str) -> String {
    path.split('/')
        .map(|seg| if seg.starts_with('{') && seg.ends_with('}') { "probe" } else { seg })
        .collect::<Vec<_>>()
        .join("/")
}

async fn routed(routes: &BTreeSet<Route>) -> BTreeSet<Route> {
    let app = atest::init_service(
        App::new()
            .configure(crate::server::configure_routes)
            .default_service(web::to(|| async { HttpResponse::build(StatusCode::from_u16(SENTINEL).unwrap()).finish() })),
    )
    .await;
    let mut out = BTreeSet::new();
    for (m, p) in routes {
        let req = atest::TestRequest::default()
            .method(Method::from_bytes(m.as_bytes()).unwrap())
            .uri(&concrete(p))
            .to_request();
        let resp = atest::call_service(&app, req).await;
        let s = resp.status().as_u16();
        // A 405 from a resource that has the path but not the method is not a route; a
        // handler that answers 405 on purpose (GET /mcp: no SSE stream) names `Allow`.
        let deliberate_405 = s == 405 && resp.headers().contains_key("allow");
        if s != SENTINEL && (s != 405 || deliberate_405) {
            out.insert((m.clone(), p.clone()));
        }
    }
    out
}

fn show(set: &BTreeSet<Route>) -> String {
    set.iter().map(|(m, p)| format!("  {m} {p}")).collect::<Vec<_>>().join("\n")
}

#[actix_web::test]
async fn openapi_lists_every_registered_route() {
    let declared = declared_routes();
    assert!(declared.len() > 60, "the source scan found only {} routes", declared.len());
    let registered = routed(&declared).await;
    let documented = documented_routes();
    let missing: BTreeSet<Route> = registered.difference(&documented).cloned().collect();
    assert!(missing.is_empty(), "registered but missing from openapi.yaml:\n{}", show(&missing));
}

#[actix_web::test]
async fn openapi_lists_no_route_that_does_not_exist() {
    let documented = documented_routes();
    assert!(documented.len() > 60, "openapi.yaml parsed to only {} operations", documented.len());
    let routed = routed(&documented).await;
    let extra: BTreeSet<Route> = documented.difference(&routed).cloned().collect();
    assert!(extra.is_empty(), "in openapi.yaml but not routed by the app:\n{}", show(&extra));
    // Every documented route is also declared in the source (so it is not matched only by
    // a wider pattern such as /api/v1/members/{subject}).
    let declared = declared_routes();
    let undeclared: BTreeSet<Route> = documented.difference(&declared).cloned().collect();
    assert!(undeclared.is_empty(), "in openapi.yaml but declared nowhere in src/:\n{}", show(&undeclared));
}

#[test]
fn the_source_scan_reads_both_registration_styles() {
    let mut out = BTreeSet::new();
    scan(
        "#[get(\"/a/{id}\")]\nfn x() {}\ncfg.service(web::resource(\"/b\").route(web::post().to(y)))\n.service(web::resource(\"/c\").guard(guard::Put()).to(z));",
        &mut out,
    );
    let want: BTreeSet<Route> = [("GET", "/a/{id}"), ("POST", "/b"), ("PUT", "/c")]
        .iter()
        .map(|(m, p)| (m.to_string(), p.to_string()))
        .collect();
    assert_eq!(out, want);
}
