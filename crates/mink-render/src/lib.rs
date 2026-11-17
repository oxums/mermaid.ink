//! Native Mermaid rendering (merman) to SVG/PNG/JPEG/WebP/PDF. Blocking API: callers run it on
//! `spawn_blocking`. See docs/PLAN.md "Rendering pipeline".
//!
//! Every format starts from one merman SVG render (native font measurement, the same fonts as
//! painting). /svg post-processes that string; rasters and PDFs resize its root to the box Chrome
//! would have screenshotted/printed, then go through `finalize_resvg_svg` and merman-export.

mod coverage;
mod fa;
mod pdf;
mod raster;
mod source;
mod svg;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use merman::model::runtime::RuntimePolicy;
use merman::svg::export::{
    ExportError, NativeFontContext, NativeFontError, NativeFontLimits, NativeFontOptions,
};
use merman::svg::{
    LayoutOptions, RenderResourcePolicy, ResvgCompatibleSvg, SvgRenderOptions,
    ThemeAdmissionPolicy, TrustedThemeLane, TrustedThemeLanes,
};
use merman::{
    MermaidEngine, OperationControl, RenderRequest, Renderer, SourcePresentationConfig,
    SvgEnvironment, SvgRequest, TerminalDiagnosticClass,
};
use merman_render::environment::{
    MeasurementProfileId, RenderEnvironment, TextMeasurementPhase, TextMeasurementPolicy,
    TextMeasurementProfileIdentity,
};
use mink_core::FetchedImage;
use serde_json::{Value, json};

pub struct RenderSettings {
    pub font_dir: PathBuf,
    /// Overrides merman's layout work-unit ceiling (must admit Mermaid's maxEdges=500).
    pub max_layout_work_units: Option<u64>,
    pub max_width: u32,
    pub max_height: u32,
    pub max_raster_pixels: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputFormat {
    Svg,
    Png,
    Jpeg,
    Webp,
    Pdf,
}

/// Everything needed to render one response body. Built by the server from mink-core outputs.
#[derive(Clone, Debug)]
pub struct RenderJob {
    pub code: String,
    /// Mermaid site config after the legacy semantics are applied (JSON-parsed, ?theme override).
    pub config: serde_json::Value,
    pub format: OutputFormat,
    /// Already normalized by mink_core::query (e.g. "#123123" or "slategray").
    pub bg_color: Option<String>,
    /// Final dimensions from mink_core::query extract_dimension (scale applied).
    pub width: Option<f64>,
    pub height: Option<f64>,
    /// pdf only: "a4", "letter", ...
    pub paper: String,
    pub landscape: bool,
    pub fit: bool,
    /// svg only: FontAwesome CSS URL injected as `@import` (FA_VERSION already substituted).
    pub fa_css_url: String,
}

pub struct RenderOutput {
    pub body: Vec<u8>,
    pub mime: &'static str,
}

/// A laid-out job from [`Engine::prepare`], sealed for its raster/PDF step. Its real output
/// size is known, so [`Prepared::memory_bytes`] can be reserved before [`Engine::finish`].
pub struct Prepared {
    stage: Stage,
    memory: u64,
}

enum Stage {
    Done(RenderOutput),
    Raster(Box<raster::Stage>),
    Pdf(pdf::Stage),
}

impl Prepared {
    /// Upper bound of the memory [`Engine::finish`] holds at its peak: the sealed SVGs, the usvg
    /// tree, inlined images, the pixmap and the encoder buffers (0 for SVG, already finished).
    pub fn memory_bytes(&self) -> u64 {
        self.memory
    }
}

#[derive(Debug)]
pub enum RenderError {
    /// 400 with this exact body text.
    BadRequest(String),
    /// Deadline exceeded: 503.
    Timeout,
    /// Anything else: 500.
    Internal(String),
}

/// Layout work units admitted when `max_layout_work_units` is unset. Measured: a 400-edge
/// flowchart needs 15.2M, a 500-edge one (Mermaid's maxEdges) between 20.7M and 30M; merman's
/// interactive profile stops at 14.1M. 40M leaves headroom for denser graphs.
pub const DEFAULT_LAYOUT_WORK_UNITS: u64 = 40_000_000;

/// Chrome's layout box in the legacy service: 1920px viewport minus the 8px body margins.
const CONTAINER_WIDTH: f64 = 1904.0;
const UNKNOWN_DIAGRAM: &str = "Unknown diagram error";

pub struct Engine {
    fonts: NativeFontContext,
    /// Finalize-step environment wrapping fallback text with the native fonts used for layout.
    finalize_native: RenderEnvironment,
    coverage: coverage::FontCoverage,
    engine: MermaidEngine,
    native_env: SvgEnvironment,
    deterministic_env: SvgEnvironment,
    fa: fa::FaIcons,
    max_raster_pixels: u64,
    identity: String,
}

impl Engine {
    /// Loads fonts once (fail fast) and builds the shared merman renderer.
    pub fn new(settings: RenderSettings) -> Result<Self, String> {
        let limits = NativeFontLimits {
            max_bytes: 256 << 20,
            max_measurements: 1 << 17,
            max_cache_entries: 1 << 14,
            max_cache_bytes: 32 << 20,
            ..NativeFontLimits::default()
        };
        let fonts = NativeFontOptions::default()
            .with_font_dir(&settings.font_dir)
            .with_limits(limits)
            .load(&OperationControl::new())
            .map_err(|e| format!("loading fonts from {}: {e}", settings.font_dir.display()))?;

        let work_units = settings
            .max_layout_work_units
            .unwrap_or(DEFAULT_LAYOUT_WORK_UNITS);
        let mut resources = RenderResourcePolicy::interactive();
        resources
            .apply_override("max_layout_work_units", work_units as usize)
            .map_err(|e| e.to_string())?;
        let deterministic_env = SvgEnvironment::deterministic()
            .with_resource_policy(resources)
            .with_theme_admission_policy(ThemeAdmissionPolicy::permissive().with_trusted_lanes(
                TrustedThemeLanes::from_allowed([TrustedThemeLane::RawThemeCss]),
            ));
        let native_env = deterministic_env
            .clone()
            .with_native_font_context(fonts.clone())
            .map_err(|e| e.to_string())?;
        let runtime = RuntimePolicy::deterministic()
            .try_with_system_clock()
            .map_err(|e| e.to_string())?;
        let finalize_native = native_finalize_env(&fonts).map_err(|e| e.to_string())?;
        let coverage = coverage::FontCoverage::load(&settings.font_dir)?;
        let identity = format!(
            "merman=0.10.0;fonts={};fa=7.2.0;work_units={work_units};max_raster_pixels={}",
            fonts.identity(),
            settings.max_raster_pixels
        );
        Ok(Self {
            fonts,
            finalize_native,
            coverage,
            engine: MermaidEngine::new().with_runtime_policy(runtime),
            native_env,
            deterministic_env,
            fa: fa::FaIcons::load(),
            max_raster_pixels: settings.max_raster_pixels,
            identity,
        })
    }

