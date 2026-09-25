//! `download_chunked` end-to-end against a Range-aware server that drips each
//! range slowly: the file must come out byte-exact and progress must advance
//! *within* a chunk, not jump only when a whole chunk lands.

use kuro_api::ChunkInfo;
use kuro_core::download::download_chunked;
use kuro_core::ProgressEvent;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Serve `body` with `206` + `Content-Length` for a `Range` request, writing
/// the slice in `pieces` dribbles `delay_ms` apart.
async fn spawn_range_server(body: Vec<u8>, pieces: usize, delay_ms: u64) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let body = std::sync::Arc::new(body);
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                continue;
            };
            let body = body.clone();
            tokio::spawn(async move {
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
                    tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                }
            });
        }
    });
    format!("http://{addr}")
}

fn md5_bytes(data: &[u8]) -> String {
    format!("{:x}", md5::compute(data))
}

#[tokio::test]
async fn chunked_streams_and_reports_progress_within_a_chunk() {
    let dir = std::env::temp_dir().join(format!("kuro-chunked-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let body: Vec<u8> = (0..512 * 1024).map(|i| (i % 251) as u8).collect();
    // ONE chunk spanning the whole file (concurrency 1). A single chunk is
    // deliberate: the old per-whole-chunk implementation emitted exactly one
    // event per completed chunk, so with several chunks it would still produce
    // intermediate numbers and slip past the assertions below. With one chunk
    // it can only emit [0, total] and fails `bytes.len() >= 3`. The server
    // drips the range over ~500 ms so the 150 ms ticker fires inside it.
    let server = spawn_range_server(body.clone(), 10, 50).await;
    let client = kuro_api::build_client().unwrap();

    let chunks: Vec<ChunkInfo> = vec![ChunkInfo {
        start: 0,
        end: body.len() as u64 - 1,
        md5: String::new(),
    }];

    let dest = dir.join("out.bin");
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

    let _ = std::fs::remove_dir_all(&dir);
}
