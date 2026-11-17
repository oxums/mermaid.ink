//! End-to-end fetcher tests against a local self-signed TLS server.

use std::net::IpAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mink_fetch::{FetchError, FetchSettings, Fetcher};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

const LOOPBACK: &str = "127.0.0.1";

struct Reply {
    status: &'static str,
    headers: Vec<String>,
    body: Vec<u8>,
    /// Delay before the response is written.
    delay: Duration,
    /// Write the body one byte per interval.
    drip: Option<Duration>,
    /// Advertise Content-Length (otherwise the body ends at connection close).
    content_length: bool,
}

fn reply(ctype: &str, body: impl Into<Vec<u8>>) -> Reply {
    Reply {
        status: "200 OK",
        headers: vec![format!("Content-Type: {ctype}")],
        body: body.into(),
        delay: Duration::ZERO,
        drip: None,
        content_length: true,
    }
}

fn redirect(to: String) -> Reply {
    Reply {
        status: "302 Found",
        headers: vec![format!("Location: {to}")],
        ..reply("text/plain", "")
    }
}

struct Server {
    base: String,
    port: u16,
    cert: Vec<u8>,
    hits: Arc<AtomicUsize>,
    max_in_flight: Arc<AtomicUsize>,
    heads: Arc<Mutex<Vec<String>>>,
}

type Handler = dyn Fn(&str, u16) -> Reply + Send + Sync;

async fn serve(handler: impl Fn(&str, u16) -> Reply + Send + Sync + 'static) -> Server {
    let ck = rcgen::generate_simple_self_signed(vec![LOOPBACK.to_owned()]).unwrap();
    let cert = ck.cert.der().to_vec();
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(ck.signing_key.serialize_der()));
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![CertificateDer::from(cert.clone())], key)
        .unwrap();
    let acceptor = TlsAcceptor::from(Arc::new(config));
    let listener = TcpListener::bind((LOOPBACK, 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let handler: Arc<Handler> = Arc::new(handler);
    let hits = Arc::new(AtomicUsize::new(0));
    let in_flight = Arc::new(AtomicUsize::new(0));
    let max_in_flight = Arc::new(AtomicUsize::new(0));
    let heads = Arc::new(Mutex::new(Vec::new()));
    let (h, m, hd) = (hits.clone(), max_in_flight.clone(), heads.clone());
    tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else {
                return;
            };
            let (acceptor, handler) = (acceptor.clone(), handler.clone());
            let (hits, in_flight, max, heads) =
                (h.clone(), in_flight.clone(), m.clone(), hd.clone());
            tokio::spawn(async move {
                let Ok(mut tls) = acceptor.accept(tcp).await else {
                    return;
                };
                let mut buf = Vec::new();
                let mut chunk = [0u8; 1024];
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    match tls.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                }
                let head = String::from_utf8_lossy(&buf).into_owned();
                let path = head.split(' ').nth(1).unwrap_or("/").to_owned();
                heads.lock().unwrap().push(head);
                hits.fetch_add(1, Ordering::SeqCst);
                let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                max.fetch_max(now, Ordering::SeqCst);
                let r = handler(&path, port);
                tokio::time::sleep(r.delay).await;
                in_flight.fetch_sub(1, Ordering::SeqCst);
                let mut out = format!("HTTP/1.1 {}\r\nConnection: close\r\n", r.status);
                for header in &r.headers {
                    out.push_str(&format!("{header}\r\n"));
                }
                if r.content_length {
                    out.push_str(&format!("Content-Length: {}\r\n", r.body.len()));
                }
                out.push_str("\r\n");
                if tls.write_all(out.as_bytes()).await.is_err() {
                    return;
                }
                match r.drip {
                    None => {
                        let _ = tls.write_all(&r.body).await;
                    }
                    Some(every) => {
                        for b in &r.body {
                            if tls.write_all(&[*b]).await.is_err() || tls.flush().await.is_err() {
                                return;
                            }
                            tokio::time::sleep(every).await;
                        }
                    }
                }
                let _ = tls.shutdown().await;
            });
        }
    });
    Server {
        base: format!("https://{LOOPBACK}:{port}"),
        port,
        cert,
        hits,
        max_in_flight,
        heads,
    }
}

fn settings() -> FetchSettings {
    FetchSettings {
        timeout: Duration::from_millis(1500),
        max_bytes: 1024,
        max_count: 10,
        user_agent: "mermaid-ink/test".into(),
    }
}

fn fetcher(s: &Server, settings: FetchSettings) -> Fetcher {
    Fetcher::insecure_for_tests(settings, LOOPBACK.parse::<IpAddr>().unwrap(), &s.cert)
}

async fn fetch_one(f: &Fetcher, url: &str) -> Result<mink_core::FetchedImage, FetchError> {
    let mut out = f.fetch_all(&[url.to_owned()]).await;
    out.remove(url).expect("result for every url")
}