    /// Stable string identifying everything that changes output bytes (merman version, font set,
    /// limits). Part of the cache-key fingerprint.
    pub fn identity(&self) -> String {
        self.identity.clone()
    }

    /// https URLs of images referenced by the diagram (image shapes, <img> in labels), in order,
    /// deduplicated. The server fetches them before calling `render`.
    pub fn image_urls(&self, code: &str) -> Vec<String> {
        source::image_urls(code)
    }

    /// Cheap pre-layout lower bound of the memory `prepare` needs, for the server's memory-budget
    /// semaphore. The real figure for the raster/PDF step is [`Prepared::memory_bytes`].
    /// shortcut: the diagram size is unknown before layout, so a missing dimension is guessed
    /// (square for one side, 1904x2048 for none) and capped at max_raster_pixels.
    pub fn estimate_bytes(&self, job: &RenderJob) -> u64 {
        let svg = 16 * 1024 * 1024 + 256 * job.code.len() as u64;
        let pixels = match (job.width, job.height) {
            (Some(w), Some(h)) => w * h,
            (Some(d), None) | (None, Some(d)) => d * d,
            (None, None) => CONTAINER_WIDTH * 2048.0,
        };
        let pixels = (pixels.max(1.0) as u64).min(self.max_raster_pixels);
        svg + match job.format {
            OutputFormat::Svg => 0,
            OutputFormat::Pdf => 64 * 1024 * 1024,
            // pixmap + encoder output; WebP also holds the decoded PNG and libwebp's buffers
            OutputFormat::Png | OutputFormat::Jpeg => pixels * 8,
            OutputFormat::Webp => pixels * 16,
        }
    }

    /// Renders one job: [`Engine::prepare`] then [`Engine::finish`]. `images` maps URL -> fetched
    /// image; URLs absent from the map render as empty boxes (remote images disabled or failed).
    pub fn render(
        &self,
        job: &RenderJob,
        images: &HashMap<String, FetchedImage>,
        deadline: Instant,
    ) -> Result<RenderOutput, RenderError> {
        let prepared = self.prepare(job, images, deadline)?;
        self.finish(prepared, deadline)
    }

