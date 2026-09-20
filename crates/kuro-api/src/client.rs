//! HTTP client for the Kuro launcher API.

use crate::error::{Error, Result};
use crate::types::{CdnNode, LauncherIndex, PatchConfig, PatchIndex};
use std::time::{SystemTime, UNIX_EPOCH};

/// Thin wrapper over `reqwest` with the Kuro-specific URL builders.
#[derive(Debug, Clone)]
pub struct ApiClient {
    http: reqwest::Client,
}

impl ApiClient {
    pub fn new() -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent("kuro/0.1 (+https://github.com/vedaru/kuro)")
            .build()?;
        Ok(Self { http })
    }

    /// Fetch the launcher entry point (`index.json`).
    pub async fn fetch_index(&self, api_url: &str) -> Result<LauncherIndex> {
        let body = self.http.get(api_url).send().await?.error_for_status()?;
        Ok(body.json::<LauncherIndex>().await?)
    }

    /// Pick a CDN node from `cdnList`, weighted by `P` (0 = excluded).
    pub fn pick_cdn<'a>(&self, index: &'a LauncherIndex) -> Result<&'a CdnNode> {
        let nodes: Vec<&CdnNode> = index.default.cdn_list.iter().filter(|n| n.p > 0).collect();
        if nodes.is_empty() {
            return Err(Error::NoCdnNode);
        }
        let total: u64 = nodes.iter().map(|n| n.p).sum();
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0);
        let mut roll = nanos % total;
        for node in &nodes {
            if roll < node.p {
                return Ok(node);
            }
            roll -= node.p;
        }
        Ok(nodes[nodes.len() - 1])
    }

    /// Fetch the patch manifest for one source version.
    pub async fn fetch_patch_index(&self, cdn_base: &str, patch: &PatchConfig) -> Result<PatchIndex> {
        let url = format!("{}/{}", cdn_base.trim_end_matches('/'), patch.index_file.trim_start_matches('/'));
        let body = self.http.get(&url).send().await?.error_for_status()?;
        let index: PatchIndex = body.json().await?;
        validate_manifest_paths(&index)?;
        Ok(index)
    }

    /// Absolute URL of a krpdiff file for the given patch config.
    pub fn krpdiff_url(cdn_base: &str, patch: &PatchConfig, name: &str) -> String {
        format!(
            "{}/{}/{}",
            cdn_base.trim_end_matches('/'),
            patch.base_url.trim_matches('/'),
            name
        )
    }

    /// Absolute URL of a full-file fallback resource (`fromFolder` + `dest`).
    pub fn resource_url(cdn_base: &str, from_folder: &str, dest: &str) -> String {
        format!(
            "{}/{}/{}",
            cdn_base.trim_end_matches('/'),
            from_folder.trim_matches('/'),
            dest.trim_start_matches('/')
        )
    }
}

/// Reject manifest entries that would escape the install root.
///
/// Every consumer joins these strings straight onto the game folder — writes,
/// atomic swaps, deletes and the orphan sweep's comparisons — so a corrupted or
/// tampered manifest could otherwise steer an operation outside the install.
/// Absolute paths, drive-qualified paths and `..` components are refused at the
/// single point where a remote manifest enters the process.
pub fn validate_manifest_paths(index: &PatchIndex) -> Result<()> {
    let check = |dest: &str| -> Result<()> {
        if is_unsafe_manifest_path(dest) {
            return Err(Error::UnsafePath(dest.to_string()));
        }
        Ok(())
    };

    for r in &index.resource {
        check(&r.dest)?;
        if let Some(from) = &r.from_folder {
            check(from)?;
        }
    }
    for f in &index.delete_files {
        check(f)?;
    }
    for g in &index.group_infos {
        check(&g.dest)?;
        for f in g.src_files.iter().chain(g.dst_files.iter()) {
            check(&f.dest)?;
        }
    }
    Ok(())
}

fn is_unsafe_manifest_path(dest: &str) -> bool {
    // `\\server\share` — a Windows UNC path, never how the CDN writes a dest.
    if dest.starts_with("\\\\") {
        return true;
    }
    // A leading `/` or `\` is how manifests spell "relative to the install
    // root"; strip it, then make sure nothing above the root is left.
    let cleaned = dest.trim_start_matches(['/', '\\']);
    if cleaned.trim().is_empty() {
        return true;
    }
    // Windows drive-relative (`C:foo`, `C:\foo`).
    if cleaned.as_bytes().get(1) == Some(&b':') {
        return true;
    }
    cleaned.replace('\\', "/").split('/').any(|c| c == "..")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{FileRef, GroupInfo, ResourceItem};

    fn res(dest: &str) -> ResourceItem {
        ResourceItem {
            dest: dest.to_string(),
            md5: String::new(),
            size: 0,
            from_folder: None,
            chunk_infos: vec![],
        }
    }

    fn index(resource: Vec<ResourceItem>, delete_files: Vec<String>) -> PatchIndex {
        PatchIndex {
            resource,
            delete_files,
            group_infos: vec![],
            apply_types: vec![],
        }
    }

    #[test]
    fn accepts_relative_manifest_paths() {
        let idx = index(
            vec![res("Client/Content/Paks/pakchunk0.pak"), res("/zip/a.bin")],
            vec!["Client/old.txt".to_string()],
        );
        assert!(validate_manifest_paths(&idx).is_ok());
    }

    #[test]
    fn rejects_escaping_paths() {
        for bad in [
            "../outside.bin",
            "Client/../../outside.bin",
            "C:temp\\evil.dll",
            "\\\\server\\share\\evil.dll",
            "..",
            "",
        ] {
            let idx = index(vec![res(bad)], vec![]);
            assert!(
                matches!(validate_manifest_paths(&idx), Err(Error::UnsafePath(_))),
                "should reject {bad:?}"
            );
        }
    }

    #[test]
    fn rejects_escaping_delete_and_group_paths() {
        let idx = index(vec![], vec!["../../important".to_string()]);
        assert!(matches!(
            validate_manifest_paths(&idx),
            Err(Error::UnsafePath(_))
        ));

        let mut idx = index(vec![], vec![]);
        idx.group_infos = vec![GroupInfo {
            dest: "group/patch.krpdiff".to_string(),
            src_files: vec![],
            dst_files: vec![FileRef {
                dest: "/etc/passwd".to_string(),
                md5: String::new(),
                size: 0,
                chunk_infos: vec![],
            }],
        }];
        assert!(validate_manifest_paths(&idx).is_ok(), "leading / is stripped, not flagged");
    }
}
