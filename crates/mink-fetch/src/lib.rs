//! Hardened https image fetcher (SSRF-safe, tight timeouts). See docs/PLAN.md "Remote image fetcher".

use std::collections::HashMap;
use std::error::Error as StdError;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use hickory_resolver::TokioResolver;
use mink_core::FetchedImage;
use moka::future::Cache;
use reqwest::Url;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use tokio::sync::Semaphore;
use tokio::time::{Instant, timeout, timeout_at};

pub struct FetchSettings {
    /// Total deadline per image (REMOTE_IMAGE_TIMEOUT_MS).
    pub timeout: Duration,
    /// REMOTE_IMAGE_MAX_BYTES
    pub max_bytes: usize,
    /// REMOTE_IMAGE_MAX_COUNT. One `fetch_all` call holds at most half of it (rounded up) in
    /// fetch permits at a time, so a single diagram cannot occupy the global pool.
    pub max_count: usize,
    /// Sent as User-Agent, e.g. "mermaid-ink/18.0.0".
    pub user_agent: String,
}

#[derive(Clone, Debug)]
pub enum FetchError {
    Disallowed(String),
    Timeout,
    TooLarge,
    BadContent(String),
    Http(String),
}

const DNS_TIMEOUT: Duration = Duration::from_secs(1);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(1);
const READ_TIMEOUT: Duration = Duration::from_secs(1);
const MAX_REDIRECTS: usize = 3;
const GLOBAL_CONCURRENCY: usize = 32;
const PER_HOST_CONCURRENCY: usize = 4;
const CACHE_TTL: Duration = Duration::from_secs(60);
const CACHE_MAX_BYTES: u64 = 64 * 1024 * 1024;
/// Same as merman-export's DEFAULT_MAX_DECODED_IMAGE_PIXELS: decompression-bomb guard.
const MAX_PIXELS: u64 = 16 * 1024 * 1024;
const ALLOWED_MIMES: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];

/// IP policy: production allows only public addresses on port 443. The test policy
/// (feature `insecure-test-loopback`) additionally allows one loopback address on any port.
#[derive(Clone, Copy)]
struct Policy {
    test_loopback: Option<IpAddr>,
}

impl Policy {
    fn ip_ok(self, ip: IpAddr) -> bool {
        self.test_loopback == Some(ip) || is_public(ip)
    }

    fn check_url(self, url: &Url) -> Result<(), Denied> {
        if url.scheme() != "https" {
            return Err(Denied(format!("scheme {} not allowed", url.scheme())));
        }
        if self.test_loopback.is_none() && url.port_or_known_default() != Some(443) {
            return Err(Denied("port not allowed".into()));
        }
        let host = url
            .host_str()
            .ok_or_else(|| Denied("missing host".into()))?;
        // The url crate canonicalises every IPv4 spelling (127.1, 0x7f.0.0.1, 2130706433) to dotted form.
        let Ok(ip) = host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<IpAddr>()
        else {
            return Ok(()); // a domain name: checked by SafeResolver
        };
        if self.ip_ok(ip) {
            Ok(())
        } else {
            Err(Denied(format!("address {ip} not allowed")))
        }
    }

    /// Whole host is rejected if any address is non-public; otherwise exactly these are dialed.
    fn check_addrs(self, ips: impl IntoIterator<Item = IpAddr>) -> Result<Vec<SocketAddr>, Denied> {
        let addrs: Vec<SocketAddr> = ips.into_iter().map(|ip| SocketAddr::new(ip, 0)).collect();
        if addrs.is_empty() {
            return Err(Denied("no addresses".into()));
        }
        match addrs.iter().find(|a| !self.ip_ok(a.ip())) {
            Some(bad) => Err(Denied(format!(
                "resolves to {} which is not allowed",
                bad.ip()
            ))),
            None => Ok(addrs),
        }
    }
}

