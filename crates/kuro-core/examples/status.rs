//! Smoke test: open a game folder, fetch live status from Kuro's API.
//!
//! Usage: `cargo run -p kuro-core --example status [game-folder]`

use kuro_core::{Game, GameManager};

/// Same fallback the TUI uses, so the example runs on any machine.
fn default_folder() -> String {
    match kuro_core::detect_steam() {
        Some(info) => kuro_core::default_game_dir(&info, Game::WuWa)
            .to_string_lossy()
            .into_owned(),
        None => format!("{}/Games/Wuthering Waves", std::env::var("HOME").unwrap_or_default()),
    }
}

#[tokio::main]
async fn main() {
    let path = std::env::args().nth(1).unwrap_or_else(default_folder);

    match GameManager::open(path.into()).await {
        Ok(mgr) => match mgr.status().await {
            Ok(s) => println!(
                "game={} server={} local={:?} remote={} update_available={}",
                s.game, s.server, s.local_version, s.remote_version, s.update_available
            ),
            Err(e) => println!("status error: {e}"),
        },
        Err(e) => println!("open error: {e}"),
    }
}
