//! Parity harness: replays every case of tests/fixtures/golden/manifest.json in-process against
//! the Router (one app per capture env) and compares with the recorded legacy responses using
//! the docs/PLAN.md tolerances. Deliberate differences come from tests/fixtures/divergences.toml.
//! Never stops at the first failure: prints a summary by endpoint and failure category and writes
//! our bodies for failing cases to target/parity-out/.
//!
//! `cargo test --test parity -- --nocapture`. Needs ./fonts or FONT_DIR.

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::http::Method;
use common::{Resp, repo, send};
use image::{DynamicImage, GenericImageView, GrayImage, Pixel, Rgb, RgbImage};
use mermaid_ink::App;
use mink_core::query::{Limits, Query, extract_dimension};
use serde::Deserialize;

/// Golden error texts that come from Mermaid itself: only the status is compared.
const RENDERER_MESSAGES: [&str; 3] = ["Parse error", "Lexical error", "Edge limit exceeded"];
const RASTER_SSIM: f64 = 0.90;
const PDF_PAGE_SSIM: f64 = 0.85;
/// Gray levels away from the background that count as ink (text, strokes, markers). Coverage
/// only needs a third of it, so faint anti-aliased strokes do not flip between the two sides.
const INK_DELTA: i16 = 48;
/// SSIM runs on the union of both images' ink bounding boxes, grown by this margin.
const BBOX_MARGIN: u32 = 8;
/// An ink pixel is covered when the other image has ink within this many pixels.
const INK_RADIUS: usize = 2;
/// Largest tolerated share of uncovered ink pixels, in either direction.
const MAX_INK_MISS: f64 = 0.25;
/// Failure categories that measure renderer fidelity (layout, rasterisation) rather than the
/// HTTP contract. Always reported; they fail the test unless PARITY_STRICT=0.
const FIDELITY: [&str; 12] = [
    "svg-labels",
    "svg-counts",
    "svg-viewbox",
    "svg-colors",
    "svg-dash",
    "img-dims",
    "img-ssim",
    "img-ink",
    "pdf-pages",
    "pdf-mediabox",
    "pdf-ssim",
    "pdf-ink",
];
const FA_URL: &str = "https://example.com/ajax/libs/font-awesome/FA_VERSION/css/all.min.css";

#[derive(Deserialize)]
struct Case {
    id: String,
    env: String,
    method: String,
    path: String,
    query: String,
    headers: BTreeMap<String, String>,
}

#[derive(Deserialize)]
struct Golden {
    status: u16,
    headers: BTreeMap<String, String>,
    body_file: Option<String>,
    error_text: Option<String>,
}

#[derive(Deserialize)]
struct Divergences {
    divergence: Vec<Divergence>,
}

#[derive(Deserialize)]
struct Divergence {
    /// Case ids; `*` matches any run of characters.
    cases: Vec<String>,
    reason: String,
    /// Expected status instead of the golden one; only status and CORS are compared then.
    status: Option<u16>,
    /// Failure categories not counted for these cases (e.g. "img-ssim").
    #[serde(default)]
    skip: Vec<String>,
}

fn glob(pattern: &str, s: &str) -> bool {
    let mut parts = pattern.split('*');
    let first = parts.next().unwrap_or("");
    let Some(mut rest) = s.strip_prefix(first) else {
        return false;
    };
    let parts: Vec<&str> = parts.collect();
    for (i, p) in parts.iter().enumerate() {
        if i == parts.len() - 1 {
            return rest.ends_with(p);
        }
        match rest.find(p) {
            Some(at) => rest = &rest[at + p.len()..],
            None => return false,
        }
    }
    rest.is_empty()
}

fn env_vars(env: &str) -> Vec<(&'static str, &'static str)> {
    // Mirrors tools/golden/run.sh (concurrency left at the default: it does not change output).
    match env {
        "main" => vec![("QUEUE_ADD_TIMEOUT", "60000")],
        "timeout" => vec![("QUEUE_ADD_TIMEOUT", "1")],
        "maxw" => vec![("QUEUE_ADD_TIMEOUT", "60000"), ("MAX_WIDTH", "1005")],
        "fa" => vec![
            ("QUEUE_ADD_TIMEOUT", "60000"),
            ("FONT_AWESOME_CSS_URL", FA_URL),
        ],
        other => panic!("unknown env {other}"),
    }
}

struct Outcome {
    id: String,
    endpoint: String,
    failures: Vec<(String, String)>,
    skipped: Vec<String>,
    body: Option<(Vec<u8>, String)>,
}

