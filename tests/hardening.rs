//! Server hardening: XSS stripping, decode/URL caps, admission (load shedding, deadline, panic),
//! raster limits, cache behaviour (Redis down, LRU bound, ETag/304), graceful shutdown and remote
//! images (loopback test fetcher, SSRF refusal). Needs ./fonts or FONT_DIR.

mod common;

use std::io::Cursor;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use axum::Router;
use axum::http::Method;
use common::{app, config, get, pako, send};
use mermaid_ink::App;
use mermaid_ink::app::RenderHook;
use mink_fetch::{FetchSettings, Fetcher};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const FLOW: &str = "flowchart TD\n  A[Start] --> B{Is it?}\n  B -->|Yes| C[OK]";

fn hooked(vars: &[(&str, &str)], hook: impl Fn() + Send + Sync + 'static) -> Router {
    let hook: RenderHook = Arc::new(hook);
    let app = App::new(config(vars)).unwrap().with_render_hook(hook);
    mermaid_ink::router(Arc::new(app))
}

/// Router plus a counter of renders that reached the engine.
fn counted(vars: &[(&str, &str)]) -> (Router, Arc<AtomicUsize>) {
    let n = Arc::new(AtomicUsize::new(0));
    let c = n.clone();
    let router = hooked(vars, move || {
        c.fetch_add(1, Ordering::SeqCst);
    });
    (router, n)
}

