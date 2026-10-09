//! Per-IP token-bucket rate limiting for Axum servers.
//!
//! This bounds how often a single peer can make a request that is cheap to send
//! and expensive to serve. It is not access control: it does not say who may
//! call an endpoint, only how fast anyone may.
//!
//! Requests are keyed by the peer address of the TCP connection, never by a
//! forwarded-for header, because a header is chosen by the caller. Behind a
//! reverse proxy every client therefore shares one bucket, so the limiter
//! belongs on a directly-exposed listener.

use axum::{
    body::Body,
    extract::{ConnectInfo, State},
    http::{Request, StatusCode},
    middleware::Next,
    response::Response,
};
use log::{debug, warn};
use lru::LruCache;
use parking_lot::Mutex;
use std::net::{IpAddr, SocketAddr};
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Instant;

/// Maximum number of IP addresses to track in the rate limiter cache.
///
/// The cache is LRU, so a peer that rotates addresses evicts other peers'
/// buckets rather than growing memory without bound. Eviction hands the evicted
/// address a fresh bucket, which is the accepted cost of per-IP limiting.
const MAX_TRACKED_IPS: usize = 10_000;

/// Token bucket entry for a single IP address.
struct TokenBucket {
    tokens: f64,
    last_refill: Instant,
}

impl TokenBucket {
    fn new(max_tokens: f64) -> Self {
        Self {
            tokens: max_tokens,
            last_refill: Instant::now(),
        }
    }

    /// Refill tokens based on elapsed time and consume one token if available.
    /// Returns true if a token was consumed, false if rate limited.
    fn try_consume(&mut self, refill_rate: f64, max_tokens: f64) -> bool {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill).as_secs_f64();

        // Refill tokens based on elapsed time
        self.tokens = (self.tokens + elapsed * refill_rate).min(max_tokens);
        self.last_refill = now;