#[tokio::test(flavor = "multi_thread")]
async fn golden_parity() {
    let golden_dir = repo().join("tests/fixtures/golden");
    let cases: Vec<Case> =
        serde_json::from_slice(&read(&golden_dir.join("manifest.json"))).unwrap();
    let divergences: Divergences = toml::from_str(
        &String::from_utf8(read(&repo().join("tests/fixtures/divergences.toml"))).unwrap(),
    )
    .expect("divergences.toml");
    for d in &divergences.divergence {
        assert!(
            !d.reason.trim().is_empty(),
            "divergence {:?} needs a reason",
            d.cases
        );
    }
    let divergences = Arc::new(divergences.divergence);
    let only = std::env::var("PARITY_ONLY").ok();

    let mut routers: BTreeMap<String, Router> = BTreeMap::new();
    for env in cases.iter().map(|c| c.env.clone()).collect::<BTreeSet<_>>() {
        let mut app = App::new(common::config(&env_vars(&env))).expect("app");
        if env == "timeout" {
            // QUEUE_ADD_TIMEOUT=1 must miss its deadline however fast the render is.
            app = app.with_render_hook(Arc::new(|| std::thread::sleep(Duration::from_millis(20))));
        }
        routers.insert(env.clone(), mermaid_ink::router(Arc::new(app)));
    }

    let parallel = std::thread::available_parallelism().map_or(4, usize::from);
    let gate = Arc::new(tokio::sync::Semaphore::new(parallel));
    let mut tasks = tokio::task::JoinSet::new();
    for case in cases {
        if only.as_deref().is_some_and(|p| !glob(p, &case.id)) {
            continue;
        }
        let router = routers[&case.env].clone();
        let (gate, divergences, golden_dir) =
            (gate.clone(), divergences.clone(), golden_dir.clone());
        tasks.spawn(async move {
            let _permit = gate.acquire().await.unwrap();
            let resp = request(&router, &case, None).await;
            // Black-background JPEG goldens are checked against our PNG composited on black.
            let png = match golden_is_dark_jpeg(&golden_dir, &case) {
                true => request(&router, &case, Some("type=png")).await,
                false => None,
            };
            tokio::task::spawn_blocking(move || {
                compare(&case, &golden_dir, resp, png, &divergences)
            })
            .await
            .unwrap()
        });
    }
    let mut outcomes = Vec::new();
    while let Some(o) = tasks.join_next().await {
        outcomes.push(o.expect("case task"));
    }
    outcomes.sort_by(|a, b| a.id.cmp(&b.id));
    report(&outcomes);
    // The HTTP contract (status, headers, our own texts, svg post-processing) must match exactly.
    // Renderer fidelity gaps fail too unless listed in divergences.toml; PARITY_STRICT=0 only reports them.
    let strict = !std::env::var("PARITY_STRICT").is_ok_and(|v| v == "0");
    let failed: Vec<String> = outcomes
        .iter()
        .flat_map(|o| o.failures.iter().map(move |(cat, d)| (o, cat, d)))
        .filter(|(_, cat, _)| strict || !FIDELITY.contains(&cat.as_str()))
        .map(|(o, cat, d)| format!("{} [{cat}] {d}", o.id))
        .collect();
    assert!(
        failed.is_empty(),
        "{} parity failures:
{}",
        failed.len(),
        failed.join(
            "
"
        )
    );
}

