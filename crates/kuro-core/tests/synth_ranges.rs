//! `download_file` is the one place that decides whether a file comes down as
//! one GET or as parallel ranges. A manifest without `chunkInfos` (every pack
//! manifest, CN 3.6.0's patch manifests) must still go ranged once the file is
//! large, or a multi-GB pak is fetched over one throttled connection.
//!
//! These tests watch the requests the server actually receives, not just the
//! bytes that land: the failure mode is a single full-file GET, which produces
//! a byte-identical file.

use std::sync::{Arc, Mutex};

use kuro_core::download::{download_file, Budget, SYNTH_CHUNK_SIZE};

use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// One request as the server saw it: the byte range asked for, if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Seen {
    Whole,
    Range(usize, usize),
}

/// Serve `body` on every path, honouring `Range`, and record each request.
async fn spawn_recording_server(body: Vec<u8>) -> (String, Arc<Mutex<Vec<Seen>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let body = Arc::new(body);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                continue;
            };
            let body = body.clone();
            let log = log.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let head = req.split("\r\n\r\n").next().unwrap_or(&req).to_string();
                let range = head.lines().find_map(|l| {
                    let (name, value) = l.split_once(':')?;
                    if !name.trim().eq_ignore_ascii_case("range") {
                        return None;
                    }
                    let spec = value.trim().strip_prefix("bytes=")?;
                    let (a, b) = spec.split_once('-')?;
                    let a: usize = a.trim().parse().ok()?;
                    let b: usize = b.trim().parse().ok()?;
                    Some((a, b))
                });
                match range {
                    Some((a, b)) => {
                        let b = b.min(body.len() - 1);
                        log.lock().unwrap().push(Seen::Range(a, b));
                        let slice = &body[a..=b];
                        let head = format!(
                            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {a}-{b}/{}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len(),
                            slice.len()
                        );
                        let _ = sock.write_all(head.as_bytes()).await;
                        let _ = sock.write_all(slice).await;
                    }
                    None => {
                        log.lock().unwrap().push(Seen::Whole);
                        let head = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        let _ = sock.write_all(head.as_bytes()).await;
                        let _ = sock.write_all(&body).await;
                    }
                }
            });
        }
    });
    (format!("http://{addr}"), seen)
}

fn payload(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

fn md5_hex(data: &[u8]) -> String {
    format!("{:x}", md5::compute(data))
}

#[tokio::test]
async fn a_large_file_without_chunk_infos_is_fetched_as_ranges() {
    let body = payload(SYNTH_CHUNK_SIZE as usize * 2 + 1);
    let (url, seen) = spawn_recording_server(body.clone()).await;
    let client = kuro_api::build_client().unwrap();
    let dir = std::env::temp_dir().join(format!("kuro-synth-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let dest = dir.join("big.bin");

    download_file(
        &client,
        &[url],
        &dest,
        body.len() as u64,
        &[],
        &md5_hex(&body),
        "big.bin",
        None,
        &Budget::unlimited(),
    )
    .await
    .unwrap();

    assert_eq!(std::fs::read(&dest).unwrap(), body, "ranges must reassemble exactly");
    let seen = seen.lock().unwrap().clone();
    assert!(
        seen.len() >= 3,
        "a {}-byte file with no chunkInfos must not be one connection: {seen:?}",
        body.len()
    );
    assert!(
        seen.iter().all(|s| matches!(s, Seen::Range(..))),
        "every request should carry a Range header: {seen:?}"
    );
    // The ranges must tile the file: no gap, no overlap, nothing past the end.
    let mut spans: Vec<(usize, usize)> = seen
        .iter()
        .filter_map(|s| match s {
            Seen::Range(a, b) => Some((*a, *b)),
            Seen::Whole => None,
        })
        .collect();
    spans.sort();
    let mut next = 0;
    for (a, b) in spans {
        assert_eq!(a, next, "ranges are not contiguous");
        assert!(b >= a, "empty range {a}-{b}");
        next = b + 1;
    }
    assert_eq!(next, body.len(), "ranges do not cover the whole file");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_small_file_without_chunk_infos_is_one_request() {
    let body = payload(64 * 1024);
    let (url, seen) = spawn_recording_server(body.clone()).await;
    let client = kuro_api::build_client().unwrap();
    let dir = std::env::temp_dir().join(format!("kuro-synth-small-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let dest = dir.join("small.bin");

    download_file(
        &client,
        &[url],
        &dest,
        body.len() as u64,
        &[],
        &md5_hex(&body),
        "small.bin",
        None,
        &Budget::unlimited(),
    )
    .await
    .unwrap();

    assert_eq!(std::fs::read(&dest).unwrap(), body);
    assert_eq!(
        seen.lock().unwrap().clone(),
        vec![Seen::Whole],
        "a file smaller than one range is a single GET"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn manifest_chunk_infos_win_over_synthesis() {
    let body = payload(4096);
    let (url, seen) = spawn_recording_server(body.clone()).await;
    let client = kuro_api::build_client().unwrap();
    let dir = std::env::temp_dir().join(format!("kuro-synth-manifest-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let dest = dir.join("hinted.bin");

    // Two ranges the manifest chose, not the 4 MiB default.
    let chunks = vec![
        kuro_api::ChunkInfo {
            start: 0,
            end: 1023,
            md5: String::new(),
        },
        kuro_api::ChunkInfo {
            start: 1024,
            end: 4095,
            md5: String::new(),
        },
    ];
    download_file(
        &client,
        &[url],
        &dest,
        body.len() as u64,
        &chunks,
        &md5_hex(&body),
        "hinted.bin",
        None,
        &Budget::unlimited(),
    )
    .await
    .unwrap();

    assert_eq!(std::fs::read(&dest).unwrap(), body);
    let mut seen = seen.lock().unwrap().clone();
    seen.sort_by_key(|s| match s {
        Seen::Range(a, _) => *a,
        Seen::Whole => 0,
    });
    assert_eq!(seen, vec![Seen::Range(0, 1023), Seen::Range(1024, 4095)]);

    let _ = std::fs::remove_dir_all(&dir);
}
