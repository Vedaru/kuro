//! Download primitives: sequential single-stream and parallel chunked
//! (range-request) downloads with MD5 verification.
//!
//! Every function takes a list of URLs for the *same* payload, best CDN edge
//! first (see `ApiClient::cdn_candidates`). Requests are retried on transient
//! transport failures, then failed over to the next edge; without that, one
//! stalled edge aborts an entire run (the failure that motivated this).

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::game::ProgressEvent;
use kuro_api::retry;
use kuro_api::{ChunkInfo, Error, Result};

/// Process-wide ceiling on in-flight HTTP requests, one permit per open
/// connection.
///
/// The Kuro CDN throttles per *connection*: measured 2026-09, a single edge
/// streamed ~0.05–0.10 MB/s no matter how fast the client's link was, while N
/// parallel ranges scaled almost linearly (8 connections ≈ 0.6 MB/s against a
/// ~0.8 MB/s machine ceiling). So throughput comes from concurrency, but past
/// a point the fan-out itself provokes stalls — a 16-way batch left requests
/// hanging for minutes, and a documented 32-way fan-out stalled a 4 MiB GET
/// for 52.3s.
///
/// The budget is shared by *every* download in a run rather than handed out
/// per file. A big pak fans its ranges out into the same pool small files draw
/// from, so when a range finishes and a small file has nothing else to do, it
/// takes the freed slot instead of the tail of one file leaving the link idle
/// while the next file waits its turn.
#[derive(Clone, Default)]
pub struct Budget(Option<Arc<Semaphore>>);

impl Budget {
    /// A budget of `permits` concurrent requests.
    pub fn new(permits: usize) -> Self {
        Self(Some(Arc::new(Semaphore::new(permits.max(1)))))
    }

    /// No ceiling — used by tests and callers that manage their own.
    pub fn unlimited() -> Self {
        Self(None)
    }

    /// Take one slot, held until the returned permit is dropped.
    async fn acquire(&self) -> Option<OwnedSemaphorePermit> {
        match &self.0 {
            Some(sem) => Some(
                sem.clone()
                    .acquire_owned()
                    .await
                    .expect("download budget semaphore is never closed"),
            ),
            None => None,
        }
    }
}

/// Download a whole file to `dest` (temp file semantics: caller renames on
/// success). Verifies size and MD5 when provided. Emits per-file progress if
/// a sender is given.
#[allow(clippy::too_many_arguments)]
pub async fn download_single(
    client: &reqwest::Client,
    urls: &[String],
    dest: &Path,
    expected_size: Option<u64>,
    expected_md5: Option<&str>,
    name: &str,
    progress: Option<&tokio::sync::mpsc::Sender<ProgressEvent>>,
    budget: &Budget,
) -> Result<()> {
    retry::retry_across(urls, |url, _| {
        Box::pin(download_single_once(
            client,
            url,
            dest,
            expected_size,
            expected_md5,
            name,
            progress,
            budget,
        ))
    })
    .await
}

/// One complete attempt: fetch, stream to `dest`, verify. Written as a single
/// unit so that a truncated body (caught by the hash check) is retried too.
/// `File::create` truncates, so a retry never appends to a partial file.
#[allow(clippy::too_many_arguments)]
async fn download_single_once(
    client: &reqwest::Client,
    url: &str,
    dest: &Path,
    expected_size: Option<u64>,
    expected_md5: Option<&str>,
    name: &str,
    progress: Option<&tokio::sync::mpsc::Sender<ProgressEvent>>,
    budget: &Budget,
) -> Result<()> {
    // Held for the whole streamed body, released before a retry backs off.
    let _permit = budget.acquire().await;
    let resp = client.get(url).send().await?.error_for_status()?;
    let total = expected_size.or_else(|| resp.content_length()).unwrap_or(0);
    if let Some(tx) = progress {
        let _ = tx
            .send(ProgressEvent::FileProgress {
                name: name.to_string(),
                bytes: 0,
                total,
            })
            .await;
    }
    let mut file = tokio::fs::File::create(dest).await?;
    let mut stream = resp.bytes_stream();
    let mut done: u64 = 0;
    let mut last_report: u64 = 0;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        file.write_all(&chunk).await?;
        done += chunk.len() as u64;
        if let Some(tx) = progress {
            if done - last_report >= (1 << 20) || done >= total {
                last_report = done;
                let _ = tx
                    .send(ProgressEvent::FileProgress {
                        name: name.to_string(),
                        bytes: done,
                        total,
                    })
                    .await;
            }
        }
    }
    file.flush().await?;
    drop(file);
    verify_file(dest, expected_size, expected_md5).await
}

