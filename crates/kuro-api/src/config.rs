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
                "cn",
                ServerEntry {
                    api_url: "https://TODO/pgr-cn-index.json", // token TBD — see PGR_DISCOVERY.md notes in README
                    app_id: "10012",
                    diff_files: &[],
                },
            ),
        ],
    }
}

/// Verified PGR (战双帕弥什) CN launcher-platform facts, from the official
/// installer (2.6.3.0, 2026-08): `KRPluginConfig.json` + the embedded FE
/// bundle. The index.json *token* is NOT static — the launcher obtains its
/// config URL at runtime from the SDK API below.
pub mod pgr_cn_meta {
    /// Game platform id (launcher API path segment).
    pub const GAME_ID: &str = "G148";
    /// App id (second path segment of index.json).
    pub const APP_ID: &str = "10012";
    /// Package id from the FE bundle's per-game config.
    pub const PKG_ID: &str = "A1472";
    pub const CLIENT_ID: &str = "u43q212j621xjng8aeybtc7f";
    pub const CLIENT_SECRET: &str = "j84ufc2hs4rlfeyn416pjm0s";
    pub const CHANNEL_ID: u32 = 201;
    /// Gamestarter CDN bases (primary + backups), same platform as WuWa.
    pub const CDN_BASES: [&str; 3] = [
        "https://prod-cn-alicdn-gamestarter.kurogame.com",
        "https://prod-volcdn-gamestarter.kurogame.xyz",
        "https://prod-tencentcdn-gamestarter.kurogame.com",
    ];
    /// Runtime config-URL source (Spring Boot; endpoint path unknown yet).
    pub const SDK_API: &str = "https://pc-launcher-sdk-haru-api.kurogames.com";
    /// Official CN PC installer gateway: returns {primary, secondary, version}.
    pub const INSTALLER_JSON: &str =
        "https://download.kurogames.com/pns/official/cn/zh-Hans/pc_app.json";
    /// The launcher info URL shape (token delivered by the SDK API at runtime).
    pub fn index_url(cdn_base: &str, token: &str) -> String {
        format!("{cdn_base}/launcher/game/{GAME_ID}/{APP_ID}_{token}/index.json")
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
