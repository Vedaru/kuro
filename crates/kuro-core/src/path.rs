//! Manifest-path helpers.
//!
//! Manifest `dest`s are CDN paths: forward slashes, relative to the install
//! root, and in the live files occasionally spelled with a leading `/` or
//! backslashes. Every join, comparison and delete goes through here, so the
//! whole pipeline agrees on one spelling.
//!
//! A path is joined as [`normalise`] spells it, never [`key`]: the folded form
//! exists to compare two manifests' view of one file, and writing to it would
//! create `client/content/hd` beside the real `Client/Content/HD` on a
//! case-sensitive host.

use std::path::{Path, PathBuf};

/// The manifest's spelling of a dest, as a path relative to the game folder.
pub fn normalise(dest: &str) -> String {
    dest.trim_start_matches('/').replace('\\', "/")
}

/// Case-folded form, for comparing two manifests' view of the same file.
pub fn key(dest: &str) -> String {
    normalise(dest).to_ascii_lowercase()
}

/// Split a dest into `(directory, file name)`.
pub fn split_dest(dest: &str) -> (&str, &str) {
    match dest.rsplit_once('/') {
        Some((dir, name)) => (dir, name),
        None => ("", dest),
    }
}

/// Lowercase extension of a file name, if it has a non-empty one.
pub fn extension_of(name: &str) -> Option<String> {
    name.rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .filter(|ext| !ext.is_empty())
}

/// Temp path for an in-flight download, sitting next to its destination.
///
/// Appends instead of replacing the extension: `Path::with_extension` maps
/// `pakchunk0.pak` and `pakchunk0.sig` — siblings the CDN always ships
/// together, and repairs/downloads run several files at once — onto the same
/// `pakchunk0.tmp`, so two concurrent writers can clobber each other's
/// half-written file. The `.tmp` suffix also keeps it protected from the
/// orphan sweep.
pub fn tmp_sibling(path: &Path, tag: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(format!(".{tag}.tmp"));
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_and_absolute_spellings_are_the_same_entry() {
        assert_eq!(normalise("/Client/Content/HD/a.pak"), "Client/Content/HD/a.pak");
        assert_eq!(normalise("Client\\Content\\HD\\a.pak"), "Client/Content/HD/a.pak");
        assert_eq!(key("/Client/Content/HD/a.pak"), "client/content/hd/a.pak");
        assert_eq!(key("Client\\Content\\HD\\a.pak"), "client/content/hd/a.pak");
    }

    #[test]
    fn split_and_extension_handle_names_without_a_directory() {
        assert_eq!(split_dest("Client/a/b.pak"), ("Client/a", "b.pak"));
        assert_eq!(split_dest("b.pak"), ("", "b.pak"));
        assert_eq!(extension_of("b.PAK").as_deref(), Some("pak"));
        assert_eq!(extension_of("noext"), None);
        assert_eq!(extension_of("trailing."), None);
    }

    /// Two files sharing a stem must not share a temp file: repairs run
    /// several files concurrently, and the pak/sig pair is always adjacent.
    #[test]
    fn temp_siblings_are_distinct_per_extension() {
        let pak = Path::new("/g/Client/Content/Paks/pakchunk0.pak");
        let sig = Path::new("/g/Client/Content/Paks/pakchunk0.sig");
        assert_ne!(tmp_sibling(pak, "sync"), tmp_sibling(sig, "sync"));
        assert!(tmp_sibling(pak, "sync")
            .to_string_lossy()
            .ends_with("pakchunk0.pak.sync.tmp"));
    }
}