/// SSRF policy violation; travels through reqwest's error chain so it can be told apart.
#[derive(Debug)]
struct Denied(String);

impl fmt::Display for Denied {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl StdError for Denied {}

/// True when `ip` is a globally routable unicast address we may connect to.
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => is_public_v6(v6),
    }
}

fn in_v4(ip: u32, net: [u8; 4], len: u32) -> bool {
    let mask = u32::MAX << (32 - len);
    ip & mask == u32::from_be_bytes(net) & mask
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    const DENY: [([u8; 4], u32); 14] = [
        ([0, 0, 0, 0], 8),       // "this network", unspecified
        ([10, 0, 0, 0], 8),      // private
        ([100, 64, 0, 0], 10),   // CGNAT
        ([127, 0, 0, 0], 8),     // loopback
        ([169, 254, 0, 0], 16),  // link-local, cloud metadata
        ([172, 16, 0, 0], 12),   // private
        ([192, 0, 0, 0], 24),    // IETF protocol assignments
        ([192, 0, 2, 0], 24),    // documentation
        ([192, 168, 0, 0], 16),  // private
        ([198, 18, 0, 0], 15),   // benchmarking
        ([198, 51, 100, 0], 24), // documentation
        ([203, 0, 113, 0], 24),  // documentation
        ([224, 0, 0, 0], 4),     // multicast
        ([240, 0, 0, 0], 4),     // reserved, includes broadcast
    ];
    let ip = u32::from(ip);
    !DENY.iter().any(|&(net, len)| in_v4(ip, net, len))
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    let s = ip.segments();
    let v4_at = |hi: usize| Ipv4Addr::from((u32::from(s[hi]) << 16) | u32::from(s[hi + 1]));
    // ::ffff:a.b.c.d: an IPv4 socket in disguise.
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_public_v4(v4);
    }
    // NAT64 well-known prefix 64:ff9b::/96 embeds the v4 target in the low 32 bits.
    if s[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
        return is_public_v4(v4_at(6));
    }
    // Only global unicast 2000::/3; this excludes ::, ::1, ::/96 compat, 64:ff9b:1::/48
    // (local-use NAT64), fc00::/7, fe80::/10, fec0::/10, ff00::/8 and all other reserved space.
    if s[0] & 0xe000 != 0x2000 {
        return false;
    }
    match s[0] {
        // 6to4: 2002:AABB:CCDD::/48 wraps a.b.c.d
        0x2002 => is_public_v4(v4_at(1)),
        // Teredo 2001:0::/32: server v4 in bits 32..64, client v4 obfuscated (inverted) in the low 32
        0x2001 if s[1] == 0 => is_public_v4(v4_at(2)) && is_public_v4(!v4_at(6)),
        // documentation 2001:db8::/32, benchmarking 2001:2::/48
        0x2001 if s[1] == 0xdb8 || (s[1] == 2 && s[2] == 0) => false,
        // documentation 3fff::/20
        0x3fff if s[1] & 0xf000 == 0 => false,
        _ => true,
    }
}

struct SafeResolver {
    dns: TokioResolver,
    policy: Policy,
}

impl Resolve for SafeResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let dns = self.dns.clone();
        let policy = self.policy;
        Box::pin(async move {
            let lookup = timeout(DNS_TIMEOUT, dns.lookup_ip(name.as_str()))
                .await
                .map_err(|_| "dns lookup timed out")??;
            let addrs = policy.check_addrs(lookup.iter())?;
            Ok(Box::new(addrs.into_iter()) as Addrs)
        })
    }
}

struct Inner {
    settings: FetchSettings,
    policy: Policy,
    client: reqwest::Client,
    cache: Cache<String, Result<FetchedImage, FetchError>>,
    global: Semaphore,
    hosts: Cache<String, Arc<Semaphore>>,
}

pub struct Fetcher {
    inner: Arc<Inner>,
}