fn read(p: &Path) -> Vec<u8> {
    std::fs::read(p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

/// `first_param` goes in front of the case's query (repeated params use the first value).
async fn request(router: &Router, case: &Case, first_param: Option<&str>) -> Option<Resp> {
    let query: Vec<&str> = first_param
        .into_iter()
        .chain(Some(case.query.as_str()).filter(|q| !q.is_empty()))
        .collect();
    let uri = if query.is_empty() {
        case.path.clone()
    } else {
        format!("{}?{}", case.path, query.join("&"))
    };
    // http::Uri (and so hyper) rejects request targets over 64 KiB before any routing.
    uri.parse::<axum::http::Uri>().ok()?;
    let headers: Vec<(&str, &str)> = case
        .headers
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let method = Method::from_bytes(case.method.as_bytes()).unwrap();
    Some(send(router, method, &uri, &headers).await)
}

fn golden(golden_dir: &Path, id: &str) -> Golden {
    serde_json::from_slice(&read(&golden_dir.join(format!("cases/{id}.json")))).unwrap()
}

/// Chrome painted transparent JPEG areas white or black at random; ours are always white.
fn dark(p: image::Rgba<u8>) -> bool {
    p[0] < 40 && p[1] < 40 && p[2] < 40
}

fn golden_is_dark_jpeg(golden_dir: &Path, case: &Case) -> bool {
    let g = golden(golden_dir, &case.id);
    let jpeg = g.headers.get("content-type").map(String::as_str) == Some("image/jpeg");
    match (g.status, jpeg, &g.body_file) {
        (200, true, Some(f)) => image::load_from_memory(&read(&golden_dir.join(f)))
            .is_ok_and(|img| dark(img.get_pixel(0, 0))),
        _ => false,
    }
}

fn compare(
    case: &Case,
    golden_dir: &Path,
    resp: Option<Resp>,
    png: Option<Resp>,
    divergences: &[Divergence],
) -> Outcome {
    let golden = golden(golden_dir, &case.id);
    let endpoint = match case.path.split('/').nth(1).map(str::to_ascii_lowercase) {
        Some(e) if ["img", "svg", "pdf"].contains(&e.as_str()) => e,
        _ => "other".into(),
    };
    let matching: Vec<&Divergence> = divergences
        .iter()
        .filter(|d| d.cases.iter().any(|p| glob(p, &case.id)))
        .collect();
    let mut out = Outcome {
        id: case.id.clone(),
        endpoint,
        failures: Vec::new(),
        skipped: Vec::new(),
        body: None,
    };
    let mut fail = |cat: &str, detail: String| out.failures.push((cat.to_owned(), detail));

    // A request target the server never sees: hyper answers 414 with no headers.
    let resp = resp.unwrap_or(Resp {
        status: 414,
        headers: Default::default(),
        body: Default::default(),
    });
    let override_status = matching.iter().find_map(|d| d.status);
    let want_status = override_status.unwrap_or(golden.status);
    if resp.status != want_status {
        fail("status", format!("{} (want {want_status})", resp.status));
    }
    for h in [
        "access-control-allow-origin",
        "vary",
        "access-control-allow-methods",
        "access-control-allow-headers",
    ] {
        let (ours, theirs) = (resp.header(h), golden.headers.get(h).map(String::as_str));
        if ours != theirs {
            fail("cors", format!("{h}: {ours:?} (want {theirs:?})"));
        }
    }
    if override_status.is_none() && resp.status == golden.status {
        if let Some(ct) = golden.headers.get("content-type")
            && resp.header("content-type") != Some(ct.as_str())
        {
            fail(
                "content-type",
                format!("{:?} (want {ct:?})", resp.header("content-type")),
            );
        }
        if golden.status >= 400 {
            let theirs = golden.error_text.as_deref().unwrap_or("");
            let renderer = RENDERER_MESSAGES.iter().any(|m| theirs.starts_with(m));
            if !renderer && resp.text() != theirs {
                fail("error-text", format!("{:?} (want {theirs:?})", resp.text()));
            }
        } else if golden.status == 200 && case.method == "GET" {
            let body = golden.body_file.as_ref().map(|f| read(&golden_dir.join(f)));
            let ct = golden
                .headers
                .get("content-type")
                .map_or("", String::as_str);
            let query = Query::parse(&case.query);
            let limits = if case.env == "maxw" {
                Limits::from_env_values(Some("1005"), None)
            } else {
                Limits::from_env_values(None, None)
            };
            let explicit = extract_dimension(Some(&query), &limits);
            let explicit = (
                explicit.and_then(|d| d.width).is_some(),
                explicit.and_then(|d| d.height).is_some(),
            );
            match (body, ct) {
                (None, _) => fail("golden", "no golden body".into()),
                (Some(g), "image/svg+xml") => compare_svg(&g, &resp.body, &mut fail),
                (Some(g), "application/pdf") => {
                    compare_pdf(&case.id, &g, &resp.body, golden_dir, &mut fail)
                }
                (Some(g), ct) if ct.starts_with("image/") => compare_raster(
                    &g,
                    &resp.body,
                    png.as_ref().map(|p| p.body.as_ref()),
                    explicit,
                    &mut fail,
                ),
                (Some(g), _) => {
                    if g != resp.body.as_ref() {
                        fail("body", "differs from golden".into());
                    }
                }
            }
        }
    }
    let (skipped, failures): (Vec<_>, Vec<_>) = std::mem::take(&mut out.failures)
        .into_iter()
        .partition(|(cat, _)| matching.iter().any(|d| d.skip.iter().any(|s| s == cat)));
    out.failures = failures;
    out.skipped.extend(
        skipped
            .into_iter()
            .map(|(cat, _)| format!("{cat}: listed divergence")),
    );
    if !out.failures.is_empty() {
        // For black-background JPEG goldens, the picture that was compared is our PNG.
        let resp = png.as_ref().unwrap_or(&resp);
        let ext = match resp.header("content-type").unwrap_or("") {
            "image/svg+xml" => "svg",
            "image/png" => "png",
            "image/jpeg" => "jpg",
            "image/webp" => "webp",
            "application/pdf" => "pdf",
            _ => "txt",
        };
        out.body = Some((resp.body.to_vec(), ext.into()));
    }
    out
}

// SVG -----------------------------------------------------------------------------------------

#[derive(Debug, PartialEq)]
struct SvgSummary {
    width: Option<String>,
    height: Option<String>,
    background: Option<String>,
    max_width: Option<String>,
    import_style: Option<String>,
    view_box: Option<Vec<f64>>,
    labels: BTreeSet<String>,
    nodes: usize,
    clusters: usize,
    edges: usize,
    markers: usize,
    /// Distinct fill/stroke colours (attributes, inline styles and <style> rules) as #rrggbb.
    colors: BTreeSet<String>,
    /// (sorted class tokens of an edge path, effectively dashed).
    dashes: BTreeSet<(String, bool)>,
}

fn has_class(n: roxmltree::Node, token: &str) -> bool {
    n.attribute("class")
        .is_some_and(|c| c.split_whitespace().any(|t| t == token))
}

/// `prop: value` pairs of a declaration list; also fine on whole stylesheets once braces are
/// turned into separators (selectors then come out as pairs nobody asks for).
fn declarations(css: &str) -> impl Iterator<Item = (String, &str)> {
    css.split([';', '{', '}'])
        .filter_map(|d| d.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim()))
}

/// CSS colour to `#rrggbb`; None for none/transparent/url()/currentColor. Unknown syntax is kept
/// as-is so it still has to match exactly.
fn normalize_color(v: &str) -> Option<String> {
    let v = v.trim_end_matches("!important").trim().to_ascii_lowercase();
    let hex = |r: f64, g: f64, b: f64| {
        let c = |x: f64| x.round().clamp(0.0, 255.0) as u8;
        format!("#{:02x}{:02x}{:02x}", c(r), c(g), c(b))
    };
    let args = |s: &str| -> Vec<f64> {
        s.split([' ', ',', '/'])
            .filter(|p| !p.is_empty())
            .filter_map(|p| {
                p.trim_end_matches(['%'])
                    .trim_end_matches("deg")
                    .parse()
                    .ok()
            })
            .collect()
    };
    match v.as_str() {
        "" | "none" | "transparent" | "currentcolor" | "inherit" | "initial" | "unset"
        | "revert" => {
            return None;
        }
        "black" => return Some("#000000".into()),
        "white" => return Some("#ffffff".into()),
        "red" => return Some("#ff0000".into()),
        "green" => return Some("#008000".into()),
        "blue" => return Some("#0000ff".into()),
        "gray" | "grey" => return Some("#808080".into()),
        "lightgray" | "lightgrey" => return Some("#d3d3d3".into()),
        "yellow" => return Some("#ffff00".into()),
        "orange" => return Some("#ffa500".into()),
        "purple" => return Some("#800080".into()),
        _ => {}
    }
    if v.starts_with("url(") {
        return None;
    }
    if let Some(h) = v
        .strip_prefix('#')
        .filter(|h| h.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return Some(match h.len() {
            3 | 4 => h.chars().take(3).flat_map(|c| [c, c]).collect::<String>(),
            6 | 8 => h[..6].to_owned(),
            _ => return Some(v),
        })
        .map(|h| format!("#{h}"));
    }
    if let Some(inner) = v
        .strip_prefix("rgba(")
        .or_else(|| v.strip_prefix("rgb("))
        .and_then(|s| s.strip_suffix(')'))
        && let [r, g, b, ..] = args(inner)[..]
    {
        return Some(hex(r, g, b));
    }
    if let Some(inner) = v
        .strip_prefix("hsla(")
        .or_else(|| v.strip_prefix("hsl("))
        .and_then(|s| s.strip_suffix(')'))
        && let [h, s, l, ..] = args(inner)[..]
    {
        let (s, l) = (s / 100.0, l / 100.0);
        let a = s * l.min(1.0 - l);
        let f = |n: f64| {
            let k = (n + h.rem_euclid(360.0) / 30.0) % 12.0;
            255.0 * (l - a * (k - 3.0).min(9.0 - k).clamp(-1.0, 1.0))
        };
        return Some(hex(f(0.0), f(8.0), f(4.0)));
    }
    Some(v)
}

/// Dash arrays with a non-zero dash and a non-zero gap. Mermaid's edge animation start state
/// `0 0 <path length> <gap>` is one dash over the whole path: solid.
fn is_dashed(v: &str) -> bool {
    let mut n: Vec<f64> = v
        .split([' ', ','])
        .filter_map(|p| p.trim().trim_end_matches("px").parse().ok())
        .collect();
    if n.len() == 4 && n[0] == 0.0 && n[1] == 0.0 {
        return false;
    }
    if n.len() % 2 == 1 {
        n.extend(n.clone());
    }
    let any = |odd: usize| n.iter().skip(odd).step_by(2).any(|x| *x > 0.0);
    any(0) && any(1)
}

fn summarize_svg(body: &[u8]) -> Result<SvgSummary, String> {
    let text = std::str::from_utf8(body).map_err(|e| e.to_string())?;
    let doc = roxmltree::Document::parse(text).map_err(|e| e.to_string())?;
    let root = doc.root_element();
    let style: BTreeMap<String, String> = root
        .attribute("style")
        .unwrap_or("")
        .split(';')
        .filter_map(|d| d.split_once(':'))
        .map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))
        .collect();
    let elements = || doc.descendants().filter(|n| n.is_element());
    let tag = |n: &roxmltree::Node, t: &str| n.tag_name().name() == t;
    let sheets: Vec<&str> = elements()
        .filter(|n| tag(n, "style"))
        .filter_map(|n| n.text())
        .collect();

    let mut colors = BTreeSet::new();
    let mut add = |k: &str, v: &str| {
        if k == "fill" || k == "stroke" {
            colors.extend(normalize_color(v));
        }
    };
    for n in elements() {
        for a in ["fill", "stroke"] {
            if let Some(v) = n.attribute(a) {
                add(a, v);
            }
        }
        for (k, v) in declarations(n.attribute("style").unwrap_or("")) {
            add(&k, v);
        }
    }
    let tags: BTreeSet<&str> = elements().map(|n| n.tag_name().name()).collect();
    let classes: BTreeSet<&str> = elements()
        .filter_map(|n| n.attribute("class"))
        .flat_map(str::split_whitespace)
        .collect();
    // Only rules that can match something count: theme CSS carries rules for other diagram types.
    let applies = |selectors: &str| {
        selectors.split(',').any(|sel| {
            let compound = sel.split([' ', '>', '+', '~']).rfind(|c| !c.is_empty());
            let compound = compound
                .unwrap_or("")
                .split([':', '['])
                .next()
                .unwrap_or("");
            let mut parts = compound.split('.');
            let tag = parts.next().unwrap_or("").split('#').next().unwrap_or("");
            (tag.is_empty() || tag == "*" || tags.contains(tag))
                && parts.all(|c| classes.contains(c.split('#').next().unwrap_or(c)))
        })
    };
    // `.class { stroke-dasharray: x }` rules by the selector's final class, in stylesheet order.
    let mut css_dash: Vec<(&str, &str)> = Vec::new();
    for sheet in &sheets {
        for rule in sheet.split('}') {
            // `@keyframes a{to{...}`: the selector is what precedes the innermost `{`.
            let Some((selector, body)) = rule.rsplit_once('{') else {
                continue;
            };
            let selector = selector.rsplit('{').next().unwrap_or(selector);
            if applies(selector) {
                for (k, v) in declarations(body) {
                    add(&k, v);
                }
            }
            let last = selector.split_whitespace().last().unwrap_or("");
            let Some(class) = last.strip_prefix('.').filter(|c| !c.contains(['.', ':'])) else {
                continue;
            };
            if let Some((_, v)) = declarations(body).find(|(k, _)| k == "stroke-dasharray") {
                css_dash.push((class, v));
            }
        }
    }
    let edge_paths = || {
        elements().filter(|n| tag(n, "path") && n.ancestors().any(|a| has_class(a, "edgePaths")))
    };
    let dashes = edge_paths()
        .map(|n| {
            let classes = n.attribute("class").unwrap_or("");
            let inline = declarations(n.attribute("style").unwrap_or(""))
                .filter(|(k, _)| k == "stroke-dasharray")
                .map(|(_, v)| v)
                .last();
            // Equal specificity: the rule defined last wins.
            let css = css_dash
                .iter()
                .rfind(|(c, _)| classes.split_whitespace().any(|t| t == *c))
                .map(|(_, v)| *v);
            let dash = inline.or(css).or(n.attribute("stroke-dasharray"));
            let mut key: Vec<&str> = classes.split_whitespace().collect();
            key.sort_unstable();
            (key.join(" "), dash.is_some_and(is_dashed))
        })
        .collect();

    Ok(SvgSummary {
        width: root.attribute("width").map(str::to_owned),
        height: root.attribute("height").map(str::to_owned),
        background: style.get("background-color").cloned(),
        max_width: style.get("max-width").cloned(),
        import_style: root
            .children()
            .filter(|n| tag(n, "style"))
            .filter_map(|n| n.text())
            .find(|t| t.contains("@import"))
            .map(str::to_owned),
        view_box: root
            .attribute("viewBox")
            .map(|v| v.split([' ', ',']).filter_map(|n| n.parse().ok()).collect()),
        labels: doc
            .descendants()
            .filter(|n| n.is_text())
            .filter(|n| {
                n.ancestors()
                    .any(|a| matches!(a.tag_name().name(), "text" | "tspan" | "foreignObject"))
            })
            .map(|n| {
                n.text()
                    .unwrap_or("")
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .filter(|t| !t.is_empty())
            .collect(),
        nodes: elements()
            .filter(|n| tag(n, "g") && has_class(*n, "node"))
            .count(),
        clusters: elements()
            .filter(|n| tag(n, "g") && has_class(*n, "cluster"))
            .count(),
        edges: edge_paths().count(),
        markers: elements().filter(|n| tag(n, "marker")).count(),
        colors,
        dashes,
    })
}

fn close(ours: f64, theirs: f64, rel: f64, abs: f64) -> bool {
    (ours - theirs).abs() <= (theirs.abs() * rel).max(abs)
}

fn px(v: &str) -> Option<f64> {
    v.strip_suffix("px")?.parse().ok()
}

/// Colours in `a` with no colour in `b` within 3 per channel (rounding of computed hsl values).
fn unmatched_colors<'a>(a: &'a BTreeSet<String>, b: &BTreeSet<String>) -> Vec<&'a String> {
    let rgb = |c: &str| -> Option<[i32; 3]> {
        let h = c.strip_prefix('#').filter(|h| h.len() == 6)?;
        let ch = |i: usize| i32::from_str_radix(&h[i..i + 2], 16).ok();
        Some([ch(0)?, ch(2)?, ch(4)?])
    };
    let near = |x: &String, y: &String| match (rgb(x), rgb(y)) {
        (Some(p), Some(q)) => p.iter().zip(q).all(|(a, b)| (a - b).abs() <= 3),
        _ => x == y,
    };
    a.iter().filter(|x| !b.iter().any(|y| near(x, y))).collect()
}

