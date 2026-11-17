//! HTTP surface: routing, CORS, error mapping, admission and the render pipeline.
//! The behaviour contract is legacy/ (Koa + koa-route + @koa/cors); deliberate divergences are
//! listed in tests/fixtures/divergences.toml.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::server::graceful::GracefulShutdown;
use hyper_util::service::TowerToHyperService;
use mink_cache::{Cache, CacheSettings, Cached};
use mink_core::decode::{decode, prepare};
use mink_core::key::create_cache_key;
use mink_core::query::{self, JsLimit, Query};
use mink_fetch::{FetchSettings, Fetcher};
use mink_render::{Engine, OutputFormat, RenderError, RenderJob, RenderSettings};
use sha2::{Digest, Sha256};
use tokio::sync::Semaphore;

use crate::config::Config;

const HOME: &str = include_str!("../static/index.html");
const TEXT: &str = "text/plain; charset=utf-8";
/// /svg only: the FA `@import` still loads, but handlers, scripts and javascript: links are inert,
/// and HTML-label forms cannot post anywhere (form-action does not fall back to default-src).
const SVG_CSP: &str = "default-src 'none'; style-src 'unsafe-inline' https:; font-src https: data:; img-src https: data:; form-action 'none'; base-uri 'none'";
/// A connection that has not sent a complete request head within this is closed (slowloris).
pub const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(10);

pub type RenderHook = Arc<dyn Fn() + Send + Sync>;

pub struct App {
    config: Config,
    engine: Arc<Engine>,
    fetcher: Option<Fetcher>,
    cache: Cache,
    fingerprint: String,
    /// Requests waiting for admission (QUEUE_MAX_PENDING).
    pending: AtomicUsize,
    slots: Arc<Semaphore>,
    /// Permits are MiB of RENDER_MEMORY_BUDGET_MB.
    memory: Arc<Semaphore>,
    render_hook: Option<RenderHook>,
}

impl App {
    /// Loads fonts and builds the engine, fetcher (when REMOTE_IMAGES) and caches.
    /// Must run inside a Tokio runtime (Redis connects in the background).
    pub fn new(config: Config) -> Result<Self, String> {
        let fetcher = config
            .remote_images
            .then(|| {
                Fetcher::new(FetchSettings {
                    timeout: config.remote_image_timeout,
                    max_bytes: config.remote_image_max_bytes,
                    max_count: config.remote_image_max_count,
                    user_agent: concat!("mermaid-ink/", env!("CARGO_PKG_VERSION")).into(),
                })
            })
            .transpose()?;
        Self::with_fetcher(config, fetcher)
    }

    /// Like `new`, with an explicit fetcher (None disables remote images).
    pub fn with_fetcher(config: Config, fetcher: Option<Fetcher>) -> Result<Self, String> {
        let limit = |l: &JsLimit| l.to_string().parse::<f64>().map_or(10000, |n| n as u32);
        let engine = Engine::new(RenderSettings {
            font_dir: config.font_dir.clone(),
            max_layout_work_units: Some(config.max_layout_work_units),
            max_width: limit(&config.limits.max_width),
            max_height: limit(&config.limits.max_height),
            max_raster_pixels: config.max_raster_pixels,
        })?;
        let fingerprint = hex(&Sha256::digest(format!(
            "{}\n{}\n{}\n{}",
            engine.identity(),
            config.fa_css_url,
            fetcher.is_some(),
            config.max_layout_work_units
        )));
        let cache = Cache::new(CacheSettings {
            memory_bytes: config.cache_memory_mb << 20,
            redis_url: config.redis_url.clone(),
            redis_ttl: config.redis_ttl,
            redis_timeout: config.redis_timeout,
        });
        Ok(Self {
            slots: Arc::new(Semaphore::new(config.queue_concurrency)),
            memory: Arc::new(Semaphore::new(config.render_memory_budget_mb as usize)),
            pending: AtomicUsize::new(0),
            engine: Arc::new(engine),
            fetcher,
            cache,
            fingerprint,
            config,
            render_hook: None,
        })
    }