    /// Parses, lays out and post-processes the job (SVG is finished here). Rasters whose real
    /// canvas exceeds max_raster_pixels and oversized inlined images are BadRequest.
    pub fn prepare(
        &self,
        job: &RenderJob,
        images: &HashMap<String, FetchedImage>,
        deadline: Instant,
    ) -> Result<Prepared, RenderError> {
        let control = control_until(deadline)?;
        let (raw, native) = self.render_svg(job, images, &control)?;
        let bg = job.bg_color.as_deref();
        match job.format {
            OutputFormat::Svg => {
                let svg = svg::finish_svg(&raw, bg, job.width, job.height, &job.fa_css_url)?;
                Ok(Prepared {
                    stage: Stage::Done(RenderOutput {
                        body: svg.into_bytes(),
                        mime: "image/svg+xml",
                    }),
                    memory: 0,
                })
            }
            OutputFormat::Pdf => pdf::prepare(self, &raw, native, job, images, &control),
            format => raster::prepare(self, &raw, native, job, format, images, &control),
        }
    }

    /// Rasterizes/prints a prepared job and encodes the response body.
    pub fn finish(
        &self,
        prepared: Prepared,
        deadline: Instant,
    ) -> Result<RenderOutput, RenderError> {
        let control = control_until(deadline)?;
        match prepared.stage {
            Stage::Done(out) => Ok(out),
            Stage::Raster(stage) => raster::finish(self, *stage, &control),
            Stage::Pdf(stage) => pdf::finish(self, stage, &control),
        }
    }

    fn render_svg(
        &self,
        job: &RenderJob,
        images: &HashMap<String, FetchedImage>,
        control: &OperationControl,
    ) -> Result<(String, bool), RenderError> {
        // mermaid.mjs: initialize({...config, securityLevel: 'loose'}); null/primitives spread to {}.
        let mut site = match &job.config {
            Value::Object(map) => map.clone(),
            _ => serde_json::Map::new(),
        };
        site.insert("securityLevel".into(), json!("loose"));
        // Mermaid's own secure list; merman's default adds themeCSS, which Mermaid 12 honors.
        site.insert(
            "secure".into(),
            json!([
                "secure",
                "securityLevel",
                "startOnLoad",
                "maxTextSize",
                "suppressErrorRendering",
                "maxEdges"
            ]),
        );
        let max_text = site
            .get("maxTextSize")
            .and_then(Value::as_f64)
            .unwrap_or(50000.0);
        let code = if source::exceeds_max_text_size(&job.code, max_text) {
            source::MAX_TEXT_EXCEEDED.to_owned()
        } else {
            source::inject_image_sizes(&job.code, images)
        };
        let engine =
            self.engine
                .clone()
                .with_exact_site_config(Some(SourcePresentationConfig::from_value(Value::Object(
                    site,
                ))));
        let renderer = Renderer::new().with_presentation_engine(engine);
        let request = |environment: &SvgEnvironment| {
            let req = SvgRequest {
                environment: environment.clone(),
                // Headless Chrome reports an 800px screen whatever the viewport (C4 reads it).
                layout: LayoutOptions::headless_svg_defaults()
                    .with_container_size(CONTAINER_WIDTH, 1064.0)
                    .with_screen_available_width(800.0),
                options: SvgRenderOptions {
                    diagram_id: Some("mermaid-svg".into()),
                    ..SvgRenderOptions::default()
                },
                ..SvgRequest::default()
            };
            renderer.render_svg_data(RenderRequest::svg(&code, control.clone(), req))
        };
        let (svg, native) = match request(&self.native_env) {
            // Native measurement refuses themeCSS and inline font-family; Mermaid allows both.
            Err(merman::RenderError::NativeFont(NativeFontError::Measurement(_))) => {
                (request(&self.deterministic_env), false)
            }
            other => (other, true),
        };
        let svg = svg
            .map_err(map_render_error)?
            .ok_or_else(|| RenderError::BadRequest(UNKNOWN_DIAGRAM.into()))?;
        Ok((svg, native))
    }

