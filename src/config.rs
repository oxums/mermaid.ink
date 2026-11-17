//! Configuration from environment variables (docs/PLAN.md "Environment variables").
//! Every variable fails at boot on an unparseable value, except MAX_WIDTH/MAX_HEIGHT,
//! which keep legacy's JS semantics (mink_core::query::Limits).

use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use mink_core::query::Limits;

/// FontAwesome version the svg `@import` points at (the FA7 glyphs bundled by tools/fonts).
pub const FA_VERSION: &str = "7.2.0";

#[derive(Clone, Debug)]
pub struct Config {
    pub host: String,
    pub port: u16,
    /// Concurrently open client connections (MAX_CONNECTIONS).
    pub max_connections: usize,
    pub limits: Limits,
    /// FONT_AWESOME_CSS_URL with `FA_VERSION` substituted.
    pub fa_css_url: String,
    pub queue_concurrency: usize,
    /// Admission wait budget and render deadline (QUEUE_ADD_TIMEOUT, ms).
    pub queue_add_timeout: Duration,
    pub queue_max_pending: usize,
    pub render_memory_budget_mb: u32,
    pub max_raster_pixels: u64,
    pub max_decoded_bytes: usize,
    pub max_url_bytes: usize,
    pub max_layout_work_units: u64,
    pub font_dir: PathBuf,
    pub cache_memory_mb: u64,
    pub redis_url: Option<String>,
    pub redis_ttl: Duration,
    pub redis_timeout: Duration,
    /// 0 disables the Cache-Control header.
    pub http_cache_max_age: u64,
    pub remote_images: bool,
    pub remote_image_timeout: Duration,
    pub remote_image_max_bytes: usize,
    pub remote_image_max_count: usize,
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        Self::from_lookup(|k| std::env::var(k).ok())
    }

    /// `get` returns a variable's raw value; empty values count as unset.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let var = |k: &str| get(k).filter(|v| !v.is_empty());
        fn parse<T: FromStr>(k: &str, v: Option<String>, default: T) -> Result<T, String> {
            v.map_or(Ok(default), |v| {
                v.trim().parse().map_err(|_| format!("invalid {k}: {v:?}"))
            })
        }
        let num = |k: &str, default: u64| parse(k, var(k), default);
        let ms = |k: &str, default: u64| num(k, default).map(Duration::from_millis);
        let remote_images = match var("REMOTE_IMAGES").as_deref().map(str::trim) {
            None | Some("true" | "1") => true,
            Some("false" | "0") => false,
            Some(v) => return Err(format!("invalid REMOTE_IMAGES: {v:?}")),
        };
        let cpus = std::thread::available_parallelism().map_or(1, usize::from);
        Ok(Self {
            host: var("HOST").unwrap_or_else(|| "0.0.0.0".into()),
            port: parse("PORT", var("PORT"), 3000)?,
            max_connections: parse("MAX_CONNECTIONS", var("MAX_CONNECTIONS"), 10_000)?.max(1),
            limits: Limits::from_env_values(
                get("MAX_WIDTH").as_deref(),
                get("MAX_HEIGHT").as_deref(),
            ),
            fa_css_url: var("FONT_AWESOME_CSS_URL")
                .map(|u| u.replace("FA_VERSION", FA_VERSION))
                .unwrap_or_else(|| {
                    format!(
                        "https://cdnjs.cloudflare.com/ajax/libs/font-awesome/{FA_VERSION}/css/all.min.css"
                    )
                }),
            queue_concurrency: parse("QUEUE_CONCURRENCY", var("QUEUE_CONCURRENCY"), cpus)?.max(1),
            queue_add_timeout: ms("QUEUE_ADD_TIMEOUT", 3000)?,
            queue_max_pending: parse("QUEUE_MAX_PENDING", var("QUEUE_MAX_PENDING"), 256)?,
            render_memory_budget_mb: parse(
                "RENDER_MEMORY_BUDGET_MB",
                var("RENDER_MEMORY_BUDGET_MB"),
                1024,
            )?
            .max(1),
            max_raster_pixels: num("MAX_RASTER_PIXELS", 100_000_000)?,
            max_decoded_bytes: parse("MAX_DECODED_BYTES", var("MAX_DECODED_BYTES"), 1 << 20)?,
            max_url_bytes: parse("MAX_URL_BYTES", var("MAX_URL_BYTES"), 16384)?,
            max_layout_work_units: num(
                "MAX_LAYOUT_WORK_UNITS",
                mink_render::DEFAULT_LAYOUT_WORK_UNITS,
            )?,
            font_dir: var("FONT_DIR")
                .unwrap_or_else(|| "/usr/share/mermaid-ink/fonts".into())
                .into(),
            cache_memory_mb: num("CACHE_MEMORY_MB", 25)?,
            redis_url: var("REDIS_URL"),
            redis_ttl: num("REDIS_TTL_SECS", 604_800).map(Duration::from_secs)?,
            redis_timeout: ms("REDIS_TIMEOUT_MS", 150)?,
            http_cache_max_age: num("HTTP_CACHE_MAX_AGE", 604_800)?,
            remote_images,
            remote_image_timeout: ms("REMOTE_IMAGE_TIMEOUT_MS", 2000)?,
            remote_image_max_bytes: parse(
                "REMOTE_IMAGE_MAX_BYTES",
                var("REMOTE_IMAGE_MAX_BYTES"),
                5_242_880,
            )?,
            remote_image_max_count: parse(
                "REMOTE_IMAGE_MAX_COUNT",
                var("REMOTE_IMAGE_MAX_COUNT"),
                10,
            )?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mink_core::query::JsLimit;

    fn cfg(vars: &[(&str, &str)]) -> Result<Config, String> {
        Config::from_lookup(|k| {
            vars.iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.to_string())
        })
    }

    #[test]
    fn defaults() {
        let c = cfg(&[]).unwrap();
        assert_eq!((c.host.as_str(), c.port), ("0.0.0.0", 3000));
        assert_eq!(c.max_connections, 10_000);
        assert_eq!(c.limits.max_width, JsLimit::Number(10000.0));
        assert_eq!(
            c.fa_css_url,
            "https://cdnjs.cloudflare.com/ajax/libs/font-awesome/7.2.0/css/all.min.css"
        );
        assert_eq!(c.queue_add_timeout, Duration::from_secs(3));
        assert_eq!(c.queue_max_pending, 256);
        assert_eq!(c.max_decoded_bytes, 1 << 20);
        assert_eq!(c.max_url_bytes, 16384);
        assert_eq!(
            c.max_layout_work_units,
            mink_render::DEFAULT_LAYOUT_WORK_UNITS
        );
        assert_eq!(c.cache_memory_mb, 25);
        assert_eq!(c.redis_url, None);
        assert_eq!(c.http_cache_max_age, 604_800);
        assert!(c.remote_images);
        assert!(c.queue_concurrency >= 1);
    }

    #[test]
    fn overrides_and_errors() {
        let c = cfg(&[
            ("PORT", "8080"),
            ("MAX_WIDTH", "abc"),
            (
                "FONT_AWESOME_CSS_URL",
                "https://x/FA_VERSION/a-FA_VERSION.css",
            ),
            ("REMOTE_IMAGES", "false"),
            ("QUEUE_ADD_TIMEOUT", ""),
        ])
        .unwrap();
        assert_eq!(c.port, 8080);
        // JS semantics: kept as a string, never a boot error.
        assert_eq!(c.limits.max_width, JsLimit::String("abc".into()));
        assert_eq!(c.fa_css_url, "https://x/7.2.0/a-7.2.0.css");
        assert!(!c.remote_images);
        assert_eq!(c.queue_add_timeout, Duration::from_secs(3));

        for (k, v) in [
            ("PORT", "x"),
            ("QUEUE_ADD_TIMEOUT", "3s"),
            ("CACHE_MEMORY_MB", "-1"),
            ("REMOTE_IMAGES", "yes"),
            ("MAX_URL_BYTES", "1e4"),
            ("MAX_CONNECTIONS", "-5"),
        ] {
            let err = cfg(&[(k, v)]).unwrap_err();
            assert!(err.contains(k), "{err}");
        }
    }
}