    /// Runs `hook` on the blocking render thread right before every render (tests only).
    #[cfg(feature = "test-hooks")]
    pub fn with_render_hook(mut self, hook: RenderHook) -> Self {
        self.render_hook = Some(hook);
        self
    }

    /// Validates, decodes and prepares the request into a render job; Err is the 400 body.
    fn job(&self, kind: Kind, encoded: &str, q: &Query) -> Result<RenderJob, String> {
        let limits = &self.config.limits;
        query::validate_query(q, limits)?;
        let state = decode(encoded, self.config.max_decoded_bytes).map_err(|e| e.to_string())?;
        let (code, config) = prepare(state, query::extract_theme(q))?;
        let dim = query::extract_dimension(Some(q), limits);
        let format = match kind {
            Kind::Svg => OutputFormat::Svg,
            Kind::Pdf => OutputFormat::Pdf,
            Kind::Img => match query::extract_image_type(q) {
                "png" => OutputFormat::Png,
                "webp" => OutputFormat::Webp,
                _ => OutputFormat::Jpeg,
            },
        };
        Ok(RenderJob {
            code,
            config,
            format,
            bg_color: query::extract_bg_color(q),
            width: dim.and_then(|d| d.width),
            height: dim.and_then(|d| d.height),
            paper: query::extract_paper(q).into(),
            landscape: query::is_landscape(q),
            fit: query::is_fit(q),
            fa_css_url: self.config.fa_css_url.clone(),
        })
    }

    /// Admission, remote images and the blocking render.
    async fn produce(&self, job: RenderJob) -> Cached {
        if self.pending.fetch_add(1, Ordering::SeqCst) >= self.config.queue_max_pending {
            self.pending.fetch_sub(1, Ordering::SeqCst);
            return status_text(StatusCode::SERVICE_UNAVAILABLE);
        }
        let waiting = PendingGuard(&self.pending);

        let mut images = HashMap::new();
        let mut degraded = false;
        if let Some(fetcher) = &self.fetcher {
            let urls = self.engine.image_urls(&job.code);
            if !urls.is_empty() {
                for (url, res) in fetcher.fetch_all(&urls).await {
                    match res {
                        Ok(img) => {
                            images.insert(url, img);
                        }
                        Err(e) => {
                            tracing::debug!(error = ?e, "remote image failed");
                            degraded = true;
                        }
                    }
                }
            }
        }

        let wait = self.config.queue_add_timeout;
        let Ok(Ok(slot)) = tokio::time::timeout(wait, self.slots.clone().acquire_owned()).await
        else {
            return status_text(StatusCode::SERVICE_UNAVAILABLE);
        };
        drop(waiting);

        // Layout first: only then is the real canvas, and so the raster/PDF memory, known.
        let deadline = Instant::now() + wait;
        let (engine, hook) = (self.engine.clone(), self.render_hook.clone());
        let prepared = tokio::task::spawn_blocking(move || {
            if let Some(hook) = hook {
                hook();
            }
            engine.prepare(&job, &images, deadline)
        })
        .await;
        let rendered = match prepared {
            Ok(Ok(prepared)) => {
                let budget = u64::from(self.config.render_memory_budget_mb);
                let mib = prepared.memory_bytes().div_ceil(1 << 20);
                if mib > budget {
                    Ok(Err(RenderError::BadRequest(
                        "diagram too large for the memory budget".into(),
                    )))
                } else {
                    let memory = tokio::time::timeout(
                        wait,
                        self.memory.clone().acquire_many_owned(mib as u32),
                    )
                    .await;
                    let Ok(Ok(memory)) = memory else {
                        return status_text(StatusCode::SERVICE_UNAVAILABLE);
                    };
                    let engine = self.engine.clone();
                    tokio::task::spawn_blocking(move || {
                        // Held until the render ends, even if the request is dropped meanwhile.
                        let _permits = (slot, memory);
                        engine.finish(prepared, deadline)
                    })
                    .await
                }
            }
            Ok(Err(e)) => Ok(Err(e)),
            Err(e) => Err(e),
        };
        // Error texts can quote diagram content: only the debug level carries them.
        match rendered {
            Ok(Ok(out)) => Cached {
                status: 200,
                mime: out.mime.into(),
                body: out.body.into(),
                cacheable: !degraded,
            },
            Ok(Err(RenderError::BadRequest(msg))) => Cached {
                status: 400,
                mime: TEXT.into(),
                body: msg.into(),
                cacheable: !degraded,
            },
            Ok(Err(RenderError::Timeout)) => status_text(StatusCode::SERVICE_UNAVAILABLE),
            Ok(Err(RenderError::Internal(msg))) => {
                tracing::error!("render failed with an internal error");
                tracing::debug!(error = %msg, "render failed");
                status_text(StatusCode::INTERNAL_SERVER_ERROR)
            }
            Err(e) => {
                tracing::error!(panicked = e.is_panic(), "render task failed");
                tracing::debug!(error = %e, "render task failed");
                status_text(StatusCode::INTERNAL_SERVER_ERROR)
            }
        }
    }
}