/// PNG signature + IHDR header, enough for type and size sniffing.
fn png(w: u32, h: u32) -> Vec<u8> {
    let mut v = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
    v.extend_from_slice(&w.to_be_bytes());
    v.extend_from_slice(&h.to_be_bytes());
    v.extend_from_slice(&[8, 6, 0, 0, 0, 0, 0, 0, 0]);
    v
}

#[tokio::test]
async fn fetches_png_with_natural_size() {
    let s = serve(|_, _| reply("image/png; charset=binary", png(30, 20))).await;
    let img = fetch_one(&fetcher(&s, settings()), &format!("{}/a.png", s.base))
        .await
        .unwrap();
    assert_eq!((img.mime, img.width, img.height), ("image/png", 30, 20));
    assert_eq!(img.bytes.as_ref(), png(30, 20).as_slice());
    let head = s.heads.lock().unwrap()[0].to_ascii_lowercase();
    assert!(head.contains("user-agent: mermaid-ink/test\r\n"), "{head}");
}

#[tokio::test]
async fn refuses_svg() {
    let svg = r#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"/>"#;
    let s = serve(move |_, _| reply("image/svg+xml", svg)).await;
    let res = fetch_one(&fetcher(&s, settings()), &format!("{}/a.svg", s.base)).await;
    assert!(matches!(res, Err(FetchError::BadContent(_))), "{res:?}");
}

