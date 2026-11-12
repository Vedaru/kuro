//! `download_chunked` end-to-end against a Range-aware server that drips each
//! range slowly: the file must come out byte-exact and progress must advance
//! *within* a chunk, not jump only when a whole chunk lands.

mod common;

use common::md5_bytes;
use kuro_api::ChunkInfo;
use kuro_core::download::{download_chunked, Budget};
use kuro_core::ProgressEvent;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Serve `body` with `206` + `Content-Length` for a `Range` request, writing
/// the slice in `pieces` dribbles `delay_ms` apart. Returns the URL, the
/// number of connections accepted, and how many are still streaming.
async fn spawn_range_server(
    body: Vec<u8>,
    pieces: usize,
    delay_ms: u64,
) -> (String, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let body = Arc::new(body);
    let started = Arc::new(AtomicUsize::new(0));
    let active = Arc::new(AtomicUsize::new(0));
    let (started_srv, active_srv) = (started.clone(), active.clone());
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                continue;
            };
            let body = body.clone();
            let started = started_srv.clone();
            let active = active_srv.clone();
            tokio::spawn(async move {
                struct Open(Arc<AtomicUsize>);
                impl Drop for Open {
                    fn drop(&mut self) {
                        self.0.fetch_sub(1, Ordering::SeqCst);
                    }
                }
                started.fetch_add(1, Ordering::SeqCst);
                active.fetch_add(1, Ordering::SeqCst);
                let _open = Open(active);
                let mut buf = vec![0u8; 8192];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_ascii_lowercase();
                let mut start = 0usize;
                let mut end = body.len().saturating_sub(1);
                for line in req.lines() {
                    if let Some(v) = line.trim().strip_prefix("range: bytes=") {
                        let (a, b) = v.split_once('-').unwrap();
                        start = a.trim().parse().unwrap();
                        end = b.trim().parse().unwrap();
                    }
                }
                let slice = &body[start..=end];
                let head = format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    slice.len()
                );
                if sock.write_all(head.as_bytes()).await.is_err() {
                    return;
                }
                let step = slice.len().div_ceil(pieces.max(1)).max(1);
                for piece in slice.chunks(step) {
                    if sock.write_all(piece).await.is_err() {
                        return;
                    }
                    let _ = sock.flush().await;
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                }
            });
        }
    });
    (format!("http://{addr}"), started, active)
}

#[tokio::test]
async fn chunked_streams_and_reports_progress_within_a_chunk() {
    let dir = tempfile::Builder::new().prefix("kuro-chunked-").tempdir().unwrap();

    let body: Vec<u8> = (0..512 * 1024).map(|i| (i % 251) as u8).collect();
    // ONE chunk spanning the whole file (concurrency 1). A single chunk is
    // deliberate: the old per-whole-chunk implementation emitted exactly one
    // event per completed chunk, so with several chunks it would still produce
    // intermediate numbers and slip past the assertions below. With one chunk
    // it can only emit [0, total] and fails `bytes.len() >= 3`. The server
    // drips the range over ~500 ms so the 150 ms ticker fires inside it.
    let (server, _, _) = spawn_range_server(body.clone(), 10, 50).await;
    let client = kuro_api::build_client().unwrap();

    let chunks: Vec<ChunkInfo> = vec![ChunkInfo {
        start: 0,
        end: body.len() as u64 - 1,
        md5: String::new(),
    }];

    let dest = dir.path().join("out.bin");
    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    download_chunked(
        &client,
        &[server],
        &dest,
        &chunks,
        Some(&md5_bytes(&body)),
        1,
        "out.bin",
        Some(&tx),
        &Budget::unlimited(),
    )
    .await
    .unwrap();
    drop(tx);

    let mut events = Vec::new();
    while let Ok(e) = rx.try_recv() {
        events.push(e);
    }

    // File is byte-exact (a truncated or misaligned chunk would fail this).
    assert_eq!(std::fs::read(&dest).unwrap(), body);

    let total = body.len() as u64;
    let bytes: Vec<u64> = events
        .iter()
        .map(|e| match e {
            ProgressEvent::FileProgress { bytes, total: t, .. } => {
                assert_eq!(*t, total, "every event reports the full file size");
                *bytes
            }
            other => panic!("unexpected event: {other:?}"),
        })
        .collect();

    assert!(bytes.len() >= 3, "expected several progress ticks, got {bytes:?}");
    assert!(
        bytes.windows(2).all(|w| w[0] <= w[1]),
        "progress must not go backwards: {bytes:?}"
    );
    assert!(
        bytes.iter().any(|&b| b > 0 && b < total),
        "progress must advance inside a chunk, not only on whole-chunk completion: {bytes:?}"
    );
    assert_eq!(
        bytes.last().copied(),
        Some(total),
        "the last event must close the file's bar at 100%: {bytes:?}"
    );
}

/// Aborting the parent task must abort the chunk tasks too. The server counts
/// connections that are still streaming, so a leaked chunk task shows up as a
/// connection that outlives the abort.
#[tokio::test]
async fn aborting_the_task_closes_the_download_connections() {
    let dir = tempfile::Builder::new().prefix("kuro-abort-").tempdir().unwrap();
    let body = vec![9u8; 4 * 1024 * 1024];
    // One chunk, 4 MiB, dripped over ~4 s: long enough that a leaked task is
    // still streaming well after the abort.
    let (server, started, active) = spawn_range_server(body.clone(), 40, 100).await;
    let client = kuro_api::build_client().unwrap();
    let chunks: Vec<ChunkInfo> = vec![ChunkInfo {
        start: 0,
        end: body.len() as u64 - 1,
        md5: String::new(),
    }];
    let dest = dir.path().join("out.bin");
    let (tx, rx) = tokio::sync::mpsc::channel(64);
    let task = tokio::spawn(async move {
        let _ = download_chunked(
            &client,
            &[server],
            &dest,
            &chunks,
            None,
            1,
            "out.bin",
            Some(&tx),
            &Budget::unlimited(),
        )
        .await;
    });

    let deadline = Instant::now() + Duration::from_secs(5);
    while started.load(Ordering::SeqCst) == 0 {
        assert!(Instant::now() < deadline, "server never saw the chunk request");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    task.abort();
    let _ = task.await;
    // Requesting the abort is not enough; wait for the connection to go away.
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(
        active.load(Ordering::SeqCst),
        0,
        "a chunk connection is still streaming after the task was aborted"
    );
    drop(rx);
}