fn compare_svg(golden: &[u8], ours: &[u8], fail: &mut impl FnMut(&str, String)) {
    let (g, o) = match (summarize_svg(golden), summarize_svg(ours)) {
        (Ok(g), Ok(o)) => (g, o),
        (Err(e), _) => return fail("golden", format!("golden svg: {e}")),
        (_, Err(e)) => return fail("svg-parse", e),
    };
    if (&o.width, &o.height, &o.background, &o.import_style)
        != (&g.width, &g.height, &g.background, &g.import_style)
    {
        fail(
            "svg-root",
            format!(
                "width/height/bg/import {:?} {:?} {:?} {:?} (want {:?} {:?} {:?} {:?})",
                o.width,
                o.height,
                o.background,
                o.import_style,
                g.width,
                g.height,
                g.background,
                g.import_style
            ),
        );
    }
    let max_width_ok = match (o.max_width.as_deref(), g.max_width.as_deref()) {
        (Some(a), Some(b)) => match (px(a), px(b)) {
            (Some(a), Some(b)) => close(a, b, 0.05, 1.0),
            _ => a == b,
        },
        (a, b) => a == b,
    };
    if !max_width_ok {
        fail(
            "svg-viewbox",
            format!("max-width {:?} (want {:?})", o.max_width, g.max_width),
        );
    }
    match (&o.view_box, &g.view_box) {
        (Some(a), Some(b)) if a.len() == 4 && b.len() == 4 => {
            let ok = close(a[2], b[2], 0.05, 1.0)
                && close(a[3], b[3], 0.05, 1.0)
                && (a[0] - b[0]).abs() <= b[2] * 0.05 + 1.0
                && (a[1] - b[1]).abs() <= b[3] * 0.05 + 1.0;
            if !ok {
                fail("svg-viewbox", format!("{a:?} (want {b:?})"));
            }
        }
        (a, b) if a != b => fail("svg-viewbox", format!("{a:?} (want {b:?})")),
        _ => {}
    }
    if o.labels != g.labels {
        let missing: Vec<_> = g.labels.difference(&o.labels).collect();
        let extra: Vec<_> = o.labels.difference(&g.labels).collect();
        fail("svg-labels", format!("missing {missing:?} extra {extra:?}"));
    }
    let counts = |s: &SvgSummary| (s.nodes, s.clusters, s.edges, s.markers);
    if counts(&o) != counts(&g) {
        fail(
            "svg-counts",
            format!(
                "nodes/clusters/edges/markers {:?} (want {:?})",
                counts(&o),
                counts(&g)
            ),
        );
    }
    let (missing, extra) = (
        unmatched_colors(&g.colors, &o.colors),
        unmatched_colors(&o.colors, &g.colors),
    );
    if !missing.is_empty() || !extra.is_empty() {
        fail("svg-colors", format!("missing {missing:?} extra {extra:?}"));
    }
    if o.dashes != g.dashes {
        let missing: Vec<_> = g.dashes.difference(&o.dashes).collect();
        let extra: Vec<_> = o.dashes.difference(&g.dashes).collect();
        fail(
            "svg-dash",
            format!("(edge class, dashed) missing {missing:?} extra {extra:?}"),
        );
    }
}

