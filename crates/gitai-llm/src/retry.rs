//! Shared retry policy. Rate limits and gateway hiccups are routine when a
//! round fans out across a dozen calls, so this stays deliberately boring.

use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryDecision {
    Ok,
    Retry,
    Fatal,
}

/// 429 and 5xx are worth another go. A 4xx means the request itself is wrong,
/// and repeating it just wastes the rate limit.
pub fn classify(status: u16) -> RetryDecision {
    match status {
        200..=299 => RetryDecision::Ok,
        408 | 409 | 425 | 429 => RetryDecision::Retry,
        500..=599 => RetryDecision::Retry,
        _ => RetryDecision::Fatal,
    }
}

/// Exponential backoff capped at 30s, with `Retry-After` winning when present.
pub fn backoff_delay(attempt: u32, retry_after: Option<u64>) -> Duration {
    if let Some(secs) = retry_after {
        return Duration::from_secs(secs.min(60));
    }
    let secs = 2u64.saturating_pow(attempt).min(30);
    Duration::from_secs(secs)
}

pub async fn sleep_backoff(attempt: u32, retry_after: Option<u64>) {
    tokio::time::sleep(backoff_delay(attempt, retry_after)).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_classification() {
        assert_eq!(classify(200), RetryDecision::Ok);
        assert_eq!(classify(429), RetryDecision::Retry);
        assert_eq!(classify(503), RetryDecision::Retry);
        assert_eq!(classify(401), RetryDecision::Fatal);
        assert_eq!(classify(400), RetryDecision::Fatal);
    }

    #[test]
    fn backoff_grows_then_caps_and_honours_retry_after() {
        assert_eq!(backoff_delay(0, None), Duration::from_secs(1));
        assert_eq!(backoff_delay(3, None), Duration::from_secs(8));
        assert_eq!(backoff_delay(20, None), Duration::from_secs(30));
        assert_eq!(backoff_delay(0, Some(5)), Duration::from_secs(5));
        assert_eq!(backoff_delay(0, Some(9999)), Duration::from_secs(60));
    }
}