impl Fetcher {
    /// Fails when the system DNS configuration cannot be read.
    pub fn new(settings: FetchSettings) -> Result<Self, String> {
        Self::build(
            settings,
            Policy {
                test_loopback: None,
            },
            None,
        )
    }

    /// Test-only fetcher that may also reach `loopback` on any port and trusts only `root_cert_der`.
    #[cfg(feature = "insecure-test-loopback")]
    pub fn insecure_for_tests(
        settings: FetchSettings,
        loopback: IpAddr,
        root_cert_der: &[u8],
    ) -> Self {
        assert!(
            loopback.is_loopback(),
            "test fetcher may only relax loopback"
        );
        let cert = reqwest::Certificate::from_der(root_cert_der).expect("test root certificate");
        Self::build(
            settings,
            Policy {
                test_loopback: Some(loopback),
            },
            Some(cert),
        )
        .expect("test fetcher")
    }

    fn build(
        settings: FetchSettings,
        policy: Policy,
        test_root: Option<reqwest::Certificate>,
    ) -> Result<Self, String> {
        let dns = TokioResolver::builder_tokio()
            .and_then(|b| b.build())
            .map_err(|e| format!("system DNS configuration: {e}"))?;
        let mut builder = reqwest::Client::builder()
            .no_proxy()
            .https_only(true)
            .retry(reqwest::retry::never())
            .dns_resolver(Arc::new(SafeResolver { dns, policy }))
            .redirect(reqwest::redirect::Policy::custom(move |attempt| {
                if attempt.previous().len() > MAX_REDIRECTS {
                    return attempt.error(Denied("too many redirects".into()));
                }
                match policy.check_url(attempt.url()) {
                    Ok(()) => attempt.follow(),
                    Err(e) => attempt.error(e),
                }
            }))
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(READ_TIMEOUT)
            .pool_max_idle_per_host(2)
            .user_agent(settings.user_agent.as_str());
        if let Some(cert) = test_root {
            builder = builder.tls_certs_only([cert]);
        }
        let client = builder.build().map_err(|e| format!("https client: {e}"))?;
        let cache = Cache::builder()
            .time_to_live(CACHE_TTL)
            .max_capacity(CACHE_MAX_BYTES)
            .weigher(|url: &String, v: &Result<FetchedImage, FetchError>| {
                let body = v.as_ref().map_or(0, |img| img.bytes.len());
                u32::try_from(url.len() + body + 64).unwrap_or(u32::MAX)
            })
            .build();
        let hosts = Cache::builder().time_to_idle(CACHE_TTL).build();
        let inner = Inner {
            settings,
            policy,
            client,
            cache,
            global: Semaphore::new(GLOBAL_CONCURRENCY),
            hosts,
        };
        Ok(Self {
            inner: Arc::new(inner),
        })
    }

    /// Fetches up to `max_count` URLs concurrently (per-host and global caps, 60 s result cache).
    /// URLs beyond `max_count` map to `FetchError::Disallowed`.
    pub async fn fetch_all(
        &self,
        urls: &[String],
    ) -> HashMap<String, Result<FetchedImage, FetchError>> {
        let mut out = HashMap::new();
        let mut tasks = tokio::task::JoinSet::new();
        // One timeout round for parallel fetches plus one for per-host queueing.
        let deadline = Instant::now() + self.inner.settings.timeout * 2;
        let share = Arc::new(Semaphore::new(
            self.inner.settings.max_count.div_ceil(2).max(1),
        ));
        for url in urls {
            if out.contains_key(url) {
                continue;
            }
            if tasks.len() >= self.inner.settings.max_count {
                out.insert(
                    url.clone(),
                    Err(FetchError::Disallowed("too many images".into())),
                );
                continue;
            }
            out.insert(url.clone(), Err(FetchError::Timeout));
            let (inner, url, share) = (self.inner.clone(), url.clone(), share.clone());
            tasks.spawn(async move {
                let res = timeout_at(deadline, async {
                    let _share = share.acquire().await;
                    inner.get(&url).await
                })
                .await;
                (url, res.unwrap_or(Err(FetchError::Timeout)))
            });
        }
        while let Some(joined) = tasks.join_next().await {
            match joined {
                Ok((url, res)) => {
                    out.insert(url, res);
                }
                // A panicked task leaves its pre-inserted Timeout in place.
                Err(_) => continue,
            }
        }
        out
    }
}