/// Download a file in parallel byte ranges (`chunkInfos` from the manifest).
/// Each chunk is fetched with a `Range` header and written at its offset;
/// every chunk's MD5 is checked, then the whole file's size/MD5 if given.
#[allow(clippy::too_many_arguments)]
pub async fn download_chunked(
    client: &reqwest::Client,
    urls: &[String],
    dest: &Path,
    chunks: &[ChunkInfo],
    expected_md5: Option<&str>,
    concurrency: usize,
    name: &str,
    progress: Option<&tokio::sync::mpsc::Sender<ProgressEvent>>,
    budget: &Budget,
) -> Result<()> {
    if chunks.is_empty() {
        return Err(Error::MissingField("chunkInfos"));
    }
    if urls.is_empty() {
        return Err(Error::NoCdnNode);
    }
    let last_end = chunks.iter().map(|c| c.end).max().unwrap_or(0);
    let total = last_end + 1;

    if let Some(tx) = progress {
        let _ = tx
            .send(ProgressEvent::FileProgress {
                name: name.to_string(),
                bytes: 0,
                total,
            })
            .await;
    }

    // Preallocate so concurrent writers can seek freely.
    {
        let f = tokio::fs::File::create(dest).await?;
        f.set_len(total).await?;
        f.sync_all().await?;
    }

    let urls = Arc::new(urls.to_vec());
    let sem = Arc::new(tokio::sync::Semaphore::new(concurrency.max(1)));
    // Bytes written per chunk, index-stable so each task owns one slot and a
    // retry can reset its own without racing the others.
    let written: Arc<Vec<AtomicU64>> =
        Arc::new((0..chunks.len()).map(|_| AtomicU64::new(0)).collect());
    let finished = Arc::new(AtomicBool::new(false));

    // Body bytes arrive continuously but a chunk only reaches the old
    // completion counter when its whole range lands, so the bar advanced in
    // whole-chunk jumps and stood still between them. A single ticker reports
    // the sum of per-chunk written counters instead — smooth, and no event
    // per streamed piece (which would backpressure the download).
    let reporter = progress.map(|tx| {
        let tx = tx.clone();
        let written = written.clone();
        let finished = finished.clone();
        let name = name.to_string();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(150));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                if finished.load(Ordering::Relaxed) {
                    break;
                }
                let done: u64 = written.iter().map(|b| b.load(Ordering::Relaxed)).sum();
                let _ = tx
                    .send(ProgressEvent::FileProgress {
                        name: name.clone(),
                        bytes: done.min(total),
                        total,
                    })
                    .await;
            }
        })
    });

    let mut handles = Vec::with_capacity(chunks.len());
    for (idx, chunk) in chunks.iter().enumerate() {
        let client = client.clone();
        let urls = urls.clone();
        let dest = dest.to_path_buf();
        let chunk = chunk.clone();
        let sem = sem.clone();
        let written = written.clone();
        let budget = budget.clone();
        handles.push(tokio::spawn(async move {
            let _permit = sem.acquire().await.unwrap();
            let range = format!("bytes={}-{}", chunk.start, chunk.end);
            retry::retry_across(&urls, |url, _| {
                let range = range.clone();
                let md5 = chunk.md5.clone();
                let start = chunk.start;
                let end = chunk.end;
                let url = url.to_string();
                let client = client.clone();
                let dest = dest.clone();
                let budget = budget.clone();
                let counter = &written[idx];
                Box::pin(async move {
                    // Re-fetch overwrites the same region, so count from zero
                    // again rather than double-counting the retried bytes.
                    counter.store(0, Ordering::Relaxed);
                    // One slot per in-flight range, from the shared pool: a
                    // file's ranges compete with every other file's rather than
                    // each file reserving its own.
                    let _permit = budget.acquire().await;
                    let resp = client
                        .get(&url)
                        .header(reqwest::header::RANGE, range)
                        .send()
                        .await?
                        .error_for_status()?;
                    // Stream to the file rather than buffering the range: a
                    // 26 GB pak's range would otherwise sit in RAM, and a
                    // stalled edge shows no progress until it resumes.
                    let mut f = tokio::fs::OpenOptions::new().write(true).open(&dest).await?;
                    f.seek(std::io::SeekFrom::Start(start)).await?;
                    let mut stream = resp.bytes_stream();
                    let mut hasher = md5::Context::new();
                    let mut n: u64 = 0;
                    while let Some(piece) = stream.next().await {
                        let piece = piece?;
                        hasher.consume(&piece);
                        f.write_all(&piece).await?;
                        n += piece.len() as u64;
                        counter.store(n, Ordering::Relaxed);
                    }
                    f.flush().await?;
                    let actual = format!("{:x}", hasher.finalize());
                    if !md5.is_empty() && actual != md5 {
                        return Err(Error::ChecksumMismatch {
                            path: format!("{url} [{start}-{end}]"),
                            expected: md5,
                            actual,
                        });
                    }
                    Ok(())
                })
            })
            .await
        }));
    }

    let mut failed: Option<Error> = None;
    for h in handles {
        // Keep the first error, but drain every handle so the reporter is
        // stopped on the failure path too (an abandoned ticker would keep
        // emitting stale progress into the UI).
        match h.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                if failed.is_none() {
                    failed = Some(e);
                }
            }
            Err(e) => {
                if failed.is_none() {
                    failed = Some(Error::Patch(format!("chunk task join: {e}")));
                }
            }
        }
    }
    finished.store(true, Ordering::Relaxed);
    if let Some(r) = reporter {
        let _ = r.await;
    }
    if let Some(e) = failed {
        return Err(e);
    }
    // The ticker can stop a few milliseconds short of the last byte; close the
    // file's bar exactly at 100% so it never sits at 99% before the swap.
    if let Some(tx) = progress {
        let _ = tx
            .send(ProgressEvent::FileProgress {
                name: name.to_string(),
                bytes: total,
                total,
            })
            .await;
    }

    verify_file(dest, Some(total), expected_md5).await
}