struct PendingGuard<'a>(&'a AtomicUsize);

impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[derive(Clone, Copy)]
enum Kind {
    Img,
    Svg,
    Pdf,
}

impl Kind {
    fn asset(self) -> &'static str {
        match self {
            Kind::Img => "img",
            Kind::Svg => "svg",
            Kind::Pdf => "pdf",
        }
    }
}

enum Route<'a> {
    Home,
    Render(Kind, &'a str),
}

/// koa-route (path-to-regexp, case-insensitive, non-strict): `/` and `/{img,svg,pdf}/:code`,
/// one segment, one optional trailing slash.
fn route(path: &str) -> Option<Route<'_>> {
    if path == "/" || path == "//" {
        return Some(Route::Home);
    }
    let rest = path.strip_prefix('/')?;
    let rest = rest.strip_suffix('/').unwrap_or(rest);
    let (kind, param) = rest.split_once('/')?;
    if param.is_empty() || param.contains('/') {
        return None;
    }
    let kind = match kind.to_ascii_lowercase().as_str() {
        "img" => Kind::Img,
        "svg" => Kind::Svg,
        "pdf" => Kind::Pdf,
        _ => return None,
    };
    Some(Route::Render(kind, param))
}

/// JS `decodeURIComponent`: None where it would throw a URIError.
fn decode_uri_component(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = b
                .get(i + 1..i + 3)
                .filter(|h| h.iter().all(u8::is_ascii_hexdigit))?;
            out.push(u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// 5xx bodies are the status text, as Koa's onerror does for unexposed errors. Never cached.
fn status_text(status: StatusCode) -> Cached {
    Cached {
        status: status.as_u16(),
        mime: TEXT.into(),
        body: Bytes::from_static(status.canonical_reason().unwrap_or("").as_bytes()),
        cacheable: false,
    }
}

fn reply(status: StatusCode, mime: &str, body: Bytes) -> Response {
    let len = body.len();
    let mut res = (status, Body::from(body)).into_response();
    let h = res.headers_mut();
    if let Ok(v) = HeaderValue::from_str(mime) {
        h.insert(header::CONTENT_TYPE, v);
    }
    h.insert(header::CONTENT_LENGTH, len.into());
    res
}

fn not_found() -> Response {
    reply(
        StatusCode::NOT_FOUND,
        TEXT,
        Bytes::from_static(b"Not Found"),
    )
}

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .fallback(dispatch)
        .layer(middleware::from_fn(cors))
        .layer(middleware::from_fn_with_state(app.clone(), url_limit))
        .with_state(app)
}

/// Node's header limit answered 431 with no headers and no body; we cap the request target.
async fn url_limit(State(app): State<Arc<App>>, req: Request, next: Next) -> Response {
    let len = req.uri().path_and_query().map_or(0, |p| p.as_str().len());
    if len > app.config.max_url_bytes {
        return StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE.into_response();
    }
    next.run(req).await
}

/// @koa/cors 5 with `allowMethods: 'GET'` and `origin: Origin || '*'`.
async fn cors(req: Request, next: Next) -> Response {
    let headers = req.headers().clone();
    let options = req.method() == Method::OPTIONS;
    let preflight = options && headers.contains_key(header::ACCESS_CONTROL_REQUEST_METHOD);
    let mut res = if preflight {
        let mut res = StatusCode::NO_CONTENT.into_response();
        let h = res.headers_mut();
        h.insert(
            header::ACCESS_CONTROL_ALLOW_METHODS,
            HeaderValue::from_static("GET"),
        );
        if let Some(v) = headers.get(header::ACCESS_CONTROL_REQUEST_HEADERS) {
            h.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, v.clone());
        }
        res
    } else {
        next.run(req).await
    };
    let h = res.headers_mut();
    h.insert(header::VARY, HeaderValue::from_static("Origin"));
    // A plain OPTIONS (no request method) passes through without CORS headers.
    if !options || preflight {
        let origin = headers
            .get(header::ORIGIN)
            .cloned()
            .unwrap_or(HeaderValue::from_static("*"));
        h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
    }
    res
}

