//! `kuro-patch` — native KrDiff / HDiff patch application.
//!
//! Wraps [`hdiffpatch-rs`](https://github.com/TwintailTeam/hdiffpatch-rs) — a
//! pure-Rust implementation of HDiffPatch including KuroGames' `KrDiff`
//! directory format (compression: zstd, verified against a real krpdiff from
//! the live WuWa CDN, 2026-08).
//!
//! This crate is what removes the `wine` + `hpatchz.exe` dependency the Python
//! tool needed for the merge step.

use std::path::Path;

use hdiffpatch_rs::patchers::{DiffInfo, KrDiff, PatchOptions};

use kuro_api::Error;

/// Default parallelism for merges (big paks benefit from threads).
const DEFAULT_THREADS: usize = 4;
/// Pipeline memory budget for the patcher.
const MEMORY_BUDGET: usize = 256 << 20;

/// Inspect a patch file without applying it.
pub fn patch_info(diff_path: &Path) -> kuro_api::Result<DiffInfo> {
    let patcher = KrDiff::new(
        String::new(),
        diff_path.to_string_lossy().into_owned(),
        String::new(),
    );
    patcher
        .info()
        .map_err(|e| Error::Patch(format!("krdiff info failed: {e}")))
}

/// Apply a `KrDiff` (krpdiff) directory patch: `source_dir + krpdiff -> out_dir`.
///
/// `source_dir` must contain the *old* version files; the merged *new* files
/// are written to `out_dir` (created if missing). Nothing in `source_dir` is
/// modified — the caller decides when/where to stage results.
pub fn apply_krdiff(source_dir: &Path, krpdiff: &Path, out_dir: &Path) -> kuro_api::Result<()> {
    let mut patcher = KrDiff::new(
        source_dir.to_string_lossy().into_owned(),
        krpdiff.to_string_lossy().into_owned(),
        out_dir.to_string_lossy().into_owned(),
    )
    .with_options(
        PatchOptions::default()
            .with_threads(DEFAULT_THREADS)
            .with_memory_budget(MEMORY_BUDGET),
    );

    if !patcher.apply() {
        return Err(Error::Unimplemented("krdiff apply failed"));
    }
    Ok(())
}

/// MD5 of a file, hex-encoded lowercase. Streams in 1 MiB chunks — reading a
/// whole file into memory would OOM on WuWa's multi-GB paks (pakchunk70 is
/// 26 GB alone).
pub fn md5_file(path: &Path) -> kuro_api::Result<String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut ctx = md5::Context::new();
    let mut buf = [0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        ctx.consume(&buf[..n]);
    }
    Ok(format!("{:x}", ctx.compute()))
}

/// EXPERIMENTAL: create a KrDiff-format patch (`old_tree + diff -> new_tree`).
///
/// The upstream crate's `create` mode is pathologically slow on large files
/// and may not produce patches Kuro's own tooling accepts — it exists so the
/// apply pipeline can be tested with realistic data. Not for production use.
pub fn create_krdiff(old_dir: &Path, new_dir: &Path, out_diff: &Path) -> kuro_api::Result<()> {
    let mut patcher = KrDiff::new(
        old_dir.to_string_lossy().into_owned(),
        out_diff.to_string_lossy().into_owned(),
        new_dir.to_string_lossy().into_owned(),
    );
    if !patcher.create() {
        return Err(Error::Patch("krdiff create failed".into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn md5_known_vector() {
        let dir = std::env::temp_dir().join("kuro-patch-md5-test.txt");
        std::fs::write(&dir, b"hello").unwrap();
        assert_eq!(md5_file(&dir).unwrap(), "5d41402abc4b2a76b9719d911017c592");
        let _ = std::fs::remove_file(&dir);
    }
}