#[tokio::test]
async fn refuses_wrong_magic_bytes() {
    let s = serve(|path, _| match path {
        "/text" => reply("image/png", "definitely not a png file"),
        _ => reply("image/png", r#"<svg xmlns="http://www.w3.org/2000/svg"/>"#),
    })
    .await;
    let f = fetcher(&s, settings());
    for path in ["/text", "/svg"] {
        let res = fetch_one(&f, &format!("{}{path}", s.base)).await;
        assert!(
            matches!(res, Err(FetchError::BadContent(_))),
            "{path}: {res:?}"
        );
    }
}

#[tokio::test]
async fn refuses_wrong_content_type() {
    let s = serve(|_, _| reply("text/html", png(1, 1))).await;
    let res = fetch_one(&fetcher(&s, settings()), &format!("{}/a", s.base)).await;
    assert!(matches!(res, Err(FetchError::BadContent(_))), "{res:?}");
}

#[tokio::test]
async fn caps_body_size() {
    let s = serve(|path, _| {
        let mut body = png(1, 1);
        body.resize(4096, 0);
        Reply {
            content_length: path == "/declared",
            ..reply("image/png", body)
        }
    })
    .await;
    let f = fetcher(&s, settings());
    for path in ["/declared", "/streamed"] {
        let res = fetch_one(&f, &format!("{}{path}", s.base)).await;
        assert!(matches!(res, Err(FetchError::TooLarge)), "{path}: {res:?}");
    }
}

#[tokio::test]
async fn slow_drip_times_out_within_budget() {
    let s = serve(|_, _| Reply {
        drip: Some(Duration::from_millis(200)),
        ..reply("image/png", png(1, 1))
    })
    .await;
    let f = fetcher(&s, settings());
    let start = Instant::now();
    let res = fetch_one(&f, &format!("{}/slow", s.base)).await;
    let took = start.elapsed();
    assert!(matches!(res, Err(FetchError::Timeout)), "{res:?}");
    assert!(took < Duration::from_millis(2500), "took {took:?}");
}

#[tokio::test]
async fn redirects_are_revalidated() {
    let s = serve(|path, port| match path {
        "/ok" => reply("image/png", png(2, 2)),
        "/hop" => redirect("/ok".into()),
        "/to-loopback" => redirect(format!("https://127.0.0.2:{port}/ok")),
        "/to-v6-loopback" => redirect(format!("https://[::1]:{port}/ok")),
        "/to-metadata" => redirect("https://169.254.169.254/latest/meta-data/".into()),
        "/to-http" => redirect(format!("http://127.0.0.1:{port}/ok")),
        p => {
            // /loopN redirects N more times before landing on /ok
            let n: u32 = p.trim_start_matches("/loop").parse().unwrap();
            redirect(if n == 0 {
                "/ok".into()
            } else {
                format!("/loop{}", n - 1)
            })
        }
    })
    .await;
    let f = fetcher(&s, settings());
    let f = &f;
    let get = |p: &str| {
        let url = format!("{}{p}", s.base);
        async move { fetch_one(f, &url).await }
    };
    assert!(get("/hop").await.is_ok());
    assert!(get("/loop2").await.is_ok(), "3 redirects are allowed");
    for p in [
        "/to-loopback",
        "/to-v6-loopback",
        "/to-metadata",
        "/to-http",
        "/loop3",
    ] {
        let res = get(p).await;
        assert!(
            matches!(res, Err(FetchError::Disallowed(_))),
            "{p}: {res:?}"
        );
    }
}

#[tokio::test]
async fn rejects_disallowed_urls_without_connecting() {
    let f = Fetcher::new(settings()).unwrap();
    for url in [
        "http://example.com/a.png",
        "https://example.com:8443/a.png",
        "https://10.0.0.1/a.png",
        "https://127.0.0.1/a.png",
        "https://2130706433/a.png",
        "https://[::ffff:127.0.0.1]/a.png",
        "https://169.254.169.254/latest/meta-data/",
        "not a url",
    ] {
        let res = fetch_one(&f, url).await;
        assert!(
            matches!(res, Err(FetchError::Disallowed(_))),
            "{url}: {res:?}"
        );
    }
    // The production policy also refuses loopback with a self-signed server actually listening.
    let s = serve(|_, _| reply("image/png", png(1, 1))).await;
    let res = fetch_one(&f, &format!("{}/a.png", s.base)).await;
    assert!(matches!(res, Err(FetchError::Disallowed(_))), "{res:?}");
    assert_eq!(s.hits.load(Ordering::SeqCst), 0);
    // Even the test fetcher only relaxes the one loopback address it was given.
    let res = fetch_one(
        &fetcher(&s, settings()),
        &format!("https://127.0.0.2:{}/a.png", s.port),
    )
    .await;
    assert!(matches!(res, Err(FetchError::Disallowed(_))), "{res:?}");
}

#[tokio::test]
async fn coalesces_and_caches_results() {
    let s = serve(|path, _| match path {
        "/missing" => Reply {
            status: "404 Not Found",
            ..reply("text/plain", "")
        },
        _ => Reply {
            delay: Duration::from_millis(300),
            ..reply("image/gif", b"GIF89a\x05\x00\x07\x00\x00\x00\x00;".to_vec())
        },
    })
    .await;
    let f = Arc::new(fetcher(&s, settings()));
    let url = format!("{}/a.gif", s.base);
    let tasks: Vec<_> = (0..10)
        .map(|_| {
            let (f, url) = (f.clone(), url.clone());
            tokio::spawn(async move { fetch_one(&f, &url).await })
        })
        .collect();
    for t in tasks {
        let img = t.await.unwrap().unwrap();
        assert_eq!((img.mime, img.width, img.height), ("image/gif", 5, 7));
    }
    assert!(fetch_one(&f, &url).await.is_ok());
    assert_eq!(
        s.hits.load(Ordering::SeqCst),
        1,
        "one upstream hit for N identical urls"
    );

    let missing = format!("{}/missing", s.base);
    assert!(matches!(
        fetch_one(&f, &missing).await,
        Err(FetchError::Http(_))
    ));
    assert!(matches!(
        fetch_one(&f, &missing).await,
        Err(FetchError::Http(_))
    ));
    assert_eq!(s.hits.load(Ordering::SeqCst), 2, "failures are cached too");
}

#[tokio::test]
async fn enforces_max_count_and_per_host_cap() {
    let s = serve(|_, _| Reply {
        delay: Duration::from_millis(200),
        ..reply("image/png", png(1, 1))
    })
    .await;
    // max_count 9: the request's fair share is 5, so the per-host cap of 4 is what binds.
    let f = fetcher(
        &s,
        FetchSettings {
            max_count: 9,
            ..settings()
        },
    );
    let mut urls: Vec<String> = (0..10).map(|i| format!("{}/{i}.png", s.base)).collect();
    urls.insert(1, urls[0].clone()); // duplicates count once
    let out = f.fetch_all(&urls).await;
    assert_eq!(out.len(), 10);
    let ok = out.values().filter(|r| r.is_ok()).count();
    let refused = out
        .values()
        .filter(|r| matches!(r, Err(FetchError::Disallowed(_))))
        .count();
    assert_eq!((ok, refused), (9, 1));
    assert!(out[&format!("{}/9.png", s.base)].is_err());
    assert_eq!(s.hits.load(Ordering::SeqCst), 9);
    assert_eq!(s.max_in_flight.load(Ordering::SeqCst), 4, "per-host cap");
}

#[tokio::test]
async fn one_request_holds_at_most_half_of_max_count() {
    let s = serve(|_, _| Reply {
        delay: Duration::from_millis(200),
        ..reply("image/png", png(1, 1))
    })
    .await;
    let f = fetcher(
        &s,
        FetchSettings {
            max_count: 5,
            ..settings()
        },
    );
    let urls: Vec<String> = (0..5).map(|i| format!("{}/{i}.png", s.base)).collect();
    let out = f.fetch_all(&urls).await;
    assert!(out.values().all(Result::is_ok), "{out:?}");
    assert_eq!(
        s.max_in_flight.load(Ordering::SeqCst),
        3,
        "fair share: ceil(5 / 2)"
    );
}