async fn dispatch(State(app): State<Arc<App>>, req: Request) -> Response {
    let head = req.method() == Method::HEAD;
    if req.method() != Method::GET && !head {
        return not_found();
    }
    let res = match route(req.uri().path()) {
        None => not_found(),
        Some(Route::Home) => reply(
            StatusCode::OK,
            "text/html; charset=utf-8",
            Bytes::from_static(HOME.as_bytes()),
        ),
        Some(Route::Render(kind, param)) => {
            render(&app, kind, param, req.uri().query(), req.headers()).await
        }
    };
    if head {
        // Same headers (Content-Length included), no body.
        let (parts, _) = res.into_parts();
        return Response::from_parts(parts, Body::empty());
    }
    res
}

async fn render(
    app: &Arc<App>,
    kind: Kind,
    param: &str,
    raw_query: Option<&str>,
    headers: &HeaderMap,
) -> Response {
    // Legacy: an unguarded decodeURIComponent URIError became a 500.
    let Some(encoded) = decode_uri_component(param) else {
        let c = status_text(StatusCode::INTERNAL_SERVER_ERROR);
        return reply(StatusCode::INTERNAL_SERVER_ERROR, &c.mime, c.body);
    };
    let q = Query::parse(raw_query.unwrap_or(""));
    let key = format!(
        "{}:{}",
        app.fingerprint,
        create_cache_key(kind.asset(), &encoded, Some(&q), &app.config.limits)
    );
    let etag = format!("W/\"{}\"", &hex(&Sha256::digest(&key))[..32]);
    let cache_control = (app.config.http_cache_max_age > 0)
        .then(|| format!("public, max-age={}", app.config.http_cache_max_age));
    let cache_headers = |res: &mut Response| {
        let h = res.headers_mut();
        h.insert(
            header::ETAG,
            HeaderValue::from_str(&etag).expect("hex etag"),
        );
        if let Some(cc) = &cache_control {
            h.insert(
                header::CACHE_CONTROL,
                HeaderValue::from_str(cc).expect("ascii"),
            );
        }
    };

    let matches_etag = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.split(',')
                .any(|t| t.trim() == "*" || t.trim().trim_start_matches("W/") == &etag[2..])
        });
    if matches_etag {
        let mut res = StatusCode::NOT_MODIFIED.into_response();
        cache_headers(&mut res);
        return res;
    }

    let cached = match app.job(kind, &encoded, &q) {
        Err(msg) => Cached {
            status: 400,
            mime: TEXT.into(),
            body: msg.into(),
            cacheable: true,
        },
        // Coalesced waiters share the producer's Cached, cacheable flag included.
        Ok(job) => app.cache.get_or_render(&key, || app.produce(job)).await,
    };

    let status = StatusCode::from_u16(cached.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let mut res = reply(status, &cached.mime, cached.body);
    let h = res.headers_mut();
    if status == StatusCode::SERVICE_UNAVAILABLE {
        h.insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
    }
    if cached.mime == "image/svg+xml" {
        h.insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(SVG_CSP),
        );
    }
    if cached.cacheable {
        cache_headers(&mut res);
    }
    res
}