// Raster --------------------------------------------------------------------------------------

/// Composites on `bg` (the golden's top-left colour) and converts to gray.
fn flatten(img: &DynamicImage, bg: Rgb<u8>) -> GrayImage {
    let rgba = img.to_rgba8();
    let mut out = RgbImage::new(rgba.width(), rgba.height());
    for (x, y, p) in rgba.enumerate_pixels() {
        let a = p[3] as f32 / 255.0;
        let mix = |c: usize| (p[c] as f32 * a + bg[c] as f32 * (1.0 - a)).round() as u8;
        out.put_pixel(x, y, Rgb([mix(0), mix(1), mix(2)]));
    }
    DynamicImage::ImageRgb8(out).to_luma8()
}

/// Bounding box (x0, y0, x1, y1), exclusive end, of the set pixels.
fn ink_bbox(ink: &[bool], w: usize) -> Option<(usize, usize, usize, usize)> {
    ink.iter()
        .enumerate()
        .filter(|(_, i)| **i)
        .map(|(n, _)| (n % w, n / w))
        .fold(None, |b, (x, y)| {
            let (x0, y0, x1, y1) = b.unwrap_or((x, y, x + 1, y + 1));
            Some((x0.min(x), y0.min(y), x1.max(x + 1), y1.max(y + 1)))
        })
}

