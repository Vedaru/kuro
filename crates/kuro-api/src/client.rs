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
        Ok(body.json::<PatchIndex>().await?)
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