        // Try to consume a token
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// Rate limiter state shared across all request handlers.
pub struct RateLimiterState {
    buckets: Mutex<LruCache<IpAddr, TokenBucket>>,
    max_tokens: f64,
    refill_rate: f64, // tokens per second
    on_refused: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl RateLimiterState {
    /// Create a new rate limiter with the specified requests per minute limit.
    ///
    /// # Arguments
    /// * `requests_per_minute` - Maximum requests allowed per minute per IP
    pub fn new(requests_per_minute: u32) -> Self {
        let max_tokens = f64::from(requests_per_minute.max(1));
        let refill_rate = max_tokens / 60.0; // Convert to per-second rate

        Self {
            buckets: Mutex::new(LruCache::new(
                NonZeroUsize::new(MAX_TRACKED_IPS).expect("cache size must be > 0"),
            )),
            max_tokens,
            refill_rate,
            on_refused: None,
        }
    }

    pub fn with_refusal_callback(mut self, callback: Arc<dyn Fn() + Send + Sync>) -> Self {
        self.on_refused = Some(callback);
        self
    }

    /// Check if a request from the given address should be allowed.
    pub fn check(&self, ip: IpAddr) -> bool {
        let mut buckets = self.buckets.lock();
        let allowed = if let Some(bucket) = buckets.get_mut(&ip) {
            bucket.try_consume(self.refill_rate, self.max_tokens)
        } else {
            // New address: start from a full bucket and spend one token on this
            // request.
            let mut bucket = TokenBucket::new(self.max_tokens);
            bucket.tokens -= 1.0;
            buckets.put(ip, bucket);
            true
        };
        drop(buckets);
        if !allowed && let Some(callback) = &self.on_refused {
            callback();
        }
        allowed
    }
}

/// Axum middleware that applies rate limiting based on the peer address.
///
/// The limiter is a typed piece of middleware state rather than something read
/// out of request extensions, so a server cannot be wired up in a way that
/// silently serves every request unlimited.
pub async fn rate_limit_middleware(
    State(limiter): State<Arc<RateLimiterState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    request: Request<Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    let ip = addr.ip();
    if !limiter.check(ip) {
        warn!("rate_limiter: rejecting request from {ip} - rate limit exceeded");
        return Err(StatusCode::TOO_MANY_REQUESTS);
    }
    debug!("rate_limiter: allowing request from {ip}");
    Ok(next.run(request).await)
}

/// Create the shared state for [`rate_limit_middleware`].
///
/// # Arguments
/// * `requests_per_minute` - Maximum requests allowed per minute per IP
///
/// # Example
/// ```
/// use axum::{Router, middleware, routing::get};
/// use axum_support::{create_rate_limiter, rate_limit_middleware};
///
/// let app: Router = Router::new()
///     .route("/api", get(|| async { "ok" }))
///     .layer(middleware::from_fn_with_state(
///         create_rate_limiter(600),
///         rate_limit_middleware,
///     ));
/// ```
///
/// The middleware identifies callers by the peer address of the connection, so
/// the server must be run with
/// `into_make_service_with_connect_info::<std::net::SocketAddr>()`. The helpers
/// in this crate already do that.
pub fn create_rate_limiter(requests_per_minute: u32) -> Arc<RateLimiterState> {
    Arc::new(RateLimiterState::new(requests_per_minute))
}

pub fn create_rate_limiter_with_callback(
    requests_per_minute: u32,
    callback: Arc<dyn Fn() + Send + Sync>,
) -> Arc<RateLimiterState> {
    Arc::new(RateLimiterState::new(requests_per_minute).with_refusal_callback(callback))
}

/// Apply per-IP rate limiting to `router`, or leave it untouched when
/// `requests_per_minute` is zero.
///
/// Zero means "no limit" so an operator can turn the limiter off for a
/// deployment where every caller shares one source address, without the call
/// site growing a conditional.
pub fn with_rate_limit(router: axum::Router, requests_per_minute: u32) -> axum::Router {
    with_rate_limit_callback(router, requests_per_minute, None)
}

pub fn with_rate_limit_callback(
    router: axum::Router,
    requests_per_minute: u32,
    callback: Option<Arc<dyn Fn() + Send + Sync>>,
) -> axum::Router {
    if requests_per_minute == 0 {
        log::warn!("rate_limiter: disabled by configuration; this listener will not be throttled");
        return router;
    }
    let limiter = match callback {
        Some(callback) => create_rate_limiter_with_callback(requests_per_minute, callback),
        None => create_rate_limiter(requests_per_minute),
    };
    router.layer(axum::middleware::from_fn_with_state(
        limiter,
        rate_limit_middleware,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(last: u8) -> IpAddr {
        IpAddr::from([192, 168, 1, last])
    }

    #[test]
    fn requests_under_the_limit_are_allowed() {
        let limiter = RateLimiterState::new(10);
        for attempt in 0..10 {
            assert!(limiter.check(ip(1)), "request {attempt} should be allowed");
        }
    }

    #[test]
    fn excess_requests_are_blocked() {
        let limiter = RateLimiterState::new(5);
        for _ in 0..5 {
            assert!(limiter.check(ip(2)));
        }
        assert!(!limiter.check(ip(2)));
    }

    #[tokio::test]
    async fn refusal_callback_runs_only_for_rejected_requests() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let refusals = Arc::new(AtomicUsize::new(0));
        let callback_refusals = refusals.clone();
        let limiter = Arc::new(RateLimiterState::new(1).with_refusal_callback(Arc::new(
            move || {
                callback_refusals.fetch_add(1, Ordering::Relaxed);
            },
        )));
        assert!(limiter.check(ip(7)));
        assert!(!limiter.check(ip(7)));
        assert_eq!(refusals.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn addresses_are_tracked_separately() {
        let limiter = RateLimiterState::new(2);
        assert!(limiter.check(ip(3)));
        assert!(limiter.check(ip(3)));
        assert!(!limiter.check(ip(3)));

        // A different peer still has its own budget.
        assert!(limiter.check(ip(4)));
    }

    #[test]
    fn a_bucket_refills_over_time() {
        // 60 per minute is one per second, so a bucket drained now regains a
        // token shortly afterwards.
        let limiter = RateLimiterState::new(60);
        for _ in 0..60 {
            assert!(limiter.check(ip(5)));
        }
        assert!(!limiter.check(ip(5)));
        std::thread::sleep(std::time::Duration::from_millis(1_100));
        assert!(
            limiter.check(ip(5)),
            "the bucket must refill rather than latch closed"
        );
    }

    #[test]
    fn a_zero_limit_is_treated_as_one_rather_than_dividing_by_nothing() {
        let limiter = RateLimiterState::new(0);
        assert!(limiter.check(ip(6)));
        assert!(!limiter.check(ip(6)));
    }
}