/// Share of `a`'s ink pixels with no ink in `b` within INK_RADIUS (0 when `a` has none).
fn uncovered(a: &[bool], b: &[bool], w: usize, h: usize) -> f64 {
    let r = INK_RADIUS;
    let mut row = vec![false; w * h];
    for y in 0..h {
        for x in (0..w).filter(|x| b[y * w + x]) {
            row[y * w + x.saturating_sub(r)..=y * w + (x + r).min(w - 1)].fill(true);
        }
    }
    let mut near = vec![false; w * h];
    for (n, _) in row.iter().enumerate().filter(|(_, v)| **v) {
        let (x, y) = (n % w, n / w);
        for yy in y.saturating_sub(r)..=(y + r).min(h - 1) {
            near[yy * w + x] = true;
        }
    }
    let total = a.iter().filter(|v| **v).count();
    let missed = a.iter().zip(&near).filter(|(a, n)| **a && !**n).count();
    if total == 0 {
        0.0
    } else {
        missed as f64 / total as f64
    }
}

/// (SSIM, worst share of uncovered ink). `ours` is scaled to the golden's size and both are
/// composited on the golden's corner colour. With `crop`, SSIM runs on the union of both ink
/// bounding boxes, so blank margins neither inflate the score nor hide a missing half.
fn raster_scores(
    golden: &DynamicImage,
    ours: &DynamicImage,
    crop: bool,
) -> Result<(f64, f64), String> {
    let corner = golden.get_pixel(0, 0);
    let bg = if corner[3] < 128 {
        Rgb([255, 255, 255])
    } else {
        Rgb([corner[0], corner[1], corner[2]])
    };
    let ours = if ours.dimensions() == golden.dimensions() {
        ours.clone()
    } else {
        ours.resize_exact(
            golden.width(),
            golden.height(),
            image::imageops::FilterType::Triangle,
        )
    };
    let (g, o) = (flatten(golden, bg), flatten(&ours, bg));
    let (w, h) = (g.width() as usize, g.height() as usize);
    let paper = i16::from(bg.to_luma()[0]);
    let ink = |img: &GrayImage, delta: i16| -> Vec<bool> {
        img.pixels()
            .map(|p| (i16::from(p[0]) - paper).abs() > delta)
            .collect()
    };
    let (gi, oi) = (ink(&g, INK_DELTA), ink(&o, INK_DELTA));
    let (gw, ow) = (ink(&g, INK_DELTA / 3), ink(&o, INK_DELTA / 3));
    let miss = uncovered(&gi, &ow, w, h).max(uncovered(&oi, &gw, w, h));

    let (x0, y0, x1, y1) = match (ink_bbox(&gi, w), ink_bbox(&oi, w)) {
        _ if !crop => (0, 0, w, h),
        (Some(a), Some(b)) => (a.0.min(b.0), a.1.min(b.1), a.2.max(b.2), a.3.max(b.3)),
        (Some(a), None) | (None, Some(a)) => a,
        (None, None) => (0, 0, w, h),
    };
    let m = BBOX_MARGIN as usize;
    // SSIM windows need some room: at least 16 px per side where the image allows.
    let grow = |lo: usize, hi: usize, max: usize| {
        let (mut lo, mut hi) = (lo.saturating_sub(m), (hi + m).min(max));
        while hi - lo < 16.min(max) {
            lo = lo.saturating_sub(1);
            hi = (hi + 1).min(max);
        }
        (lo as u32, (hi - lo) as u32)
    };
    let ((x, cw), (y, ch)) = (grow(x0, x1, w), grow(y0, y1, h));
    let crop = |img: &GrayImage| image::imageops::crop_imm(img, x, y, cw, ch).to_image();
    let ssim = image_compare::gray_similarity_structure(
        &image_compare::Algorithm::MSSIMSimple,
        &crop(&g),
        &crop(&o),
    )
    .map_err(|e| e.to_string())?
    .score;
    Ok((ssim, miss))
}

/// Shared SSIM + ink checks; `kind` is "img" (cropped SSIM) or "pdf" (whole-page SSIM: a
/// different rasterizer than the golden's pdftoppm, so pixel structure differs on small text).
fn check_picture(
    kind: &str,
    golden: &DynamicImage,
    ours: &DynamicImage,
    min_ssim: f64,
    fail: &mut impl FnMut(&str, String),
) {
    match raster_scores(golden, ours, kind == "img") {
        Ok((ssim, miss)) => {
            if ssim < min_ssim {
                fail(&format!("{kind}-ssim"), format!("{ssim:.3} < {min_ssim}"));
            }
            if miss > MAX_INK_MISS {
                fail(
                    &format!("{kind}-ink"),
                    format!(
                        "{:.0}% of ink unmatched > {:.0}%",
                        miss * 100.0,
                        MAX_INK_MISS * 100.0
                    ),
                );
            }
        }
        Err(e) => fail(&format!("{kind}-ssim"), e),
    }
}

/// `png`: our type=png render of the same case, used when the golden JPEG has a black background.
fn compare_raster(
    golden: &[u8],
    ours: &[u8],
    png: Option<&[u8]>,
    explicit: (bool, bool),
    fail: &mut impl FnMut(&str, String),
) {
    let (Ok(gf), Ok(of)) = (image::guess_format(golden), image::guess_format(ours)) else {
        return fail("img-format", "undecodable".into());
    };
    if gf != of {
        return fail("img-format", format!("{of:?} (want {gf:?})"));
    }
    let (g, o) = match (
        image::load_from_memory(golden),
        image::load_from_memory(ours),
    ) {
        (Ok(g), Ok(o)) => (g, o),
        (_, Err(e)) => return fail("img-format", e.to_string()),
        (Err(e), _) => return fail("golden", e.to_string()),
    };
    let dim_ok = |ours: u32, theirs: u32, exact: bool| {
        if exact {
            ours == theirs
        } else {
            close(ours as f64, theirs as f64, 0.04, 6.0)
        }
    };
    if !dim_ok(o.width(), g.width(), explicit.0) || !dim_ok(o.height(), g.height(), explicit.1) {
        fail(
            "img-dims",
            format!("{:?} (want {:?})", o.dimensions(), g.dimensions()),
        );
    }
    // Our JPEGs are always white underneath: compare our transparent PNG on the golden's black.
    let o = if gf == image::ImageFormat::Jpeg && dark(g.get_pixel(0, 0)) && !dark(o.get_pixel(0, 0))
    {
        match png.map(image::load_from_memory) {
            Some(Ok(p)) => p,
            Some(Err(e)) => return fail("img-format", format!("png re-render: {e}")),
            None => return fail("img-jpeg-bg", "black golden JPEG, no png re-render".into()),
        }
    } else {
        o
    };
    check_picture("img", &g, &o, RASTER_SSIM, fail);
}

