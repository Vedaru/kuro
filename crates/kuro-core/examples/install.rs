//! `kuro install` — from-zero client download for any Kuro game.
//!
//! Usage: `cargo run -p kuro-core --example install -- <game> <server> <folder>`
//!   game:   wuwa | pgr
//!   server: cn | bilibili | global
//!
//! Example: `... install pgr global ~/PGR` (downloads the full PGR global client)

use kuro_core::{Game, GameManager, Server};

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let game = match args.first().map(|s| s.as_str()) {
        Some("wuwa") => Game::WuWa,
        Some("pgr") => Game::Pgr,
        _ => {
            println!("usage: install <wuwa|pgr> <cn|bilibili|global> <folder>");
            return;
        }
    };
    let server = match args.get(1).map(|s| s.as_str()) {
        Some("cn") => Server::Cn,
        Some("bilibili") => Server::Bilibili,
        Some("global") => Server::Global,
        _ => {
            println!("bad server (cn|bilibili|global)");
            return;
        }
    };
    let Some(folder) = args.get(2) else {
        println!("missing folder");
        return;
    };

    match GameManager::install(game, server, folder.into()).await {
        Ok(r) => println!(
            "installed v{}: checked={} ok={} repaired={} failed={}",
            r.version,
            r.sync.checked,
            r.sync.ok,
            r.sync.repaired,
            r.sync.failed.len()
        ),
        Err(e) => println!("install failed: {e}"),
    }
}