    /// Sanitizes, resizes and seals the SVG for resvg/krilla. `native`: layout measured with the
    /// native fonts, so fallback text is wrapped with them too (else deterministically, as layout).
    /// Also returns the memory the inlined images take once usvg resolves them.
    fn export_svg(
        &self,
        doc: &roxmltree::Document,
        native: bool,
        canvas: (f64, f64),
        view_box: [f64; 4],
        images: &HashMap<String, FetchedImage>,
        control: &OperationControl,
    ) -> Result<(ResvgCompatibleSvg, u64), RenderError> {
        let (svg, image_bytes) =
            svg::for_export(doc, canvas, view_box, &self.fa, &self.coverage, images)?;
        let env = if native {
            self.finalize_native.clone()
        } else {
            RenderEnvironment::deterministic()
        };
        let session = env
            .begin_session_with_control(control.clone())
            .map_err(|e| RenderError::Internal(format!("finalize session: {e}")))?;
        let sealed = merman::svg::finalize_resvg_svg(&svg, &session).map_err(map_finalize_error)?;
        Ok((sealed, image_bytes))
    }
}

fn control_until(deadline: Instant) -> Result<OperationControl, RenderError> {
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .filter(|d| !d.is_zero())
        .ok_or(RenderError::Timeout)?;
    Ok(OperationControl::new().with_deadline(remaining))
}

fn native_finalize_env(
    fonts: &NativeFontContext,
) -> Result<RenderEnvironment, Box<dyn std::error::Error>> {
    let identity = TextMeasurementProfileIdentity::new(
        MeasurementProfileId::new("native-fonts")?,
        fonts.identity(),
    )?;
    let policy = TextMeasurementPolicy::host_display(
        identity,
        Arc::new(fonts.clone()),
        TextMeasurementPhase::ALL,
    );
    Ok(RenderEnvironment::deterministic().with_text_measurement_policy(policy))
}

/// Limits hit by the diagram's own size/content are the client's (cacheable 400). Other
/// failures are ours; their text may quote user markup, so only the variant name is kept.
fn map_finalize_error(e: merman::svg::RenderError) -> RenderError {
    use merman::svg::RenderError as E;
    match e {
        E::Cancelled(_) => RenderError::Timeout,
        E::ResourceLimitExceeded(_)
        | E::ThemeResourceLimitExceeded(_)
        | E::OperationResourceTerminal(_) => RenderError::BadRequest(e.to_string()),
        E::SvgPostprocess { pass, .. } => {
            RenderError::Internal(format!("finalize: postprocess pass {pass} failed"))
        }
        e => RenderError::Internal(format!("finalize: {}", variant_name(&e))),
    }
}

/// The variant name of an enum value's `Debug` output, without its payload.
fn variant_name(e: &impl std::fmt::Debug) -> String {
    format!("{e:?}")
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .next()
        .unwrap_or_default()
        .to_owned()
}

fn map_render_error(e: merman::RenderError) -> RenderError {
    use merman::RenderError as E;
    match e {
        E::Cancelled(_) => RenderError::Timeout,
        E::NoDiagram => RenderError::BadRequest(UNKNOWN_DIAGRAM.into()),
        E::Parse(d) => match d.class() {
            TerminalDiagnosticClass::Cancelled => RenderError::Timeout,
            TerminalDiagnosticClass::Internal | TerminalDiagnosticClass::RuntimePolicy => {
                RenderError::Internal(d.terminal_safe_message())
            }
            _ => {
                let code = d.terminal_diagnostic_details().code;
                if code == "merman.parse.no_diagram_detected"
                    || code == "merman.parse.unsupported_diagram"
                {
                    RenderError::BadRequest(UNKNOWN_DIAGRAM.into())
                } else {
                    RenderError::BadRequest(d.terminal_safe_message())
                }
            }
        },
        E::Export(e) => map_export_error(e),
        // Font loading, runtime policy and environment failures are ours, not the diagram's.
        E::NativeFont(_) | E::RuntimePolicy(_) | E::SvgEnvironment(_) | E::UnsupportedTarget(_) => {
            RenderError::Internal(e.to_string())
        }
        // Mermaid surfaces every other render failure as a 400 with its message.
        e => RenderError::BadRequest(e.to_string()),
    }
}

fn map_export_error(e: ExportError) -> RenderError {
    match e {
        ExportError::Cancelled(_) => RenderError::Timeout,
        e if e.resource_limit_details().is_some() => RenderError::BadRequest(e.to_string()),
        e @ (ExportError::JpegDimensionLimit
        | ExportError::EmbeddedImageLimit { .. }
        | ExportError::SvgConversionLimit { .. }
        | ExportError::PdfFilterImageLimit { .. }) => RenderError::BadRequest(e.to_string()),
        e => RenderError::Internal(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn prepared_jobs_cross_threads() {
        fn send<T: Send + 'static>() {}
        send::<super::Prepared>();
    }

    #[test]
    fn variant_names_drop_payloads() {
        #[derive(Debug)]
        #[allow(dead_code)]
        enum E {
            Parse { message: String },
            Tuple(String),
        }
        let leak = "expected 'img' tag".to_owned();
        assert_eq!(
            super::variant_name(&E::Parse {
                message: leak.clone()
            }),
            "Parse"
        );
        assert_eq!(super::variant_name(&E::Tuple(leak)), "Tuple");
    }
}
