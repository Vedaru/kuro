//! Shared test helpers: a minimal in-process HTTP server.

use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Serve the given `(path, body)` routes on 127.0.0.1 and return the base URL.
/// Paths must start with `/`.
pub async fn spawn_http_server(routes: Vec<(String, Vec<u8>)>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                continue;
            };
            let routes = routes.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                // Ranged downloads pipeline several requests per connection. A
                // second request read in the same 4 KiB fills `buf` after the
                // first, so take the leading request only: leaving them stacked
                // makes the range parse read the wrong offsets.
                let req = req.split("\r\n\r\n").next().unwrap_or(&req);
                let path = req
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("/");
                match routes.iter().find(|(p, _)| p == path) {
                    Some((_, body)) => {
                        // Ranged requests are how large files are fetched; a
                        // server that ignored `Range` would turn every chunked
                        // download into a mismatch, so honour it. Header names
                        // are case-insensitive and reqwest sends them lowercased.
                        let range = req.lines().find_map(|l| {
                            let (name, value) = l.split_once(':')?;
                            if !name.trim().eq_ignore_ascii_case("range") {
                                return None;
                            }
                            let spec = value.trim().strip_prefix("bytes=")?;
                            let (start, end) = spec.split_once('-')?;
                            let start: usize = start.parse().ok()?;
                            let end: usize = end.parse().unwrap_or(body.len() - 1);
                            Some(start..=end.min(body.len() - 1))
                        });
                        let (status, slice, extra) = match range {
                            Some(r) => (
                                "206 Partial Content",
                                &body[r.clone()],
                                format!(
                                    "Content-Range: bytes {}-{}/{}\r\n",
                                    r.start(),
                                    r.end(),
                                    body.len()
                                ),
                            ),
                            None => ("200 OK", &body[..], String::new()),
                        };
                        let head = format!(
                            "HTTP/1.1 {status}\r\nContent-Type: application/octet-stream\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n",
                            slice.len()
                        );
                        let _ = sock.write_all(head.as_bytes()).await;
                        let _ = sock.write_all(slice).await;
                    }
                    None => {
                        let head = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                        let _ = sock.write_all(head.as_bytes()).await;
                    }
                }
            });
        }
    });
    format!("http://{addr}")
}