/// Serves HTTP/1.1 until `shutdown` resolves, then drains in-flight requests. Unlike
/// `axum::serve`, request heads must arrive within HEADER_READ_TIMEOUT and at most
/// MAX_CONNECTIONS connections are open at once (further clients wait in the listen backlog).
pub async fn serve(
    listener: tokio::net::TcpListener,
    app: Arc<App>,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    let connections = Arc::new(Semaphore::new(app.config.max_connections));
    let service = TowerToHyperService::new(router(app));
    let mut http = hyper::server::conn::http1::Builder::new();
    http.timer(TokioTimer::new())
        .header_read_timeout(HEADER_READ_TIMEOUT);
    let graceful = GracefulShutdown::new();
    tokio::pin!(shutdown);
    loop {
        let permit = tokio::select! {
            () = &mut shutdown => break,
            p = connections.clone().acquire_owned() => p,
        };
        let Ok(permit) = permit else { break };
        let accepted = tokio::select! {
            () = &mut shutdown => break,
            r = listener.accept() => r,
        };
        let stream = match accepted {
            Ok((stream, _)) => stream,
            Err(e) => {
                use std::io::ErrorKind::{ConnectionAborted, ConnectionRefused, ConnectionReset};
                // Out of file descriptors and the like: back off, as hyper 0.14 and axum do.
                if !matches!(
                    e.kind(),
                    ConnectionRefused | ConnectionAborted | ConnectionReset
                ) {
                    tracing::error!("accept error: {e}");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
                continue;
            }
        };
        let conn = graceful.watch(http.serve_connection(TokioIo::new(stream), service.clone()));
        tokio::spawn(async move {
            let _ = conn.await;
            drop(permit);
        });
    }
    drop(listener);
    graceful.shutdown().await;
    Ok(())
}

/// Resolves on ctrl-c or SIGTERM.
pub async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {},
        () = term => {},
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_like_koa_route() {
        let r = |p| match route(p) {
            None => "none".to_owned(),
            Some(Route::Home) => "home".to_owned(),
            Some(Route::Render(k, c)) => format!("{}:{c}", k.asset()),
        };
        assert_eq!(r("/"), "home");
        assert_eq!(r("//"), "home");
        assert_eq!(r("/svg/abc"), "svg:abc");
        assert_eq!(r("/IMG/abc/"), "img:abc");
        assert_eq!(r("/Pdf/a%2Fb"), "pdf:a%2Fb");
        for p in [
            "/img", "/img/", "/img//", "/svg/a/b", "/svg/a//", "/foo/a", "/svgx/a",
        ] {
            assert_eq!(r(p), "none", "{p}");
        }
    }

    #[test]
    fn decode_uri_component_like_js() {
        let d = decode_uri_component;
        assert_eq!(d("a%2Fb%20c").as_deref(), Some("a/b c"));
        assert_eq!(d("a+b").as_deref(), Some("a+b"));
        assert_eq!(d("%E2%82%AC").as_deref(), Some("\u{20ac}"));
        for bad in [
            "%E0%A4%A",
            "%",
            "%G0",
            "%+1",
            "%E0%A4",
            "%C0%80",
            "%ED%A0%80",
        ] {
            assert_eq!(d(bad), None, "{bad}");
        }
    }
}
