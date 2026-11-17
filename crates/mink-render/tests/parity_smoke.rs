//! Self-parity smoke check against tests/fixtures/golden for the T-series (pako form): raster
//! dimensions, SVG label sets and PDF page boxes. It reports how many cases match; the full
//! parity harness (SSIM, headers, statuses) lives at the workspace level.
//! Run with `--nocapture` to see the report; mismatching outputs go to target/parity-smoke/.

mod common;

use std::collections::BTreeSet;

use common::{corpus, job, render, repo};
use mink_render::OutputFormat;
use serde_json::Value;

struct Case {
    id: String,
    corpus: String,
    endpoint: String,
    query: Vec<(String, String)>,
}

fn cases() -> Vec<Case> {
    let dir = repo().join("tests/fixtures/golden");
    let manifest: Vec<Value> =
        serde_json::from_str(&std::fs::read_to_string(dir.join("manifest.json")).unwrap()).unwrap();
    manifest
        .iter()
        .filter_map(|c| {
            let id = c["id"].as_str()?.to_owned();
            let corpus = c["corpus"].as_str()?.to_owned();
            let golden: Value = serde_json::from_str(
                &std::fs::read_to_string(dir.join(format!("cases/{id}.json"))).ok()?,
            )
            .ok()?;
            let pako_form = !corpus.contains('.'); // T01.base64 / T01.plain are other encodings
            if !corpus.starts_with('T')
                || !pako_form
                || c["env"] != "main"
                || golden["status"] != 200
            {
                return None;
            }
            let query: Vec<(String, String)> = c["query"]
                .as_str()?
                .split('&')
                .filter(|kv| !kv.is_empty())
                .map(|kv| {
                    let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
                    (k.to_owned(), v.replace("%20", " ").replace("%23", "#"))
                })
                .collect();
            let mut keys: Vec<_> = query.iter().map(|(k, _)| k).collect();
            keys.dedup();
            let endpoint = c["path"].as_str()?.split('/').nth(1)?.to_owned();
            (keys.len() == query.len()).then_some(Case {
                id,
                corpus,
                endpoint,
                query,
            })
        })
        .collect()
}

fn param<'a>(case: &'a Case, key: &str) -> Option<&'a str> {
    case.query
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
        .filter(|v| !v.is_empty())
}