fn corpus(id: &str) -> String {
    let all: serde_json::Value = serde_json::from_slice(
        &std::fs::read(common::repo().join("tests/fixtures/corpus/encoded.json")).unwrap(),
    )
    .unwrap();
    all["items"][id]["pako"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn svg_output_has_no_handlers_scripts_or_javascript_urls() {
    let router = mermaid_ink::router(app(&[]));
    let mut payloads: Vec<String> = ["F21a", "F21b", "F21c"].map(corpus).into();
    for code in [
        "flowchart TD\n  A[\"<img src=x onerror=alert(1)>\"] --> B[\"<svg onload=alert(2)>\"]",
        "flowchart TD\n  A[\"<a href='javascript:alert(1)'>x</a>\"]\n  click A call alert(3)",
        "flowchart TD\n  A[\"<math><mi xlink:href='javascript:alert(1)'>x</mi></math>\"]",
        "sequenceDiagram\n  A->>B: <img src=x onerror=alert(1)>",
    ] {
        payloads.push(pako(code));
    }
    for p in payloads {
        let resp = get(&router, &format!("/svg/{p}")).await;
        assert_eq!(resp.status, 200, "{p}: {}", resp.text());
        let csp = resp.header("content-security-policy").unwrap();
        assert!(
            csp.contains("form-action 'none'") && csp.contains("base-uri 'none'"),
            "{csp}"
        );
        let text = resp.text();
        let body = text.to_ascii_lowercase();
        assert!(!body.contains("<script"), "{p}");
        assert!(!body.contains("javascript:"), "{p}");
        let doc = roxmltree::Document::parse(&text).unwrap();
        for n in doc.descendants().filter(|n| n.is_element()) {
            for a in n.attributes() {
                assert!(
                    !a.name().to_ascii_lowercase().starts_with("on"),
                    "{p}: {}={}",
                    a.name(),
                    a.value()
                );
            }
        }
    }
}

#[tokio::test]
async fn inflate_bomb_is_400() {
    let router = mermaid_ink::router(app(&[]));
    let bomb = pako(&format!("graph TD\n{}", " ".repeat(8 << 20)));
    assert!(bomb.len() < 16384, "{}", bomb.len());
    let resp = get(&router, &format!("/svg/{bomb}")).await;
    assert_eq!(resp.status, 400);
    assert_eq!(resp.text(), "Decoded diagram is too large");
}

#[tokio::test]
async fn memory_budget_uses_the_real_canvas() {
    // No width/height: the pre-layout guess would be small, the real canvas is ~1904x10000.
    let tasks: String = (0..400)
        .map(|i| format!("  t{i} :2024-01-01, 1d\n"))
        .collect();
    let tall = pako(&format!(
        "gantt\n  dateFormat YYYY-MM-DD\n  section S\n{tasks}"
    ));
    let router = mermaid_ink::router(app(&[("RENDER_MEMORY_BUDGET_MB", "64")]));
    let resp = get(&router, &format!("/img/{tall}?type=png")).await;
    assert_eq!(resp.status, 400, "{}", resp.text());
    assert_eq!(resp.text(), "diagram too large for the memory budget");
    // Same diagram as SVG needs no raster memory; a small diagram still fits the budget.
    assert_eq!(get(&router, &format!("/svg/{tall}")).await.status, 200);
    let small = get(&router, &format!("/img/{}?type=png", pako(FLOW))).await;
    assert_eq!(small.status, 200, "{}", small.text());
}

#[tokio::test]
async fn oversized_url_is_431_without_headers() {
    let router = mermaid_ink::router(app(&[]));
    let resp = get(&router, &format!("/svg/{}", "A".repeat(16384))).await;
    assert_eq!(resp.status, 431);
    assert!(resp.body.is_empty());
    assert!(resp.header("access-control-allow-origin").is_none());
    // Just under the cap still routes.
    let path = format!("/svg/{}", "A".repeat(16384 - 5));
    assert_ne!(get(&router, &path).await.status, 431);
}

#[tokio::test]
async fn load_shedding_returns_503_immediately() {
    let router = hooked(
        &[
            ("QUEUE_CONCURRENCY", "1"),
            ("QUEUE_MAX_PENDING", "1"),
            ("QUEUE_ADD_TIMEOUT", "10000"),
        ],
        || std::thread::sleep(Duration::from_millis(400)),
    );
    let started = Instant::now();
    let tasks: Vec<_> = (0..4)
        .map(|i| {
            let router = router.clone();
            tokio::spawn(async move {
                let resp = get(
                    &router,
                    &format!("/svg/{}", pako(&format!("graph TD\nA{i}-->B"))),
                )
                .await;
                (
                    resp.status,
                    resp.header("retry-after").map(str::to_owned),
                    started.elapsed(),
                )
            })
        })
        .collect();
    let mut results = Vec::new();
    for t in tasks {
        results.push(t.await.unwrap());
    }
    let shed: Vec<_> = results.iter().filter(|r| r.0 == 503).collect();
    assert!(results.iter().any(|r| r.0 == 200), "{results:?}");
    assert!(!shed.is_empty(), "{results:?}");
    for (_, retry, at) in shed {
        assert_eq!(retry.as_deref(), Some("1"));
        assert!(*at < Duration::from_millis(350), "shed after {at:?}");
    }
}

#[tokio::test]
async fn deadline_is_503_and_not_cached() {
    let n = Arc::new(AtomicUsize::new(0));
    let count = n.clone();
    let router = hooked(&[("QUEUE_ADD_TIMEOUT", "50")], move || {
        count.fetch_add(1, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(150));
    });
    let path = format!("/svg/{}", pako(FLOW));
    for _ in 0..2 {
        let resp = get(&router, &path).await;
        assert_eq!(resp.status, 503);
        assert_eq!(resp.text(), "Service Unavailable");
        assert_eq!(resp.header("retry-after"), Some("1"));
        assert!(resp.header("cache-control").is_none() && resp.header("etag").is_none());
    }
    assert_eq!(n.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn render_panic_is_500_and_server_keeps_serving() {
    let panicked = Arc::new(AtomicBool::new(false));
    let p = panicked.clone();
    let router = hooked(&[], move || {
        if !p.swap(true, Ordering::SeqCst) {
            panic!("injected render panic");
        }
    });
    let path = format!("/svg/{}", pako(FLOW));
    let resp = get(&router, &path).await;
    assert_eq!(resp.status, 500);
    assert_eq!(resp.text(), "Internal Server Error");
    assert_eq!(
        resp.header("content-type"),
        Some("text/plain; charset=utf-8")
    );
    assert_eq!(get(&router, &path).await.status, 200);
}

#[tokio::test]
async fn webp_wider_than_16383_is_400() {
    let router = mermaid_ink::router(app(&[]));
    let path = format!(
        "/img/{}?type=webp&width=17000",
        pako("graph LR\nA-->B-->C-->D")
    );
    let resp = get(&router, &path).await;
    assert_eq!(resp.status, 400, "{}", resp.text());
}

#[tokio::test]
async fn redis_unreachable_still_renders() {
    let router = mermaid_ink::router(app(&[
        ("REDIS_URL", "redis://127.0.0.1:1"),
        ("REDIS_TIMEOUT_MS", "100"),
    ]));
    let started = Instant::now();
    let resp = get(&router, &format!("/svg/{}", pako(FLOW))).await;
    assert_eq!(resp.status, 200);
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn memory_cache_is_byte_bounded() {
    let (router, renders) = counted(&[("CACHE_MEMORY_MB", "1")]);
    let small = format!("/svg/{}", pako(FLOW));
    get(&router, &small).await;
    get(&router, &small).await;
    assert_eq!(
        renders.load(Ordering::SeqCst),
        1,
        "second request is an L1 hit"
    );

    // Push well over 1 MiB of other responses through the cache.
    let mut total = 0;
    for i in 0..40 {
        let big = format!(
            "/img/{}?type=png&width=3000",
            pako(&format!("graph TD\nA{i}-->B{i}-->C"))
        );
        total += get(&router, &big).await.body.len();
        if total > 2 << 20 {
            break;
        }
    }
    assert!(total > 2 << 20, "only {total} bytes cached");
    // moka applies evictions during housekeeping, which runs after its sync delay.
    tokio::time::sleep(Duration::from_millis(600)).await;
    get(&router, &format!("/svg/{}", pako("graph TD\nX-->Y"))).await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    let before = renders.load(Ordering::SeqCst);
    get(&router, &small).await;
    assert_eq!(
        renders.load(Ordering::SeqCst),
        before + 1,
        "least recently used entry was evicted"
    );

    let (router, renders) = counted(&[("CACHE_MEMORY_MB", "0")]);
    get(&router, &small).await;
    get(&router, &small).await;
    assert_eq!(
        renders.load(Ordering::SeqCst),
        2,
        "CACHE_MEMORY_MB=0 disables L1"
    );
}

#[tokio::test]
async fn etag_and_cache_control() {
    let router = mermaid_ink::router(app(&[]));
    let path = format!("/svg/{}", pako(FLOW));
    let resp = get(&router, &path).await;
    assert_eq!(resp.status, 200);
    assert_eq!(resp.header("cache-control"), Some("public, max-age=604800"));
    let etag = resp.header("etag").unwrap().to_owned();

    let resp = send(&router, Method::GET, &path, &[("If-None-Match", &etag)]).await;
    assert_eq!(resp.status, 304);
    assert!(resp.body.is_empty());
    assert_eq!(resp.header("etag"), Some(etag.as_str()));
    assert_eq!(resp.header("vary"), Some("Origin"));

    // Different query, different entity.
    let other = send(
        &router,
        Method::GET,
        &format!("{path}?theme=dark"),
        &[("If-None-Match", &etag)],
    )
    .await;
    assert_eq!(other.status, 200);
    assert_ne!(other.header("etag"), Some(etag.as_str()));

    // 400s are cacheable too; a disabled max-age drops only Cache-Control.
    let bad = get(&router, &format!("{path}?width=-1")).await;
    assert_eq!(bad.status, 400);
    assert!(bad.header("etag").is_some() && bad.header("cache-control").is_some());
    let router = mermaid_ink::router(app(&[("HTTP_CACHE_MAX_AGE", "0")]));
    let resp = get(&router, &path).await;
    assert!(resp.header("cache-control").is_none() && resp.header("etag").is_some());
}

#[tokio::test]
async fn head_matches_get_headers() {
    let router = mermaid_ink::router(app(&[]));
    let path = format!("/svg/{}", pako(FLOW));
    let get_resp = get(&router, &path).await;
    let head = send(&router, Method::HEAD, &path, &[]).await;
    assert_eq!(head.status, 200);
    assert!(head.body.is_empty());
    assert_eq!(
        head.header("content-length"),
        Some(get_resp.body.len().to_string().as_str())
    );
    assert_eq!(head.header("content-type"), Some("image/svg+xml"));
}

#[tokio::test]
async fn graceful_shutdown_drains_in_flight_requests() {
    let hook: RenderHook = Arc::new(|| std::thread::sleep(Duration::from_millis(500)));
    let app = Arc::new(App::new(config(&[])).unwrap().with_render_hook(hook));
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(mermaid_ink::serve(listener, app, async {
        let _ = stopped.await;
    }));

    let path = format!("/svg/{}", pako(FLOW));
    let client = tokio::spawn(async move {
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        let req = format!("GET {path} HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n");
        s.write_all(req.as_bytes()).await.unwrap();
        let mut out = Vec::new();
        s.read_to_end(&mut out).await.unwrap();
        String::from_utf8_lossy(&out).into_owned()
    });
    tokio::time::sleep(Duration::from_millis(150)).await;
    stop.send(()).unwrap();
    let reply = client.await.unwrap();
    assert!(
        reply.starts_with("HTTP/1.1 200"),
        "{}",
        &reply[..reply.len().min(200)]
    );
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("server stops after draining")
        .unwrap()
        .unwrap();
    assert!(tokio::net::TcpStream::connect(addr).await.is_err());
}

#[tokio::test]
async fn slow_headers_are_cut_off_and_connections_are_capped() {
    let app = Arc::new(App::new(config(&[("MAX_CONNECTIONS", "1")])).unwrap());
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(mermaid_ink::serve(listener, app, std::future::pending()));
    let started = Instant::now();
    // Slowloris: an unfinished request head holds the only connection slot.
    let mut slow = tokio::net::TcpStream::connect(addr).await.unwrap();
    slow.write_all(b"GET / HTTP/1.1\r\nHost: t\r\n")
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    // A second client waits in the backlog until the slot frees up.
    let waiting = tokio::spawn(async move {
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        s.write_all(b"GET / HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut out = Vec::new();
        s.read_to_end(&mut out).await.unwrap();
        (
            String::from_utf8_lossy(&out).into_owned(),
            started.elapsed(),
        )
    });
    let mut buf = [0u8; 64];
    let n = tokio::time::timeout(Duration::from_secs(15), slow.read(&mut buf))
        .await
        .expect("slow connection is closed");
    let closed_at = started.elapsed();
    assert!(!matches!(n, Ok(n) if n > 0 && !buf.starts_with(b"HTTP/1.1 408")));
    let timeout = mermaid_ink::app::HEADER_READ_TIMEOUT;
    assert!(
        closed_at >= timeout - Duration::from_millis(500) && closed_at < timeout * 2,
        "closed after {closed_at:?}"
    );
    let (reply, served_at) = waiting.await.unwrap();
    assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
    assert!(served_at >= closed_at, "served at {served_at:?}");
}

// Remote images ------------------------------------------------------------------------------

struct ImageServer {
    port: u16,
    cert: Vec<u8>,
    hits: Arc<AtomicUsize>,
}

/// HTTPS server on 127.0.0.1 with a self-signed cert: `/red.png` is a 40x40 red PNG, anything
/// else is 404.
async fn image_server() -> ImageServer {
    use tokio_rustls::TlsAcceptor;
    use tokio_rustls::rustls::ServerConfig;
    use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

    let ck = rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_owned()]).unwrap();
    let cert = ck.cert.der().to_vec();
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(ck.signing_key.serialize_der()));
    let tls = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![CertificateDer::from(cert.clone())], key)
        .unwrap();
    let acceptor = TlsAcceptor::from(Arc::new(tls));
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let mut png = Vec::new();
    image::RgbaImage::from_pixel(40, 40, image::Rgba([255, 0, 0, 255]))
        .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
        .unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let h = hits.clone();
    tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            let (acceptor, png, hits) = (acceptor.clone(), png.clone(), h.clone());
            tokio::spawn(async move {
                let Ok(mut s) = acceptor.accept(tcp).await else {
                    return;
                };
                let mut buf = Vec::new();
                let mut chunk = [0u8; 1024];
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    match s.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                }
                hits.fetch_add(1, Ordering::SeqCst);
                let head = String::from_utf8_lossy(&buf);
                let (status, body) = if head.starts_with("GET /red.png ") {
                    ("200 OK", png)
                } else {
                    ("404 Not Found", Vec::new())
                };
                let out = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = s.write_all(out.as_bytes()).await;
                let _ = s.write_all(&body).await;
                let _ = s.shutdown().await;
            });
        }
    });
    ImageServer { port, cert, hits }
}

