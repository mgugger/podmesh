//! Simple IP-based rate limiter middleware for Axum.
//!
//! This module provides a token bucket rate limiter that tracks request counts
//! per IP address and rejects requests that exceed the configured limit.

use axum::{
    body::Body,
    extract::ConnectInfo,
    http::{Request, StatusCode},
    middleware::Next,
    response::Response,
};
use log::{debug, warn};
use lru::LruCache;
use parking_lot::Mutex;
use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Instant;

/// Maximum number of IP addresses to track in the rate limiter cache.
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
    buckets: Mutex<LruCache<String, TokenBucket>>,
    max_tokens: f64,
    refill_rate: f64, // tokens per second
}

impl RateLimiterState {
    /// Create a new rate limiter with the specified requests per minute limit.
    ///
    /// # Arguments
    /// * `requests_per_minute` - Maximum requests allowed per minute per IP
    pub fn new(requests_per_minute: u32) -> Self {
        let max_tokens = requests_per_minute as f64;
        let refill_rate = max_tokens / 60.0; // Convert to per-second rate

        Self {
            buckets: Mutex::new(LruCache::new(
                NonZeroUsize::new(MAX_TRACKED_IPS).expect("cache size must be > 0"),
            )),
            max_tokens,
            refill_rate,
        }
    }

    /// Check if a request from the given IP should be allowed.
    pub fn check(&self, ip: &str) -> bool {
        let mut buckets = self.buckets.lock();

        if let Some(bucket) = buckets.get_mut(ip) {
            bucket.try_consume(self.refill_rate, self.max_tokens)
        } else {
            // New IP - create bucket with full tokens minus one for this request
            let mut bucket = TokenBucket::new(self.max_tokens);
            bucket.tokens -= 1.0; // Consume token for this request
            buckets.put(ip.to_string(), bucket);
            true
        }
    }
}

/// Axum middleware that applies rate limiting based on client IP address.
///
/// # Arguments
/// * `state` - Shared rate limiter state
/// * `request` - Incoming HTTP request
/// * `next` - Next middleware/handler in the chain
pub async fn rate_limit_middleware(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    request: Request<Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    // Get the rate limiter from request extensions
    let rate_limiter = request.extensions().get::<Arc<RateLimiterState>>().cloned();

    if let Some(limiter) = rate_limiter {
        let ip = addr.ip().to_string();

        if !limiter.check(&ip) {
            warn!(
                "rate_limiter: rejecting request from {} - rate limit exceeded",
                ip
            );
            return Err(StatusCode::TOO_MANY_REQUESTS);
        }

        debug!("rate_limiter: allowing request from {}", ip);
    }

    Ok(next.run(request).await)
}

/// Create a rate limiter layer for use with Axum routers.
///
/// # Arguments
/// * `requests_per_minute` - Maximum requests allowed per minute per IP
///
/// # Example
/// ```ignore
/// let app = Router::new()
///     .route("/api", get(handler))
///     .layer(rate_limiter_layer(100));
/// ```
pub fn create_rate_limiter(requests_per_minute: u32) -> Arc<RateLimiterState> {
    Arc::new(RateLimiterState::new(requests_per_minute))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rate_limiter_allows_requests_under_limit() {
        let limiter = RateLimiterState::new(10); // 10 per minute

        // Should allow first 10 requests immediately
        for i in 0..10 {
            assert!(
                limiter.check("192.168.1.1"),
                "Request {} should be allowed",
                i
            );
        }
    }

    #[test]
    fn test_rate_limiter_blocks_excess_requests() {
        let limiter = RateLimiterState::new(5); // 5 per minute

        // Exhaust the bucket
        for _ in 0..5 {
            assert!(limiter.check("192.168.1.2"));
        }

        // 6th request should be blocked
        assert!(!limiter.check("192.168.1.2"));
    }

    #[test]
    fn test_rate_limiter_tracks_ips_separately() {
        let limiter = RateLimiterState::new(2); // 2 per minute

        // Exhaust IP 1's bucket
        assert!(limiter.check("192.168.1.1"));
        assert!(limiter.check("192.168.1.1"));
        assert!(!limiter.check("192.168.1.1")); // Blocked

        // IP 2 should still have tokens
        assert!(limiter.check("192.168.1.2"));
        assert!(limiter.check("192.168.1.2"));
    }
}