fn js_parse_int(s: &str) -> Option<f64> {
    let digits: String = s.trim().chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// utils.js extractBgColor (unanchored-alternation regex included).
fn bg_color(s: &str) -> Option<String> {
    let s = s.trim();
    if s.len() > 1 && s.starts_with('!') && s.as_bytes()[1].is_ascii_alphabetic() {
        return Some(s[1..].to_owned());
    }
    let tail = s.bytes().rev().take_while(u8::is_ascii_hexdigit).count();
    (tail >= 3).then(|| format!("#{s}"))
}

fn build_job(case: &Case, code: &str, config: &Value) -> mink_render::RenderJob {
    let format = match (
        case.endpoint.as_str(),
        param(case, "type").map(str::to_ascii_lowercase).as_deref(),
    ) {
        ("svg", _) => OutputFormat::Svg,
        ("pdf", _) => OutputFormat::Pdf,
        (_, Some("png")) => OutputFormat::Png,
        (_, Some("webp")) => OutputFormat::Webp,
        _ => OutputFormat::Jpeg,
    };
    let mut config = config.clone();
    if let Some(theme) = param(case, "theme").map(str::to_ascii_lowercase)
        && ["default", "neutral", "dark", "forest"].contains(&theme.as_str())
    {
        config["theme"] = theme.into();
    }
    let mut j = job(code, config, format);
    let scale = param(case, "scale")
        .and_then(|s| s.parse::<f64>().ok())
        .map_or(1.0, |s| ((s + f64::EPSILON) * 100.0).round() / 100.0);
    j.width = param(case, "width")
        .and_then(js_parse_int)
        .map(|w| w * scale);
    j.height = param(case, "height")
        .and_then(js_parse_int)
        .map(|h| h * scale);
    j.bg_color = param(case, "bgColor").and_then(bg_color);
    j.paper = param(case, "paper").map_or("a4".into(), str::to_ascii_lowercase);
    j.landscape = case.query.iter().any(|(k, _)| k == "landscape");
    j.fit = case.query.iter().any(|(k, _)| k == "fit");
    j
}

fn labels(svg: &str) -> BTreeSet<String> {
    let doc = roxmltree::Document::parse(svg).unwrap();
    doc.descendants()
        .filter(|n| n.is_text())
        .filter(|n| {
            n.ancestors()
                .any(|a| matches!(a.tag_name().name(), "text" | "foreignObject"))
        })
        .map(|n| {
            n.text()
                .unwrap_or("")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        })
        .filter(|t| !t.is_empty())
        .collect()
}

fn pdf_boxes(pdf: &[u8]) -> Vec<Vec<f32>> {
    let doc = lopdf::Document::load_mem(pdf).unwrap();
    doc.get_pages()
        .values()
        .map(|&id| {
            let page = doc.get_dictionary(id).unwrap();
            page.get(b"MediaBox")
                .unwrap()
                .as_array()
                .unwrap()
                .iter()
                .map(|n| n.as_float().unwrap())
                .collect()
        })
        .collect()
}

fn close(ours: u32, golden: u32, exact: bool) -> bool {
    let tolerance = if exact {
        0.0
    } else {
        (golden as f64 * 0.04).max(6.0)
    };
    (ours as f64 - golden as f64).abs() <= tolerance
}

#[test]
fn t_series_self_parity() {
    let corpus = corpus();
    let golden_dir = repo().join("tests/fixtures/golden");
    let out_dir = repo().join("target/parity-smoke");
    std::fs::create_dir_all(&out_dir).unwrap();
    let mut tally: std::collections::BTreeMap<&str, (usize, usize)> = Default::default();
    let mut report = Vec::new();
    for case in cases() {
        let (_, code, config) = corpus.iter().find(|(id, ..)| *id == case.corpus).unwrap();
        let job = build_job(&case, code, config);
        let golden = std::fs::read(
            golden_dir.join(format!("bodies/{}", golden_body(&golden_dir, &case.id))),
        )
        .unwrap();
        let ours = match render(&job) {
            Ok(body) => body,
            Err(e) => {
                report.push(format!("{}: render error {e:?}", case.id));
                tally.entry(endpoint_key(&case)).or_default().1 += 1;
                continue;
            }
        };
        let (ok, detail) = match case.endpoint.as_str() {
            "img" => {
                let (g, o) = (
                    image::load_from_memory(&golden).unwrap(),
                    image::load_from_memory(&ours).unwrap(),
                );
                let exact_w = job.width.is_some();
                let exact_h = job.height.is_some();
                let ok =
                    close(o.width(), g.width(), exact_w) && close(o.height(), g.height(), exact_h);
                (
                    ok,
                    format!(
                        "ours {}x{} golden {}x{}",
                        o.width(),
                        o.height(),
                        g.width(),
                        g.height()
                    ),
                )
            }
            "svg" => {
                let (g, o) = (
                    labels(&String::from_utf8_lossy(&golden)),
                    labels(&String::from_utf8_lossy(&ours)),
                );
                let missing: Vec<_> = g.difference(&o).take(3).collect();
                let extra: Vec<_> = o.difference(&g).take(3).collect();
                (g == o, format!("missing {missing:?} extra {extra:?}"))
            }
            _ => {
                let (g, o) = (pdf_boxes(&golden), pdf_boxes(&ours));
                let ok = g.len() == o.len()
                    && g.iter().zip(&o).all(|(a, b)| {
                        a.iter()
                            .zip(b)
                            .all(|(x, y)| (x - y).abs() <= 1.0 + 0.04 * x * job.fit as u8 as f32)
                    });
                (
                    ok,
                    format!(
                        "ours {} pages {:?} golden {} pages {:?}",
                        o.len(),
                        o.first(),
                        g.len(),
                        g.first()
                    ),
                )
            }
        };
        let entry = tally.entry(endpoint_key(&case)).or_default();
        if ok {
            entry.0 += 1;
        } else {
            entry.1 += 1;
            report.push(format!("{}: {detail}", case.id));
            let ext = match job.format {
                OutputFormat::Svg => "svg",
                OutputFormat::Pdf => "pdf",
                OutputFormat::Png => "png",
                OutputFormat::Jpeg => "jpg",
                OutputFormat::Webp => "webp",
            };
            std::fs::write(out_dir.join(format!("{}.{ext}", case.id)), &ours).unwrap();
        }
    }
    println!("mismatches:\n  {}", report.join("\n  "));
    for (key, (pass, fail)) in &tally {
        println!("{key}: {pass} match, {fail} differ");
    }
    let (pass, fail) = tally.values().fold((0, 0), |a, b| (a.0 + b.0, a.1 + b.1));
    println!("total: {pass}/{} within tolerance", pass + fail);
    // Regression floor: the 2026-11-17 run matched 320 of 334 cases (misses: T28 zenuml, T08 ledger page break).
    assert!(
        pass * 100 >= (pass + fail) * 85,
        "parity dropped to {pass}/{}",
        pass + fail
    );
}

fn endpoint_key(case: &Case) -> &'static str {
    match case.endpoint.as_str() {
        "img" => "img dimensions",
        "svg" => "svg label sets",
        _ => "pdf page boxes",
    }
}

fn golden_body(dir: &std::path::Path, id: &str) -> String {
    let case: Value = serde_json::from_str(
        &std::fs::read_to_string(dir.join(format!("cases/{id}.json"))).unwrap(),
    )
    .unwrap();
    case["body_file"]
        .as_str()
        .unwrap()
        .trim_start_matches("bodies/")
        .to_owned()
}
