//! Live proof: kuro managing PGR global (auto-detect -> status -> predownload plan)
use kuro_api::LocalConfig;
use kuro_core::GameManager;

#[tokio::main]
async fn main() {
    let dir = tempfile::Builder::new().prefix("kuro-pgr-proof-").tempdir().unwrap();
    let cfg = LocalConfig::new("4.6.0", "50015"); // PGR global
    std::fs::write(
        dir.path().join("launcherDownloadConfig.json"),
        serde_json::to_string_pretty(&cfg).unwrap(),
    )
    .unwrap();

    match GameManager::open(dir.path().to_path_buf()).await {
        Ok(m) => {
            println!("auto-detect: game={} server={}", m.game, m.server);
            match m.status().await {
                Ok(s) => println!(
                    "status: local={:?} remote={} update={}",
                    s.local_version, s.remote_version, s.update_available
                ),
                Err(e) => println!("status FAILED: {e}"),
            }
            match m.plan_predownload().await {
                Ok(p) => println!(
                    "predownload plan: {} -> {}, {} groups, {} full files, {:.1} GiB",
                    p.from_version,
                    p.to_version,
                    p.patch_groups.len(),
                    p.full_files.len(),
                    p.total_bytes as f64 / (1 << 30) as f64
                ),
                Err(e) => println!("plan FAILED: {e}"),
            }
        }
        Err(e) => println!("open FAILED: {e}"),
    }
}
