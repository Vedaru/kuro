//! Smoke test: open a game folder, fetch live status from Kuro's API.
//!
//! Usage: `cargo run -p kuro-core --example status [game-folder]`

use kuro_core::GameManager;

#[tokio::main]
async fn main() {
    let path = std::env::args().nth(1).unwrap_or_else(|| {
        "/home/vedaru/.local/share/Steam/steamapps/common/Wuthering Waves".to_string()
    });

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
