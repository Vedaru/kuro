//! `kuro install` — from-zero client download for any Kuro game.
//!
//! Usage: `cargo run -p kuro-core --example install -- <game> <server> <folder> [sd|hd|uhd]`
//!   game:   wuwa | pgr
//!   server: cn | bilibili | global
//!   (optional) quality pack: fetch only that `Client/Content/<TIER>` body —
//!   WuWa only; PGR has no quality packs.
//!
//! Example: `... install pgr global ~/PGR` (downloads the full PGR global client)

use kuro_core::{BodyChoice, Game, GameManager, Quality, Server};

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let game = match args.first().map(|s| s.as_str()) {
        Some("wuwa") => Game::WuWa,
        Some("pgr") => Game::Pgr,
        _ => {
            println!("usage: install <wuwa|pgr> <cn|bilibili|global> <folder> [sd|hd|uhd]");
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
    // Only the body this pack mounts, when one is named (the same choice
    // `kuro install --quality` makes).
    let body = match args.get(3) {
        Some(s) if game.uses_quality_tiers() => match Quality::parse(s) {
            Some(q) => BodyChoice::Only(q),
            None => {
                println!("unknown quality `{s}` (use sd|hd|uhd)");
                return;
            }
        },
        Some(_) => {
            println!("{game} has no quality packs — leave the pack argument off");
            return;
        }
        None => BodyChoice::All,
    };

    match GameManager::install_with_body(game, server, folder.into(), body).await {
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