/// Size (+ optional MD5) check of a finished file.
pub async fn verify_file(path: &Path, expected_size: Option<u64>, expected_md5: Option<&str>) -> Result<()> {
    let meta = tokio::fs::metadata(path).await?;
    if let Some(size) = expected_size {
        if meta.len() != size {
            return Err(Error::ChecksumMismatch {
                path: path.display().to_string(),
                expected: size.to_string(),
                actual: meta.len().to_string(),
            });
        }
    }
    if let Some(md5) = expected_md5 {
        if !md5.is_empty() {
            let actual = tokio::task::spawn_blocking({
                let path = path.to_path_buf();
                move || kuro_patch::md5_file(&path)
            })
            .await
            .map_err(|e| Error::Patch(format!("md5 task join: {e}")))??;
            if actual != md5 {
                return Err(Error::ChecksumMismatch {
                    path: path.display().to_string(),
                    expected: md5.to_string(),
                    actual,
                });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kuro-dl-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn download_single_with_no_candidates_fails_cleanly() {
        let dir = temp_dir("nocand");
        let client = reqwest::Client::new();
        let err = download_single(
            &client,
            &[],
            &dir.join("x.bin"),
            None,
            None,
            "x",
            None,
            &Budget::unlimited(),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, Error::NoCdnNode), "got {err:?}");
    }

    #[tokio::test]
    async fn download_chunked_with_no_candidates_fails_cleanly() {
        let dir = temp_dir("nocand-chunked");
        let client = reqwest::Client::new();
        let chunks = vec![ChunkInfo {
            start: 0,
            end: 3,
            md5: String::new(),
        }];
        let err = download_chunked(
            &client,
            &[],
            &dir.join("x.bin"),
            &chunks,
            None,
            2,
            "x",
            None,
            &Budget::unlimited(),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, Error::NoCdnNode), "got {err:?}");
    }
}
