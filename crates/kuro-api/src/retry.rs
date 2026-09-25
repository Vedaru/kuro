//! Bounded retry with exponential backoff for transient transport failures.
//!
//! The Kuro CDN edges stall: a handshake or a mid-body read can hang for tens
//! of seconds while the identical request succeeds a moment later on another
//! edge (measured 2026-09: one 4 MiB ranged GET took 52.3s under 32-way
//! fan-out while its peers finished in 2.8–8.4s). Retrying the request is
//! cheaper and more reliable than trying to predict which edge will stall.
//!
//! Only failures that can plausibly be transport noise are retried —
//! a checksum mismatch (a truncated or truncated-looking body) and reqwest's
//! timeout/connect/request/body classes. A missing manifest, a bad path or a
//! JSON shape error is deterministic and fails immediately.

use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::error::{Error, Result};

/// Total attempts per request, including the first.
pub const ATTEMPTS: usize = 4;

const BACKOFF_BASE: Duration = Duration::from_millis(250);
const JITTER_MS: u32 = 250;

/// A boxed future borrowing from the caller's captured state. The `async move`
/// block in each `op` closure moves its own copies of the borrows in, so the
/// returned future never borrows the closure itself and can be dropped before
/// the next attempt.
pub type Attempt<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

/// Whether `e` is worth another attempt.
pub fn is_transient(e: &Error) -> bool {
    match e {
        // `is_decode` matters: a response body cut short mid-stream surfaces as
        // kind Decode (`hyper::Error(Body, UnexpectedEof)`), not Body, and the
        // CDN is known to truncate under load. Without it a truncated body is
        // never retried and only multi-edge failover saves the request.
        Error::Http(re) => {
            re.is_timeout()
                || re.is_connect()
                || re.is_request()
                || re.is_body()
                || re.is_decode()
        }
        // A body that failed its MD5 is retried: the CDN is known to truncate
        // under load, and the manifest's hash is authoritative.
        Error::ChecksumMismatch { .. } => true,
        _ => false,
    }
}

fn backoff(attempt: usize) -> Duration {
    let exp = 2u32.saturating_pow(attempt.saturating_sub(1) as u32);
    BACKOFF_BASE.saturating_mul(exp) + jitter()
}

fn jitter() -> Duration {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    Duration::from_millis((nanos % JITTER_MS) as u64)
}

/// Run `op` until it succeeds, `attempts` is exhausted, or it returns a
/// non-transient error. `op` receives the 1-based attempt number.
pub async fn retry<'a, T, F>(attempts: usize, mut op: F) -> Result<T>
where
    F: FnMut(usize) -> Attempt<'a, T>,
{
    let total = attempts.max(1);
    let mut attempt = 1;
    loop {
        let outcome = op(attempt).await;
        match outcome {
            Ok(value) => return Ok(value),
            Err(e) if attempt < total && is_transient(&e) => {
                let wait = backoff(attempt);
                attempt += 1;
                tokio::time::sleep(wait).await;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Run `op` with per-URL retries, walking `urls` until one succeeds.
///
/// `urls` are the same payload on different CDN edges (best first). Each edge
/// gets the full [`ATTEMPTS`] budget before moving on — a dead edge costs a
/// few quick failures, whereas retrying the whole list in lockstep would keep
/// hammering the edge that is known-bad.
pub async fn retry_across<'a, T, F>(urls: &'a [String], mut op: F) -> Result<T>
where
    F: FnMut(&'a str, usize) -> Attempt<'a, T>,
{
    if urls.is_empty() {
        return Err(Error::NoCdnNode);
    }
    let mut last: Option<Error> = None;
    for url in urls {
        match retry(ATTEMPTS, |attempt| op(url, attempt)).await {
            Ok(value) => return Ok(value),
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or(Error::NoCdnNode))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn returns_first_success_without_sleeping() {
        let calls = AtomicUsize::new(0);
        let out: Result<u32> = retry(ATTEMPTS, |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(7) })
        })
        .await;
        assert_eq!(out.unwrap(), 7);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn retries_transient_then_succeeds() {
        let calls = AtomicUsize::new(0);
        let out: Result<u32> = retry(ATTEMPTS, |attempt| {
            calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if attempt < 3 {
                    Err(Error::ChecksumMismatch {
                        path: "x".into(),
                        expected: "a".into(),
                        actual: "b".into(),
                    })
                } else {
                    Ok(attempt as u32)
                }
            })
        })
        .await;
        assert_eq!(out.unwrap(), 3);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn gives_up_after_all_attempts() {
        let calls = AtomicUsize::new(0);
        let out: Result<u32> = retry(2, |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {
                Err(Error::ChecksumMismatch {
                    path: "x".into(),
                    expected: "a".into(),
                    actual: "b".into(),
                })
            })
        })
        .await;
        assert!(out.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn falls_over_to_the_next_candidate() {
        let urls = vec!["dead".to_string(), "alive".to_string()];
        let out: Result<String> = retry_across(&urls, |url, _| {
            let url = url.to_string();
            Box::pin(async move {
                if url == "alive" {
                    Ok(url)
                } else {
                    Err(Error::NoCdnNode)
                }
            })
        })
        .await;
        assert_eq!(out.unwrap(), "alive");
    }

    #[tokio::test]
    async fn empty_candidate_list_is_an_error() {
        let urls: Vec<String> = vec![];
        let out: Result<u32> = retry_across(&urls, |_, _| Box::pin(async { Ok(0) })).await;
        assert!(matches!(out, Err(Error::NoCdnNode)));
    }

    #[tokio::test]
    async fn does_not_retry_deterministic_errors() {
        let calls = AtomicUsize::new(0);
        let out: Result<u32> = retry(ATTEMPTS, |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Err(Error::NoCdnNode) })
        })
        .await;
        assert!(matches!(out, Err(Error::NoCdnNode)));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