impl Inner {
    /// Cached and coalesced: concurrent identical URLs cause one upstream fetch; results
    /// (successes and failures) live for 60 s.
    async fn get(&self, url: &str) -> Result<FetchedImage, FetchError> {
        self.cache
            .get_with_by_ref(url, self.fetch_limited(url))
            .await
    }

    async fn fetch_limited(&self, raw: &str) -> Result<FetchedImage, FetchError> {
        let url =
            Url::parse(raw).map_err(|e| FetchError::Disallowed(format!("invalid url: {e}")))?;
        self.policy
            .check_url(&url)
            .map_err(|e| FetchError::Disallowed(e.0))?;
        let host_sem = self
            .hosts
            .get_with(host_key(&url), async {
                Arc::new(Semaphore::new(PER_HOST_CONCURRENCY))
            })
            .await;
        let closed = |_| FetchError::Http("fetcher closed".into());
        let _host = host_sem.acquire().await.map_err(closed)?;
        let _global = self.global.acquire().await.map_err(closed)?;
        timeout(self.settings.timeout, self.fetch(url))
            .await
            .unwrap_or(Err(FetchError::Timeout))
    }

    async fn fetch(&self, url: Url) -> Result<FetchedImage, FetchError> {
        let mut resp = self.client.get(url).send().await.map_err(map_reqwest)?;
        let status = resp.status();
        if !status.is_success() {
            return Err(FetchError::Http(format!("upstream status {status}")));
        }
        let ctype = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(';').next())
            .map(|v| v.trim().to_ascii_lowercase())
            .unwrap_or_default();
        if !ALLOWED_MIMES.contains(&ctype.as_str()) {
            return Err(FetchError::BadContent(format!(
                "content-type {ctype:?} not allowed"
            )));
        }
        let max = self.settings.max_bytes;
        if resp.content_length().is_some_and(|len| len > max as u64) {
            return Err(FetchError::TooLarge);
        }
        let mut body = Vec::new();
        while let Some(chunk) = resp.chunk().await.map_err(map_reqwest)? {
            if body.len() + chunk.len() > max {
                return Err(FetchError::TooLarge);
            }
            body.extend_from_slice(&chunk);
        }
        sniff(body.into())
    }
}

/// Per-host concurrency key: the last two DNS labels, so wildcard subdomains share one cap.
/// shortcut: no public suffix list, so every `*.co.uk` (or `*.github.io`) site shares one cap;
/// upgrade to a PSL lookup if that starves legitimate hosts. IP literals are their own key.
fn host_key(url: &Url) -> String {
    let host = url.host_str().unwrap_or_default();
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    if bare.parse::<IpAddr>().is_ok() {
        return host.to_owned();
    }
    let domain = host.trim_end_matches('.');
    match domain.rsplitn(3, '.').collect::<Vec<_>>()[..] {
        [tld, name, ..] => format!("{name}.{tld}"),
        _ => domain.to_owned(),
    }
}

