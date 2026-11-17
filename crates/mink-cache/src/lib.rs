//! Response cache: in-memory byte-bounded LRU (moka) + optional Redis L2.
//! See docs/PLAN.md "Caching".

use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use moka::policy::EvictionPolicy;
use redis::AsyncCommands;
use redis::aio::{ConnectionManager, ConnectionManagerConfig};

const KEY_PREFIX: &str = "mink:v1:";
const CONNECT_RETRY: Duration = Duration::from_secs(5);
const WARN_EVERY_SECS: u64 = 60;

pub struct CacheSettings {
    /// CACHE_MEMORY_MB * 1024 * 1024; 0 disables the in-memory LRU.
    pub memory_bytes: u64,
    /// REDIS_URL; None disables L2.
    pub redis_url: Option<String>,
    pub redis_ttl: Duration,
    pub redis_timeout: Duration,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cached {
    pub status: u16,
    pub mime: String,
    pub body: Bytes,
    /// Only cacheable responses are stored (L1 and L2). It travels with the response so every
    /// coalesced waiter knows whether it may be cached downstream (ETag/Cache-Control).
    pub cacheable: bool,
}

pub struct Cache {
    l1: Option<moka::future::Cache<String, Cached>>,
    l2: Option<Redis>,
}

struct Redis {
    /// Set by the background connect task; until then every L2 lookup is a miss.
    conn: Arc<OnceLock<ConnectionManager>>,
    ttl_secs: u64,
    timeout: Duration,
}

impl Cache {
    /// Never blocks on Redis: it connects in the background.
    /// With `redis_url` set, it must be called inside a Tokio runtime.
    pub fn new(settings: CacheSettings) -> Self {
        let l1 = (settings.memory_bytes > 0).then(|| {
            moka::future::Cache::builder()
                .eviction_policy(EvictionPolicy::lru())
                .weigher(|k: &String, v: &Cached| {
                    u32::try_from(k.len() + v.body.len() + v.mime.len()).unwrap_or(u32::MAX)
                })
                .max_capacity(settings.memory_bytes)
                .build()
        });
        let l2 = settings
            .redis_url
            .map(|url| Redis::spawn(url, settings.redis_ttl, settings.redis_timeout));
        Self { l1, l2 }
    }

    /// Returns the cached response for `key` (L1, then L2), or runs `render` once for all
    /// concurrent callers of the same key. Only `cacheable` responses are stored in L1 and L2.
    pub async fn get_or_render<F, Fut>(&self, key: &str, render: F) -> Cached
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Cached>,
    {
        let Some(l1) = &self.l1 else {
            return self.load(key, render).await;
        };
        // Err carries a non-cacheable result: moka hands it to every coalesced waiter but does not store it.
        l1.try_get_with(key.to_owned(), async {
            let c = self.load(key, render).await;
            if c.cacheable { Ok(c) } else { Err(c) }
        })
        .await
        .unwrap_or_else(Arc::unwrap_or_clone)
    }

    /// L2, then render. Writes cacheable renders to L2.
    async fn load<F, Fut>(&self, key: &str, render: F) -> Cached
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Cached>,
    {
        if let Some(l2) = &self.l2
            && let Some(hit) = l2.get(key).await
        {
            return hit;
        }
        let c = render().await;
        if c.cacheable
            && let Some(l2) = &self.l2
        {
            l2.set(key, &c);
        }
        c
    }
}

impl Redis {
    fn spawn(url: String, ttl: Duration, timeout: Duration) -> Self {
        // redis-rs builds its rustls config from the process-wide provider and panics without one
        // (rediss://). Err means one is installed already (main.rs does the same).
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let conn = Arc::new(OnceLock::new());
        let slot = conn.clone();
        tokio::spawn(async move {
            let client = match redis::Client::open(url) {
                Ok(c) => c,
                Err(e) => return tracing::warn!("invalid REDIS_URL, L2 cache disabled: {e}"),
            };
            let config = ConnectionManagerConfig::new()
                .set_connection_timeout(Some(Duration::from_millis(500)))
                .set_response_timeout(Some(timeout))
                .set_number_of_retries(1);
            loop {
                match ConnectionManager::new_with_config(client.clone(), config.clone()).await {
                    Ok(m) => return drop(slot.set(m)),
                    Err(e) => warn_redis("connect", &e),
                }
                tokio::time::sleep(CONNECT_RETRY).await;
            }
        });
        Self {
            conn,
            ttl_secs: ttl.as_secs().max(1),
            timeout,
        }
    }

