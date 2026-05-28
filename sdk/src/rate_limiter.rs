use std::sync::Arc;

use parking_lot::Mutex;

/// Simple token-bucket rate limiter.
///
/// Call `acquire().await` before each network operation.
/// The limiter ensures at most `max_rps` calls per second by sleeping
/// when tokens are exhausted.
///
/// Thread-safe and Clone-friendly via `Arc` interior.
#[derive(Clone, Debug)]
pub struct RateLimiter {
    inner: Arc<Mutex<TokenBucket>>,
}

#[derive(Debug)]
struct TokenBucket {
    /// Minimum interval between requests in milliseconds.
    interval_ms: u64,
    /// Timestamp (ms) when the next request is allowed.
    next_allowed_ms: u64,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(all(feature = "wasm", target_arch = "wasm32"))]
async fn sleep_ms(ms: u64) {
    use gloo_timers::future::TimeoutFuture;

    let ms_u32 = ms.min(u32::MAX as u64) as u32;
    TimeoutFuture::new(ms_u32).await;
}

#[cfg(not(all(feature = "wasm", target_arch = "wasm32")))]
async fn sleep_ms(ms: u64) {
    tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
}

#[cfg(all(feature = "wasm", target_arch = "wasm32"))]
fn now_ms_platform() -> u64 {
    js_sys::Date::now() as u64
}

#[cfg(not(all(feature = "wasm", target_arch = "wasm32")))]
fn now_ms_platform() -> u64 {
    now_ms()
}

impl RateLimiter {
    /// Creates a rate limiter allowing `max_rps` requests per second.
    ///
    /// Panics if `max_rps` is 0.
    pub fn new(max_rps: u32) -> Self {
        assert!(max_rps > 0, "max_rps must be > 0");
        Self {
            inner: Arc::new(Mutex::new(TokenBucket {
                interval_ms: 1000 / (max_rps as u64),
                next_allowed_ms: 0,
            })),
        }
    }

    /// Convenience: creates `Some(RateLimiter)` if `max_rps` is provided.
    pub fn optional(max_rps: Option<u32>) -> Option<Self> {
        max_rps.map(Self::new)
    }

    /// Waits until a request is allowed, then reserves a slot.
    pub async fn acquire(&self) {
        let wait_ms = {
            let mut bucket = self.inner.lock();
            let now = now_ms_platform();
            if now >= bucket.next_allowed_ms {
                bucket.next_allowed_ms = now + bucket.interval_ms;
                0
            } else {
                let wait = bucket.next_allowed_ms - now;
                bucket.next_allowed_ms += bucket.interval_ms;
                wait
            }
        };

        if wait_ms > 0 {
            sleep_ms(wait_ms).await;
        }
    }
}

/// Helper to call `acquire` on an optional rate limiter.
pub async fn maybe_acquire(limiter: Option<&RateLimiter>) {
    if let Some(rl) = limiter {
        rl.acquire().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limiter_creation() {
        let rl = RateLimiter::new(10);
        // interval = 1000/10 = 100ms
        assert_eq!(rl.inner.lock().interval_ms, 100);
    }

    #[test]
    fn optional_none() {
        assert!(RateLimiter::optional(None).is_none());
    }

    #[test]
    fn optional_some() {
        assert!(RateLimiter::optional(Some(5)).is_some());
    }

    #[test]
    #[should_panic(expected = "max_rps must be > 0")]
    fn zero_rps_panics() {
        RateLimiter::new(0);
    }
}