/// Magic-byte check (png/jpeg/gif/webp only) plus natural size from the image header.
fn sniff(bytes: bytes::Bytes) -> Result<FetchedImage, FetchError> {
    use imagesize::ImageType;
    let mime = match imagesize::image_type(&bytes) {
        Ok(ImageType::Png) => "image/png",
        Ok(ImageType::Jpeg) => "image/jpeg",
        Ok(ImageType::Gif) => "image/gif",
        Ok(ImageType::Webp) => "image/webp",
        _ => {
            return Err(FetchError::BadContent(
                "not a png/jpeg/gif/webp image".into(),
            ));
        }
    };
    let size = imagesize::blob_size(&bytes)
        .map_err(|e| FetchError::BadContent(format!("unreadable image header: {e}")))?;
    let (width, height) = match (u32::try_from(size.width), u32::try_from(size.height)) {
        (Ok(w), Ok(h)) if w > 0 && h > 0 && u64::from(w) * u64::from(h) <= MAX_PIXELS => (w, h),
        _ => {
            return Err(FetchError::BadContent(format!(
                "bad dimensions {}x{}",
                size.width, size.height
            )));
        }
    };
    Ok(FetchedImage {
        bytes,
        mime,
        width,
        height,
    })
}

fn map_reqwest(e: reqwest::Error) -> FetchError {
    let mut source: Option<&(dyn StdError + 'static)> = Some(&e);
    while let Some(err) = source {
        if let Some(denied) = err.downcast_ref::<Denied>() {
            return FetchError::Disallowed(denied.0.clone());
        }
        source = err.source();
    }
    if e.is_timeout() {
        FetchError::Timeout
    } else {
        FetchError::Http(e.without_url().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_public_table() {
        let rows: &[(&str, bool)] = &[
            ("8.8.8.8", true),
            ("1.1.1.1", true),
            ("93.184.216.34", true),
            ("100.63.255.255", true),
            ("100.128.0.0", true),
            ("172.15.255.255", true),
            ("172.32.0.0", true),
            ("198.17.255.255", true),
            ("198.20.0.0", true),
            ("223.255.255.255", true),
            ("0.0.0.0", false),
            ("0.1.2.3", false),
            ("10.0.0.1", false),
            ("100.64.0.1", false),
            ("100.127.255.255", false),
            ("127.0.0.1", false),
            ("127.255.255.254", false),
            ("169.254.169.254", false),
            ("172.16.0.1", false),
            ("172.31.255.255", false),
            ("192.0.0.1", false),
            ("192.0.2.1", false),
            ("192.168.1.1", false),
            ("198.18.0.1", false),
            ("198.19.255.255", false),
            ("198.51.100.7", false),
            ("203.0.113.9", false),
            ("224.0.0.1", false),
            ("239.255.255.250", false),
            ("240.0.0.1", false),
            ("255.255.255.255", false),
            // IPv6
            ("2606:4700:4700::1111", true),
            ("2a00:1450:4001::200e", true),
            ("::", false),
            ("::1", false),
            ("::127.0.0.1", false), // deprecated v4-compatible
            ("::8.8.8.8", false),
            ("fc00::1", false),
            ("fd00:ec2::254", false), // AWS metadata
            ("fe80::1", false),
            ("febf::1", false),
            ("fec0::1", false),
            ("ff02::1", false),
            ("2001:db8::1", false),
            ("2001:2::1", false),
            ("3fff::1", false),
            ("4000::1", false),
            // IPv4-mapped
            ("::ffff:127.0.0.1", false),
            ("::ffff:7f00:1", false),
            ("::ffff:169.254.169.254", false),
            ("::ffff:10.1.2.3", false),
            ("::ffff:8.8.8.8", true),
            // NAT64
            ("64:ff9b::7f00:1", false),
            ("64:ff9b::a9fe:a9fe", false),
            ("64:ff9b::8.8.8.8", true),
            ("64:ff9b:1::808:808", false), // local-use NAT64 is never allowed
            // 6to4
            ("2002:7f00:1::1", false),
            ("2002:a9fe:a9fe::", false),
            ("2002:c0a8:101::1", false),
            ("2002:808:808::1", true),
            // Teredo: server 65.54.227.120; client obfuscated in low 32 bits
            ("2001:0:4136:e378:8000:63bf:f7f7:f7f7", true), // client 8.8.8.8
            ("2001:0:4136:e378:8000:63bf:80ff:fffe", false), // client 127.0.0.1
            ("2001:0:7f00:1:8000:63bf:f7f7:f7f7", false),   // server 127.0.0.1
            ("2001:0:a00:1::", false),                      // server 10.0.0.1
        ];
        assert!(rows.len() >= 40);
        for (ip, want) in rows {
            let parsed: IpAddr = ip.parse().unwrap();
            assert_eq!(is_public(parsed), *want, "{ip}");
        }
    }

    #[test]
    fn host_key_is_the_last_two_labels() {
        for (url, want) in [
            ("https://example.com/a.png", "example.com"),
            ("https://a.b.example.com/a.png", "example.com"),
            ("https://x.example.com./a.png", "example.com"),
            ("https://EXAMPLE.com/a.png", "example.com"),
            ("https://localhost/a.png", "localhost"),
            ("https://8.8.8.8/a.png", "8.8.8.8"),
            ("https://[2606:4700::1111]/a.png", "[2606:4700::1111]"),
        ] {
            assert_eq!(host_key(&Url::parse(url).unwrap()), want, "{url}");
        }
    }

    fn prod() -> Policy {
        Policy {
            test_loopback: None,
        }
    }

    #[test]
    fn url_policy() {
        let ok = [
            "https://example.com/a.png",
            "https://example.com:443/a.png",
            "https://8.8.8.8/x",
        ];
        for u in ok {
            assert!(prod().check_url(&Url::parse(u).unwrap()).is_ok(), "{u}");
        }
        let bad = [
            "http://example.com/a.png",
            "ftp://example.com/a.png",
            "https://example.com:8443/a.png",
            "https://127.0.0.1/",
            "https://127.1/",
            "https://2130706433/",
            "https://0x7f.0.0.1/",
            "https://017700000001/",
            "https://[::1]/",
            "https://[::ffff:127.0.0.1]/",
            "https://[64:ff9b::a9fe:a9fe]/",
            "https://169.254.169.254/latest/meta-data/",
            "https://10.0.0.1/",
        ];
        for u in bad {
            assert!(prod().check_url(&Url::parse(u).unwrap()).is_err(), "{u}");
        }
    }

    #[test]
    fn resolver_rejects_whole_host_if_any_address_is_private() {
        let public: IpAddr = "8.8.8.8".parse().unwrap();
        let private: IpAddr = "10.0.0.1".parse().unwrap();
        assert!(prod().check_addrs([public, private]).is_err());
        assert!(prod().check_addrs([]).is_err());
        let addrs = prod().check_addrs([public]).unwrap();
        assert_eq!(addrs, vec![SocketAddr::new(public, 0)]);
    }

    #[test]
    fn sniff_checks_magic_and_pixels() {
        assert!(matches!(
            sniff(png(3, 2)),
            Ok(FetchedImage {
                mime: "image/png",
                width: 3,
                height: 2,
                ..
            })
        ));
        assert!(matches!(
            sniff(bytes::Bytes::from_static(b"<svg xmlns='x'/>")),
            Err(FetchError::BadContent(_))
        ));
        assert!(matches!(
            sniff(png(100_000, 100_000)),
            Err(FetchError::BadContent(_))
        ));
        assert!(matches!(sniff(png(0, 5)), Err(FetchError::BadContent(_))));
    }

    /// PNG signature + IHDR header, enough for type and size sniffing.
    pub(crate) fn png(w: u32, h: u32) -> bytes::Bytes {
        let mut v = b"\x89PNG\r\n\x1a\n\0\0\0\x0dIHDR".to_vec();
        v.extend_from_slice(&w.to_be_bytes());
        v.extend_from_slice(&h.to_be_bytes());
        v.extend_from_slice(&[8, 6, 0, 0, 0, 0, 0, 0, 0]);
        v.into()
    }
}
