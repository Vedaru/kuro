//! `kuro-core` — the game manager.
//!
//! Orchestrates the same operations as `ww-manager`, minus wine:
//!
//! * `status`      — local vs remote version
//! * `predownload` — fetch krpdiff groups + full-file fallbacks
//! * `apply`       — merge (native), verify, atomic swap
//! * `sync`        — full-tree MD5 verify/repair
//! * `checkout`    — server switch (CN <-> Bilibili <-> global)

pub mod atomic;
pub mod download;
pub mod game;
pub mod state;
pub mod steam;

pub use game::{
    ApplyReport, CheckoutReport, GameManager, GameStatus, InstallReport, PendingGroup,
    ProgressEvent, SyncReport,
};
pub use kuro_api::{Error, Game, Result, Server};
pub use steam::{default_game_dir, detect_steam, SteamInfo};
