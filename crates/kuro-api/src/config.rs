//! Game / server registry.
//!
//! Kuro ships several titles through the same launcher platform
//! (`prod[-cn]-alicdn-gamestarter.kurogame.com/launcher/game/<GID>/<appId>_<token>/index.json`).
//! Adding a new game is *just* a config entry here — everything else in the
//! pipeline (download, merge, apply, sync) is game-agnostic.

use std::fmt;

/// Known Kuro Games titles with a PC launcher.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Game {
    /// Wuthering Waves (鸣潮)
    WuWa,
    /// Punishing: Gray Raven (战双帕弥什) — endpoints TBD (discovery spike).
    Pgr,
}

impl fmt::Display for Game {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Game::WuWa => write!(f, "wuthering-waves"),
            Game::Pgr => write!(f, "punishing-gray-raven"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Server {
    /// 官服 (Kuro official, mainland)
    Cn,
    /// Bilibili服 (mainland, bilibili channel)
    Bilibili,
    /// International
    Global,
}

impl fmt::Display for Server {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Server::Cn => write!(f, "cn"),
            Server::Bilibili => write!(f, "bilibili"),
            Server::Global => write!(f, "global"),
        }
    }
}

/// One server entry: where to fetch the launcher index + which files differ
/// between channels (used by the server-switch / checkout feature).
pub struct ServerEntry {
    pub api_url: &'static str,
    pub app_id: &'static str,
    /// Files that are channel-specific and must be swapped when switching.
    pub diff_files: &'static [&'static str],
}

pub const WUWA_DIFF_FILES: &[&str] = &[
    "Client/Binaries/Win64/kuro_login.dll",
    "Client/Content/Paks/pakchunk1-Kuro-Win64-Shipping.pak",
];

pub fn servers(game: Game) -> &'static [(&'static str, ServerEntry)] {
    match game {
        Game::WuWa => &[
            (
                "cn",
                ServerEntry {
                    api_url: "https://prod-cn-alicdn-gamestarter.kurogame.com/launcher/game/G152/10003_Y8xXrXk65DqFHEDgApn3cpK5lfczpFx5/index.json",
                    app_id: "10003",
                    diff_files: WUWA_DIFF_FILES,
                },
            ),
            (
                "bilibili",
                ServerEntry {
                    api_url: "https://prod-cn-alicdn-gamestarter.kurogame.com/launcher/game/G152/10004_j5GWFuUFlb8N31Wi2uS3ZAVHcb7ZGN7y/index.json",
                    app_id: "10004",
                    diff_files: &[
                        "Client/Binaries/Win64/bilibili_sdk.dll",
                        "Client/Content/Paks/pakchunk1-Bilibili-Win64-Shipping.pak",
                    ],
                },
            ),
            (
                "global",
                ServerEntry {
                    api_url: "https://prod-alicdn-gamestarter.kurogame.com/launcher/game/G153/50004_obOHXFrFanqsaIEOmuKroCcbZkQRBC7c/index.json",
                    app_id: "50004",
                    diff_files: WUWA_DIFF_FILES,
                },
            ),
        ],
        Game::Pgr => &[
            (
                "global",
                ServerEntry {
                    api_url: "https://prod-alicdn-gamestarter.kurogame.com/launcher/game/G143/50015_LWdk9D2Ep9mpJmqBZZkcPBU2YNraEWBQ/index.json",
                    app_id: "50015",
                    diff_files: &[],
                },
            ),
            (
                "cn",
                ServerEntry {
                    api_url: "https://TODO/pgr-cn-index.json", // CN token TBD (launcher SDK runtime flow)
                    app_id: "10012",
                    diff_files: &[],
                },
            ),
        ],
    }
}

/// Verified PGR (战双帕弥什) launcher-platform facts.
///
/// **Global (G143) is fully wired** — the game manifest at
/// `launcher/game/G143/50015_<token>/index.json` is live and structurally
/// identical to WuWa (patchConfig / krpdiff / weighted cdnList). The token was
/// recovered from the launcher's WebView2 local storage on the user's Windows
/// SSD (2026-08). Current game version: 4.7.0.
///
/// CN (G148) still needs its token (runtime SDK flow; see README).
pub mod pgr_meta {
    /// Global game platform id.
    pub const GAME_ID_GLOBAL: &str = "G143";
    /// Global app id.
    pub const APP_ID_GLOBAL: &str = "50015";
    /// Global launcher token (recovered from real Windows install).
    pub const TOKEN_GLOBAL: &str = "LWdk9D2Ep9mpJmqBZZkcPBU2YNraEWBQ";
    /// CN game platform id.
    pub const GAME_ID_CN: &str = "G148";
    /// CN app id.
    pub const APP_ID_CN: &str = "10012";
    /// Global CDN bases (primary + backups).
    pub const CDN_BASES_GLOBAL: [&str; 3] = [
        "https://prod-alicdn-gamestarter.kurogame.com",
        "https://prod-volcdn-gamestarter.kurogame.net",
        "https://prod-tencentcdn-gamestarter.kurogame.net",
    ];
    /// Global launcher meta URL (launcher self-update manifest).
    pub fn launcher_meta_url(cdn_base: &str) -> String {
        format!(
            "{cdn_base}/launcher/launcher/{APP_ID_GLOBAL}_{TOKEN_GLOBAL}/{GAME_ID_GLOBAL}/index.json"
        )
    }
}

pub fn server_entry(game: Game, server: Server) -> Option<&'static ServerEntry> {
    servers(game)
        .iter()
        .find(|(name, _)| *name == server.to_string())
        .map(|(_, entry)| entry)
}

/// Map an `appId` (from `launcherDownloadConfig.json`) back to game+server.
pub fn game_server_by_app_id(app_id: &str) -> Option<(Game, Server)> {
    for game in [Game::WuWa, Game::Pgr] {
        for (name, entry) in servers(game) {
            if entry.app_id == app_id {
                let server = match *name {
                    "cn" => Server::Cn,
                    "bilibili" => Server::Bilibili,
                    "global" => Server::Global,
                    _ => continue,
                };
                return Some((game, server));
            }
        }
    }
    None
}