fn fetch_settings() -> FetchSettings {
    FetchSettings {
        timeout: Duration::from_secs(2),
        max_bytes: 1 << 20,
        max_count: 10,
        user_agent: "mermaid-ink/test".into(),
    }
}

fn image_diagram(url: &str) -> String {
    pako(&format!(
        "flowchart TD\n  A@{{ img: \"{url}\", label: \"pic\", pos: \"b\", w: 120, h: 120, constraint: \"on\" }}"
    ))
}

fn red_pixels(png: &[u8]) -> usize {
    image::load_from_memory(png)
        .unwrap()
        .to_rgba8()
        .pixels()
        .filter(|p| p[0] > 200 && p[1] < 60 && p[2] < 60 && p[3] > 200)
        .count()
}

#[tokio::test]
async fn remote_image_is_embedded_and_failures_are_not_cached() {
    let server = image_server().await;
    let fetcher = Fetcher::insecure_for_tests(
        fetch_settings(),
        IpAddr::V4(Ipv4Addr::LOCALHOST),
        &server.cert,
    );
    let renders = Arc::new(AtomicUsize::new(0));
    let r = renders.clone();
    let hook: RenderHook = Arc::new(move || {
        r.fetch_add(1, Ordering::SeqCst);
    });
    let app = App::with_fetcher(config(&[("REMOTE_IMAGES", "true")]), Some(fetcher))
        .unwrap()
        .with_render_hook(hook);
    let router = mermaid_ink::router(Arc::new(app));

    let url = format!("https://127.0.0.1:{}/red.png", server.port);
    let png = get(&router, &format!("/img/{}?type=png", image_diagram(&url))).await;
    assert_eq!(png.status, 200, "{}", png.text());
    assert!(
        png.header("etag").is_some(),
        "a complete render is cacheable"
    );
    assert!(
        red_pixels(&png.body) > 1000,
        "{} red pixels",
        red_pixels(&png.body)
    );
    let svg = get(&router, &format!("/svg/{}", image_diagram(&url))).await;
    assert!(svg.text().contains(&url), "/svg keeps the original href");

    let missing = format!(
        "/img/{}?type=png",
        image_diagram(&format!("https://127.0.0.1:{}/missing.png", server.port))
    );
    let before = renders.load(Ordering::SeqCst);
    for _ in 0..2 {
        let resp = get(&router, &missing).await;
        assert_eq!(resp.status, 200);
        assert_eq!(red_pixels(&resp.body), 0);
        assert!(resp.header("etag").is_none() && resp.header("cache-control").is_none());
    }
    assert_eq!(
        renders.load(Ordering::SeqCst),
        before + 2,
        "degraded renders are not cached"
    );

    // Requests coalesced onto one degraded render share it, and none of them gets cache headers.
    let webp = missing.replace("type=png", "type=webp");
    let waiters: Vec<_> = (0..4)
        .map(|_| {
            let (router, path) = (router.clone(), webp.clone());
            tokio::spawn(async move { get(&router, &path).await })
        })
        .collect();
    for w in waiters {
        let resp = w.await.unwrap();
        assert_eq!(resp.status, 200);
        assert!(resp.header("etag").is_none() && resp.header("cache-control").is_none());
    }
    assert_eq!(
        renders.load(Ordering::SeqCst),
        before + 3,
        "the 4 requests coalesced"
    );
}

#[tokio::test]
async fn ssrf_targets_are_not_fetched() {
    let server = image_server().await;
    let router = mermaid_ink::router(app(&[("REMOTE_IMAGES", "true")]));
    for url in [
        "https://127.0.0.1/x.png".to_owned(),
        format!("https://127.0.0.1:{}/red.png", server.port),
        "https://169.254.169.254/latest.png".to_owned(),
    ] {
        let started = Instant::now();
        let resp = get(&router, &format!("/img/{}?type=png", image_diagram(&url))).await;
        assert_eq!(resp.status, 200, "{url}: {}", resp.text());
        assert_eq!(red_pixels(&resp.body), 0);
        assert!(
            resp.header("etag").is_none(),
            "{url}: degraded, not cacheable"
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{url}: refused without connecting"
        );
    }
    assert_eq!(server.hits.load(Ordering::SeqCst), 0);
}
