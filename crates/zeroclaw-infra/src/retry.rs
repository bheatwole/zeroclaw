//! Shared exponential-backoff + `Retry-After` parsing for HTTP retry loops.
//!
//! Promoted out of `zeroclaw-channels` (formerly duplicated between
//! `webhook.rs` and `slack.rs`) so every caller — native channels and, going
//! forward, the proxy-aware plugin `wasi:http` hook — shares one
//! implementation.

use chrono::{DateTime, NaiveDateTime, Utc};
use std::time::Duration;

/// Exponential backoff with jitter, bounded by `max_delay_ms`.
///
/// `attempt` is zero-based (0 = first retry). Jitter (±25%) is applied
/// before the cap, so the returned delay is strictly `<= max_delay_ms`.
pub fn compute_backoff(attempt: u32, base_delay_ms: u64, max_delay_ms: u64) -> Duration {
    let multiplier = 1_u64.checked_shl(attempt).unwrap_or(u64::MAX);
    let base = base_delay_ms.saturating_mul(multiplier);
    let jittered = apply_jitter(base);
    let capped = jittered.min(max_delay_ms);
    Duration::from_millis(capped)
}

/// Apply ±25% jitter to a delay so parallel callers don't thunder-herd.
fn apply_jitter(delay_ms: u64) -> u64 {
    if delay_ms == 0 {
        return 0;
    }
    let jitter_factor = 0.75 + (rand::random::<f64>() * 0.5);
    // Safe: jitter_factor > 0 keeps the product non-negative; f64->u64 cast saturates on overflow.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let jittered = ((delay_ms as f64) * jitter_factor) as u64;
    jittered
}

/// Parse a `Retry-After` header value into a millisecond delay. Supports
/// integer seconds, decimal seconds (truncated to whole seconds), and
/// HTTP-date values.
pub fn parse_retry_after_ms(value: &str) -> Option<u64> {
    parse_retry_after_ms_at(value, Utc::now())
}

/// Same as [`parse_retry_after_ms`] but with an injectable reference instant,
/// for deterministic tests of HTTP-date parsing.
pub fn parse_retry_after_ms_at(value: &str, now: DateTime<Utc>) -> Option<u64> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(seconds) = trimmed.parse::<u64>() {
        return Some(seconds.saturating_mul(1000));
    }
    let whole = trimmed
        .split_once('.')
        .map(|(whole, _)| whole)
        .unwrap_or(trimmed);
    if let Ok(seconds) = whole.parse::<u64>() {
        return Some(seconds.saturating_mul(1000));
    }

    parse_retry_after_http_date(trimmed).map(|date| {
        let delay_ms = date.signed_duration_since(now).num_milliseconds();
        if delay_ms <= 0 {
            0
        } else {
            u64::try_from(delay_ms).unwrap_or(u64::MAX)
        }
    })
}

fn parse_retry_after_http_date(value: &str) -> Option<DateTime<Utc>> {
    if let Ok(date) = NaiveDateTime::parse_from_str(value, "%a, %d %b %Y %H:%M:%S GMT") {
        return Some(DateTime::from_naive_utc_and_offset(date, Utc));
    }
    if let Ok(date) = NaiveDateTime::parse_from_str(value, "%A, %d-%b-%y %H:%M:%S GMT") {
        return Some(DateTime::from_naive_utc_and_offset(date, Utc));
    }
    NaiveDateTime::parse_from_str(value, "%a %b %e %H:%M:%S %Y")
        .ok()
        .map(|date| DateTime::from_naive_utc_and_offset(date, Utc))
}

/// Whether an HTTP status code should be retried by a generic caller (429
/// rate-limit or any 5xx server error).
pub fn is_retryable_status(status: u16) -> bool {
    status == 429 || (500..600).contains(&status)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_and_caps() {
        let d0 = compute_backoff(0, 500, 30_000);
        let d3 = compute_backoff(3, 500, 30_000);
        assert!(d0.as_millis() <= 30_000);
        assert!(d3.as_millis() <= 30_000);
        // attempt 3 multiplier is 8x base before jitter/cap.
        let d_huge = compute_backoff(20, 500, 30_000);
        assert_eq!(d_huge.as_millis(), 30_000);
    }

    #[test]
    fn parse_retry_after_integer_seconds() {
        assert_eq!(parse_retry_after_ms("2"), Some(2000));
    }

    #[test]
    fn parse_retry_after_decimal_seconds() {
        assert_eq!(parse_retry_after_ms("2.5"), Some(2000));
    }

    #[test]
    fn parse_retry_after_empty_is_none() {
        assert_eq!(parse_retry_after_ms(""), None);
        assert_eq!(parse_retry_after_ms("   "), None);
    }

    #[test]
    fn parse_retry_after_http_date_in_past_clamps_to_zero() {
        let ms = parse_retry_after_ms_at("Sun, 06 Nov 1994 08:49:37 GMT", Utc::now());
        assert_eq!(ms, Some(0));
    }

    #[test]
    fn retryable_status_codes() {
        assert!(is_retryable_status(429));
        assert!(is_retryable_status(503));
        assert!(!is_retryable_status(404));
        assert!(!is_retryable_status(200));
    }
}
