//! Download primitives: sequential single-stream and parallel chunked
//! (range-request) downloads with MD5 verification.

use std::path::Path;
use std::sync::Arc;

use futures::StreamExt;
use tokio::io::{AsyncSeekExt, AsyncWriteExt};

use kuro_api::{ChunkInfo, Error, Result};

/// Download a whole file to `dest` (temp file semantics: caller renames on
/// success). Verifies size and MD5 when provided.
pub async fn download_single(
    client: &reqwest::Client,
    url: &str,
    dest: &Path,
    expected_size: Option<u64>,
    expected_md5: Option<&str>,
) -> Result<()> {
    let resp = client.get(url).send().await?.error_for_status()?;
    let mut file = tokio::fs::File::create(dest).await?;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        file.write_all(&chunk?).await?;
    }
    file.flush().await?;
    drop(file);
    verify_file(dest, expected_size, expected_md5).await
}

/// Download a file in parallel byte ranges (`chunkInfos` from the manifest).
/// Each chunk is fetched with a `Range` header and written at its offset;
/// every chunk's MD5 is checked, then the whole file's size/MD5 if given.
pub async fn download_chunked(
    client: &reqwest::Client,
    url: &str,
    dest: &Path,
    chunks: &[ChunkInfo],
    expected_md5: Option<&str>,
    concurrency: usize,
) -> Result<()> {
    if chunks.is_empty() {
        return Err(Error::MissingField("chunkInfos"));
    }
    let last_end = chunks.iter().map(|c| c.end).max().unwrap_or(0);
    let expected_size = Some(last_end + 1);

    // Preallocate so concurrent writers can seek freely.
    {
        let f = tokio::fs::File::create(dest).await?;
        f.set_len(expected_size.unwrap()).await?;
        f.sync_all().await?;
    }

    let sem = Arc::new(tokio::sync::Semaphore::new(concurrency.max(1)));
    let mut handles = Vec::with_capacity(chunks.len());
    for chunk in chunks {
        let client = client.clone();
        let url = url.to_string();
        let dest = dest.to_path_buf();
        let chunk = chunk.clone();
        let sem = sem.clone();
        handles.push(tokio::spawn(async move {
            let _permit = sem.acquire().await.unwrap();
            let range = format!("bytes={}-{}", chunk.start, chunk.end);
            let resp = client
                .get(&url)
                .header(reqwest::header::RANGE, range)
                .send()
                .await?
                .error_for_status()?;
            let bytes = resp.bytes().await?;
            let actual = format!("{:x}", md5::compute(&bytes));
            if !chunk.md5.is_empty() && actual != chunk.md5 {
                return Err(Error::ChecksumMismatch {
                    path: format!("{url} [{}-{}]", chunk.start, chunk.end),
                    expected: chunk.md5,
                    actual,
                });
            }
            let mut f = tokio::fs::OpenOptions::new()
                .write(true)
                .open(&dest)
                .await?;
            f.seek(std::io::SeekFrom::Start(chunk.start)).await?;
            f.write_all(&bytes).await?;
            f.flush().await?;
            Ok::<_, Error>(())
        }));
    }
    for h in handles {
        h.await.map_err(|e| Error::Patch(format!("chunk task join: {e}")))??;
    }

    verify_file(dest, Some(expected_size.unwrap()), expected_md5).await
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