// PDF -----------------------------------------------------------------------------------------

fn media_boxes(pdf: &[u8]) -> Result<Vec<Vec<f32>>, String> {
    let doc = lopdf::Document::load_mem(pdf).map_err(|e| e.to_string())?;
    doc.get_pages()
        .values()
        .map(|&id| {
            let page = doc.get_dictionary(id).map_err(|e| e.to_string())?;
            let b = page
                .get(b"MediaBox")
                .and_then(|b| b.as_array())
                .map_err(|e| e.to_string())?;
            b.iter()
                .map(|n| n.as_float().map_err(|e| e.to_string()))
                .collect()
        })
        .collect()
}

/// Page `index` (0-based) at 4x 72 dpi on white; raster_scores scales it down to the golden's
/// `pdftoppm -r 72` size, which matches poppler's anti-aliasing better than rendering at 1x.
fn rasterize_pdf_page(pdf: &[u8], index: usize) -> Result<DynamicImage, String> {
    use hayro::hayro_interpret::InterpreterSettings;
    use hayro::vello_cpu::color::palette::css::WHITE;
    use hayro::{PixmapSettings, RenderCache, RenderSettings};
    let doc = hayro::hayro_syntax::Pdf::new(pdf.to_vec()).map_err(|e| format!("{e:?}"))?;
    let page = doc.pages().get(index).ok_or("no such page")?;
    let pixmap = hayro::render(
        page,
        &RenderCache::new(),
        &InterpreterSettings::default(),
        &RenderSettings::default(),
        &PixmapSettings {
            bg_color: WHITE,
            x_scale: 4.0,
            y_scale: 4.0,
        },
    );
    let png = pixmap.into_png().map_err(|e| e.to_string())?;
    image::load_from_memory(&png).map_err(|e| e.to_string())
}

fn compare_pdf(
    id: &str,
    golden: &[u8],
    ours: &[u8],
    golden_dir: &Path,
    fail: &mut impl FnMut(&str, String),
) {
    let (g, o) = match (media_boxes(golden), media_boxes(ours)) {
        (Ok(g), Ok(o)) => (g, o),
        (Err(e), _) => return fail("golden", e),
        (_, Err(e)) => return fail("pdf-parse", e),
    };
    if g.len() != o.len() {
        fail("pdf-pages", format!("{} (want {})", o.len(), g.len()));
    }
    let boxes_ok = g
        .iter()
        .zip(&o)
        .all(|(g, o)| g.len() == o.len() && g.iter().zip(o).all(|(a, b)| (a - b).abs() <= 1.0));
    if !boxes_ok {
        fail(
            "pdf-mediabox",
            format!("{:?} (want {:?})", o.first(), g.first()),
        );
    }
    let page1 = golden_dir.join(format!("pdfpng/{id}-p1.png"));
    if page1.exists() {
        match (
            image::load_from_memory(&read(&page1)),
            rasterize_pdf_page(ours, 0),
        ) {
            (Ok(g), Ok(o)) => check_picture("pdf", &g, &o, PDF_PAGE_SSIM, fail),
            (Err(e), _) => fail("golden", e.to_string()),
            (_, Err(e)) => fail("pdf-ssim", format!("rasterize: {e}")),
        }
    }
    // Only page 1 has a pdftoppm picture. Page 2 compares the golden PDF rasterized by the same
    // rasterizer (text extraction cannot work: our pages carry the whole diagram, clipped).
    if g.len() >= 2 && o.len() >= 2 {
        match (rasterize_pdf_page(golden, 1), rasterize_pdf_page(ours, 1)) {
            (Ok(g), Ok(o)) => check_picture("pdf", &g, &o, PDF_PAGE_SSIM, &mut |c, d| {
                fail(c, format!("page 2: {d}"))
            }),
            (Err(e), _) => fail("golden", e),
            (_, Err(e)) => fail("pdf-ssim", format!("page 2: rasterize: {e}")),
        }
    }
}

// Report --------------------------------------------------------------------------------------

fn report(outcomes: &[Outcome]) {
    let out_dir: PathBuf = repo().join("target/parity-out");
    let _ = std::fs::remove_dir_all(&out_dir);
    std::fs::create_dir_all(&out_dir).unwrap();

    let mut by_endpoint: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    let mut by_category: BTreeMap<(String, String), Vec<&str>> = BTreeMap::new();
    let mut skipped: BTreeMap<&str, usize> = BTreeMap::new();
    let mut details = String::new();
    for o in outcomes {
        let e = by_endpoint.entry(&o.endpoint).or_default();
        if o.failures.is_empty() {
            e.0 += 1;
        } else {
            e.1 += 1;
            let mut cats = BTreeSet::new();
            for (cat, detail) in &o.failures {
                cats.insert(cat.clone());
                let _ = writeln!(details, "{} [{cat}] {detail}", o.id);
            }
            for cat in cats {
                by_category
                    .entry((o.endpoint.clone(), cat))
                    .or_default()
                    .push(&o.id);
            }
            if let Some((body, ext)) = &o.body {
                let _ = std::fs::write(out_dir.join(format!("{}.{ext}", o.id)), body);
            }
        }
        for s in &o.skipped {
            *skipped.entry(s).or_default() += 1;
        }
    }
    let _ = std::fs::write(out_dir.join("failures.txt"), &details);

    println!("\n=== parity: {} cases ===", outcomes.len());
    println!(
        "{:<8} {:>6} {:>6} {:>6}",
        "endpoint", "total", "pass", "fail"
    );
    for (e, (pass, fail)) in &by_endpoint {
        println!("{e:<8} {:>6} {pass:>6} {fail:>6}", pass + fail);
    }
    println!("\nfailures by endpoint/category (cases, examples):");
    for ((e, cat), ids) in &by_category {
        let examples: Vec<_> = ids.iter().take(6).collect();
        println!("  {e:<6} {cat:<14} {:>4}  {examples:?}", ids.len());
    }
    for (s, n) in &skipped {
        println!("skipped sub-check on {n} cases: {s}");
    }
    println!("details: {}", out_dir.join("failures.txt").display());
}

