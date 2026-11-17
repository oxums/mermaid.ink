#![allow(dead_code)] // each test binary uses a different subset

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use mink_render::{Engine, OutputFormat, RenderError, RenderJob, RenderSettings};

pub fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// One engine per test binary. Fonts come from FONT_DIR or ./fonts (`bash tools/fonts/fetch.sh`).
pub fn engine() -> &'static Engine {
    static ENGINE: OnceLock<Engine> = OnceLock::new();
    ENGINE.get_or_init(|| {
        let font_dir =
            std::env::var_os("FONT_DIR").map_or_else(|| repo().join("fonts"), PathBuf::from);
        Engine::new(RenderSettings {
            font_dir,
            max_layout_work_units: None,
            max_width: 10000,
            max_height: 10000,
            max_raster_pixels: 100_000_000,
        })
        .expect("fonts missing? run `bash tools/fonts/fetch.sh`")
    })
}

/// tools/golden/corpus: (id, code, mermaid config), sorted by id.
pub fn corpus() -> Vec<(String, String, serde_json::Value)> {
    let dir = repo().join("tools/golden/corpus");
    let mut items: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| {
            let path = e.ok()?.path();
            let id = path.file_name()?.to_str()?.strip_suffix(".mmd")?.to_owned();
            let code = std::fs::read_to_string(&path).ok()?;
            let config = std::fs::read_to_string(dir.join(format!("{id}.config.json")))
                .map_or_else(
                    |_| serde_json::json!({"theme": "default"}),
                    |s| serde_json::from_str(&s).unwrap(),
                );
            Some((id, code, config))
        })
        .collect();
    items.sort_by(|a, b| a.0.cmp(&b.0));
    items
}

pub fn job(code: &str, config: serde_json::Value, format: OutputFormat) -> RenderJob {
    RenderJob {
        code: code.to_owned(),
        config,
        format,
        bg_color: None,
        width: None,
        height: None,
        paper: "a4".into(),
        landscape: false,
        fit: false,
        fa_css_url: "https://cdnjs.cloudflare.com/ajax/libs/font-awesome/7.2.0/css/all.min.css"
            .into(),
    }
}

pub fn render(job: &RenderJob) -> Result<Vec<u8>, RenderError> {
    render_within(job, Duration::from_secs(60))
}

pub fn render_within(job: &RenderJob, budget: Duration) -> Result<Vec<u8>, RenderError> {
    engine()
        .render(job, &HashMap::new(), Instant::now() + budget)
        .map(|o| o.body)
}

/// Corpus items the reference answers with 400 (maxEdges, syntax error, no diagram).
pub fn expected_error(id: &str) -> bool {
    matches!(id, "F13" | "F14" | "F15" | "F16" | "F17")
}

pub const FORMATS: [OutputFormat; 5] = [
    OutputFormat::Svg,
    OutputFormat::Png,
    OutputFormat::Jpeg,
    OutputFormat::Webp,
    OutputFormat::Pdf,
];
