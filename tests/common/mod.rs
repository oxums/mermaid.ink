//! Shared helpers for the server-level tests: build the app in-process and send requests.
#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Method, Request};
use bytes::Bytes;
use http_body_util::BodyExt;
use mermaid_ink::{App, Config};
use tower::ServiceExt;

pub fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Test config: fonts from FONT_DIR or ./fonts (tools/fonts/fetch.sh), remote images off,
/// then `vars` on top.
pub fn config(vars: &[(&str, &str)]) -> Config {
    let font_dir = std::env::var("FONT_DIR")
        .unwrap_or_else(|_| repo().join("fonts").to_string_lossy().into_owned());
    Config::from_lookup(|k| {
        vars.iter()
            .rev()
            .find(|(n, _)| *n == k)
            .map(|(_, v)| v.to_string())
            .or_else(|| match k {
                "FONT_DIR" => Some(font_dir.clone()),
                "REMOTE_IMAGES" => Some("false".into()),
                _ => None,
            })
    })
    .expect("test config")
}

pub fn app(vars: &[(&str, &str)]) -> Arc<App> {
    Arc::new(App::new(config(vars)).expect("app"))
}

pub struct Resp {
    pub status: u16,
    pub headers: HeaderMap,
    pub body: Bytes,
}

impl Resp {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }

    /// Media type without parameters, like supertest's `resp.type`.
    pub fn mime(&self) -> &str {
        self.header("content-type")
            .map_or("", |v| v.split(';').next().unwrap_or("").trim())
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

pub async fn send(router: &Router, method: Method, uri: &str, headers: &[(&str, &str)]) -> Resp {
    let mut req = Request::builder().method(method).uri(uri);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let res = router
        .clone()
        .oneshot(req.body(Body::empty()).expect("request"))
        .await
        .expect("infallible");
    let status = res.status().as_u16();
    let headers = res.headers().clone();
    let body = res.into_body().collect().await.expect("body").to_bytes();
    Resp {
        status,
        headers,
        body,
    }
}

pub async fn get(router: &Router, uri: &str) -> Resp {
    send(router, Method::GET, uri, &[]).await
}

/// `pako:` form of `{code, mermaid: {}}` (zlib + base64url without padding), like the live editor.
pub fn pako(code: &str) -> String {
    use base64::Engine as _;
    use std::io::Write as _;
    let json = serde_json::json!({ "code": code, "mermaid": {} }).to_string();
    let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
    z.write_all(json.as_bytes()).expect("deflate");
    let raw = z.finish().expect("deflate");
    format!(
        "pako:{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw)
    )
}