#[test]
fn glob_matches() {
    assert!(glob("K01__svg__*", "K01__svg__noparams"));
    assert!(glob("*__svg__theme-a.theme-b", "T08__svg__theme-a.theme-b"));
    assert!(glob("F1*.base64__svg__*", "F11.base64__svg__noparams"));
    assert!(glob("D01__serde-unknown", "D01__serde-unknown"));
    assert!(!glob("K01__svg__*", "K01__img__noparams"));
    assert!(!glob("D01", "D01__serde-unknown"));
    assert!(!glob("*a*b", "xbxa"));
}

/// The comparisons must reject deliberately wrong outputs and accept the golden itself.
#[test]
fn harness_rejects_wrong_outputs() {
    let body = |n: &str| read(&repo().join("tests/fixtures/golden/bodies").join(n));
    fn failures(run: impl FnOnce(&mut dyn FnMut(&str, String))) -> Vec<String> {
        let mut v = Vec::new();
        run(&mut |c: &str, d: String| v.push(format!("{c}: {d}")));
        v
    }
    let png = |img: image::RgbaImage| {
        let mut out = std::io::Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(img)
            .write_to(&mut out, image::ImageFormat::Png)
            .unwrap();
        out.into_inner()
    };

    // Raster.
    let other = image::load_from_memory(&body("K01__img__type-png~7921d7.png")).unwrap();
    for name in [
        "T13__img__type-png~e94e3a.png",
        "T08__img__type-png~fe4693.png",
        "T09__img__type-png~bf9c06.png",
    ] {
        let g = body(name);
        let gi = image::load_from_memory(&g).unwrap();
        let (w, h) = gi.dimensions();
        let raster =
            |o: &[u8]| failures(|mut f| compare_raster(&g, o, None, (false, false), &mut f));
        assert_eq!(raster(&g), Vec::<String>::new(), "{name}: golden vs itself");
        let mut shifted = image::RgbaImage::new(w, h);
        image::imageops::overlay(&mut shifted, &gi.to_rgba8(), 12, 12);
        let mut half = gi.to_rgba8();
        for (_, y, p) in half.enumerate_pixels_mut() {
            if y > h / 2 {
                *p = image::Rgba([0, 0, 0, 0]);
            }
        }
        let wrong = [
            (
                "white",
                png(image::RgbaImage::from_pixel(w, h, image::Rgba([255; 4]))),
            ),
            ("transparent", png(image::RgbaImage::new(w, h))),
            ("shifted 12px", png(shifted)),
            ("bottom half erased", png(half)),
            (
                "other diagram",
                png(other
                    .resize_exact(w, h, image::imageops::FilterType::Triangle)
                    .to_rgba8()),
            ),
        ];
        for (what, o) in wrong {
            let f = raster(&o);
            println!("{name} {what}: {f:?}");
            assert!(!f.is_empty(), "{name}: {what} passed");
        }
    }

    // SVG.
    let svg = |g: &[u8], o: &str| failures(|mut f| compare_svg(g, o.as_bytes(), &mut f));
    let t01 = body("T01__svg__noparams.svg");
    let s = String::from_utf8(t01.clone()).unwrap();
    assert_eq!(svg(&t01, &s), Vec::<String>::new(), "T01 svg vs itself");
    let t02 = String::from_utf8(body("T02__svg__noparams.svg")).unwrap();
    for (what, o) in [
        ("label renamed", s.replacen(">Great<", ">Grate<", 1)),
        ("fill changed", s.replace("ECECFF", "123456")),
        ("other diagram", t02),
    ] {
        let f = svg(&t01, &o);
        println!("T01 svg {what}: {f:?}");
        assert!(!f.is_empty(), "svg: {what} passed");
    }
    let t09 = body("T09__svg__noparams.svg");
    let s = String::from_utf8(t09.clone()).unwrap();
    assert_eq!(svg(&t09, &s), Vec::<String>::new(), "T09 svg vs itself");
    // The note edge drawn solid (mermaid's `0 0 <length> 0` animation dasharray).
    let solid = s.replacen(
        "stroke-dasharray: 0 0 2 2",
        "stroke-dasharray: 0 0 450 0; x: 2 2",
        1,
    );
    let f = svg(&t09, &solid);
    assert!(f.iter().any(|f| f.starts_with("svg-dash")), "{f:?}");

    // PDF: the golden rasterized here must match pdftoppm's page; another diagram must not.
    let dir = repo().join("tests/fixtures/golden");
    let pdf = |id: &str, o: &[u8]| {
        let g = body(&format!("{id}.pdf"));
        failures(|mut f| compare_pdf(id, &g, o, &dir, &mut f))
    };
    for id in ["T02__pdf__noparams", "T08__pdf__landscape-false"] {
        assert_eq!(
            pdf(id, &body(&format!("{id}.pdf"))),
            Vec::<String>::new(),
            "{id} vs itself"
        );
    }
    for other in ["T01__pdf__noparams.pdf", "T13__pdf__noparams.pdf"] {
        let f = pdf("T02__pdf__noparams", &body(other));
        println!("T02 pdf vs {other}: {f:?}");
        assert!(
            f.iter().any(|f| f.starts_with("pdf-")),
            "T02 pdf vs {other} passed"
        );
    }
}