    async fn get(&self, key: &str) -> Option<Cached> {
        let mut conn = self.conn.get()?.clone();
        let res = tokio::time::timeout(
            self.timeout,
            conn.get::<_, Option<Vec<u8>>>(format!("{KEY_PREFIX}{key}")),
        )
        .await;
        match res {
            Ok(Ok(v)) => decode(&v?),
            Ok(Err(e)) => {
                warn_redis("GET", &e);
                None
            }
            Err(_) => {
                warn_redis("GET", &"timed out");
                None
            }
        }
    }

    fn set(&self, key: &str, c: &Cached) {
        let Some(conn) = self.conn.get() else { return };
        let mut conn = conn.clone();
        let (key, val, ttl) = (format!("{KEY_PREFIX}{key}"), encode(c), self.ttl_secs);
        tokio::spawn(async move {
            if let Err(e) = conn.set_ex::<_, _, ()>(key, val, ttl).await {
                warn_redis("SET", &e);
            }
        });
    }
}

/// At most one warning per WARN_EVERY_SECS so a dead Redis cannot flood the logs.
fn warn_redis(op: &str, err: &dyn std::fmt::Display) {
    static LAST: AtomicU64 = AtomicU64::new(0);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let last = LAST.load(Ordering::Relaxed);
    if (last == 0 || now >= last + WARN_EVERY_SECS)
        && LAST
            .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    {
        tracing::warn!("redis {op} failed (treated as cache miss): {err}");
    }
}

/// `status u16 BE | mime len u16 BE | mime | body`. Only cacheable responses are stored.
fn encode(c: &Cached) -> Vec<u8> {
    let mime = &c.mime.as_bytes()[..c.mime.len().min(u16::MAX as usize)];
    let mut v = Vec::with_capacity(4 + mime.len() + c.body.len());
    v.extend_from_slice(&c.status.to_be_bytes());
    v.extend_from_slice(&(mime.len() as u16).to_be_bytes());
    v.extend_from_slice(mime);
    v.extend_from_slice(&c.body);
    v
}

fn decode(v: &[u8]) -> Option<Cached> {
    let status = u16::from_be_bytes(v.get(0..2)?.try_into().ok()?);
    let len = u16::from_be_bytes(v.get(2..4)?.try_into().ok()?) as usize;
    let mime = std::str::from_utf8(v.get(4..4 + len)?).ok()?.to_owned();
    Some(Cached {
        status,
        mime,
        body: Bytes::copy_from_slice(&v[4 + len..]),
        cacheable: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::time::Instant;

    fn settings(memory_bytes: u64, redis_url: Option<String>) -> CacheSettings {
        CacheSettings {
            memory_bytes,
            redis_url,
            redis_ttl: Duration::from_secs(60),
            redis_timeout: Duration::from_millis(150),
        }
    }

    fn resp(body: &str, cacheable: bool) -> Cached {
        Cached {
            status: 200,
            mime: "image/svg+xml".into(),
            body: Bytes::from(body.to_owned()),
            cacheable,
        }
    }

    /// Renders `body`, bumping `calls`.
    async fn get(
        cache: &Cache,
        key: &str,
        body: &str,
        cacheable: bool,
        calls: &AtomicUsize,
    ) -> Cached {
        cache
            .get_or_render(key, || async {
                calls.fetch_add(1, Ordering::SeqCst);
                resp(body, cacheable)
            })
            .await
    }

    #[tokio::test]
    async fn lru_byte_bound() {
        let cache = Cache::new(settings(10_000, None));
        let calls = AtomicUsize::new(0);
        let body = "x".repeat(1000);
        for i in 0..50 {
            get(&cache, &format!("k{i}"), &body, true, &calls).await;
        }
        let l1 = cache.l1.as_ref().unwrap();
        l1.run_pending_tasks().await;
        assert!(l1.weighted_size() <= 10_000, "{}", l1.weighted_size());
        assert!(l1.entry_count() > 0);
        assert_eq!(calls.load(Ordering::SeqCst), 50);
    }

    #[tokio::test]
    async fn lru_keeps_recently_used() {
        // each entry weighs 2 + 13 + 985 = 1000 bytes; room for 3
        let cache = Cache::new(settings(3_000, None));
        let l1 = cache.l1.as_ref().unwrap();
        let calls = AtomicUsize::new(0);
        let body = "x".repeat(985);
        for k in ["ka", "kb", "kc"] {
            get(&cache, k, &body, true, &calls).await;
            l1.run_pending_tasks().await;
        }
        get(&cache, "ka", &body, true, &calls).await; // hit: ka becomes most recent
        l1.run_pending_tasks().await;
        get(&cache, "kd", &body, true, &calls).await;
        l1.run_pending_tasks().await;
        assert_eq!(calls.load(Ordering::SeqCst), 4);
        assert!(l1.contains_key("ka"));
        assert!(!l1.contains_key("kb"));
        assert!(l1.contains_key("kc") && l1.contains_key("kd"));
    }

    async fn concurrent(cacheable: bool) -> (Arc<Cache>, Arc<AtomicUsize>) {
        let cache = Arc::new(Cache::new(settings(1 << 20, None)));
        let calls = Arc::new(AtomicUsize::new(0));
        let mut set = tokio::task::JoinSet::new();
        for _ in 0..100 {
            let (cache, calls) = (cache.clone(), calls.clone());
            set.spawn(async move {
                cache
                    .get_or_render("k", || async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(200)).await;
                        resp("shared", cacheable)
                    })
                    .await
            });
        }
        // Every coalesced waiter sees the flag, so none sends cache headers for a degraded render.
        while let Some(r) = set.join_next().await {
            assert_eq!(r.unwrap(), resp("shared", cacheable));
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        (cache, calls)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn coalesces_concurrent_renders() {
        let (cache, calls) = concurrent(true).await;
        get(&cache, "k", "other", true, &calls).await;
        assert_eq!(calls.load(Ordering::SeqCst), 1, "stored after render");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn non_cacheable_is_shared_but_not_stored() {
        let (cache, calls) = concurrent(false).await;
        let got = get(&cache, "k", "fresh", true, &calls).await;
        assert_eq!(got, resp("fresh", true));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn zero_memory_disables_l1() {
        let cache = Cache::new(settings(0, None));
        assert!(cache.l1.is_none());
        let calls = AtomicUsize::new(0);
        get(&cache, "k", "a", true, &calls).await;
        get(&cache, "k", "a", true, &calls).await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn redis_unreachable_is_a_miss() {
        let cache = Cache::new(settings(0, Some("redis://127.0.0.1:1".into())));
        let calls = AtomicUsize::new(0);
        let start = Instant::now();
        for i in 0..20 {
            assert_eq!(get(&cache, "k", "a", true, &calls).await, resp("a", true));
            if i == 0 {
                tokio::time::sleep(Duration::from_millis(700)).await; // let a connect attempt fail
            }
        }
        assert_eq!(calls.load(Ordering::SeqCst), 20);
        // 20 calls, each bounded by redis_timeout (150ms) at most, plus the 700ms sleep
        assert!(start.elapsed() < Duration::from_millis(700 + 20 * 150));
    }

    #[test]
    fn encoding_round_trips() {
        let c = Cached {
            status: 400,
            mime: "text/plain".into(),
            body: Bytes::from_static(b"\x00\xffbody"),
            cacheable: true,
        };
        assert_eq!(decode(&encode(&c)), Some(c));
        assert_eq!(decode(&[0, 200, 0, 5, b'a']), None);
        assert_eq!(decode(&[0]), None);
    }

    /// Runs only with REDIS_TEST_URL, e.g. `docker run -d --rm -p 6390:6379 redis:7-alpine`
    /// and REDIS_TEST_URL=redis://127.0.0.1:6390.
    ///
    /// TLS: verified 2026-11 against redis:7-alpine started with `--port 0 --tls-port 6391` and a
    /// throwaway CA, using REDIS_TEST_URL=rediss://127.0.0.1:6391/#insecure (the `#insecure`
    /// fragment needs the dev-only tls-rustls-insecure feature). Production trusts only the
    /// webpki (public CA) roots: SSL_CERT_FILE is ignored, so a private-CA server fails with
    /// UnknownIssuer.
    #[tokio::test]
    async fn redis_round_trip() {
        let Ok(url) = std::env::var("REDIS_TEST_URL") else {
            return;
        };
        let new = || Cache::new(settings(0, Some(url.clone())));
        let connected = |c: &Cache| c.l2.as_ref().unwrap().conn.get().cloned();
        let (writer, reader) = (new(), new());
        let deadline = Instant::now() + Duration::from_secs(5);
        while connected(&writer).is_none() || connected(&reader).is_none() {
            assert!(Instant::now() < deadline, "redis did not connect");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let key = format!("rt-{}", std::process::id());
        let calls = AtomicUsize::new(0);
        get(&writer, &key, "stored", true, &calls).await;
        get(&writer, &format!("{key}-nc"), "skip", false, &calls).await;
        tokio::time::sleep(Duration::from_millis(200)).await; // SET is fire-and-forget

        assert_eq!(
            get(&reader, &key, "rendered", true, &calls).await,
            resp("stored", true)
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        get(&reader, &format!("{key}-nc"), "skip", false, &calls).await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            3,
            "non-cacheable not in redis"
        );

        let mut conn = connected(&reader).unwrap();
        let ttl: i64 = conn.ttl(format!("{KEY_PREFIX}{key}")).await.unwrap();
        assert!((1..=60).contains(&ttl), "ttl {ttl}");
    }
}
