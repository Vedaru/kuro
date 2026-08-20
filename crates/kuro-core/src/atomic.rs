//! Crash-safe file replacement (the `.bak` dance).
//!
//! Mirrors ww-manager's `_safe_replace_file`: old file is moved aside as
//! `.bak`, the new file is moved in, then the backup is removed. If we die
//! mid-swap, the `.bak` allows recovery — nothing is ever half-written.

use std::path::{Path, PathBuf};

use kuro_api::Result;

/// Returns the `.bak` path for a file.
pub fn backup_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".bak");
    PathBuf::from(s)
}

/// Replace `dest` with `src` atomically-ish. `src` must be fully written and
/// verified by the caller before this is called.
pub fn safe_replace(src: &Path, dest: &Path) -> Result<()> {
    let backup = backup_path(dest);

    // 1. move the current file aside (if any)
    if dest.exists() {
        if backup.exists() {
            std::fs::remove_file(&backup)?;
        }
        std::fs::rename(dest, &backup)?;
    }

    // 2. move the new file into place
    std::fs::rename(src, dest)?;

    // 3. drop the backup only after the swap succeeded
    if backup.exists() {
        std::fs::remove_file(&backup)?;
    }
    Ok(())
}

/// Recover from an interrupted swap: if `dest` is missing but `dest.bak`
/// exists, restore it.
pub fn recover_backup(dest: &Path) -> Result<()> {
    let backup = backup_path(dest);
    if !dest.exists() && backup.exists() {
        std::fs::rename(&backup, dest)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn swap_and_recover() {
        let dir = std::env::temp_dir().join("kuro-atomic-test");
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("file.bin");
        let src = dir.join("src.bin");
        std::fs::write(&dest, b"old").unwrap();
        std::fs::write(&src, b"new").unwrap();

        safe_replace(&src, &dest).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"new");
        assert!(!backup_path(&dest).exists());

        // simulate death mid-swap: dest gone, bak present
        std::fs::write(&dest, b"old2").unwrap();
        std::fs::rename(&dest, backup_path(&dest)).unwrap();
        recover_backup(&dest).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), b"old2");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
