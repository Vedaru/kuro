//! Sync: verify the whole tree, repair missing/corrupt files, leave good files
//! untouched — and sweep only stale build artifacts, never live game state.

mod common;

use std::path::{Path, PathBuf};

use kuro_api::{LocalConfig, PatchIndex, ResourceItem};
use kuro_core::GameManager;

fn md5_bytes(data: &[u8]) -> String {
    format!("{:x}", md5::compute(data))
}

fn res(dest: &str, data: &[u8]) -> ResourceItem {
    ResourceItem {
        dest: dest.to_string(),
        md5: md5_bytes(data),
        size: data.len() as u64,
        from_folder: None,
        chunk_infos: vec![],
    }
}

fn setup_game(base: &Path) -> PathBuf {
    let game = base.join("game");
    std::fs::create_dir_all(game.join("Client/Content/Paks")).unwrap();
    let cfg = LocalConfig {
        version: "3.6.0".to_string(),
        app_id: "10003".to_string(),
        group: "default".to_string(),
    };
    std::fs::write(
        game.join("launcherDownloadConfig.json"),
        serde_json::to_string_pretty(&cfg).unwrap(),
    )
    .unwrap();
    game
}

#[tokio::test]
async fn sync_repairs_missing_and_bad_files() {
    let base = std::env::temp_dir().join(format!("kuro-sync-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();

    let game = setup_game(&base);

    // three files on the server; game has: missing, corrupted, correct
    let file_a = b"FILE-A-CONTENT";
    let file_b = b"FILE-B-CONTENT";
    let file_c = b"FILE-C-CONTENT";

    std::fs::write(game.join("Client/Content/Paks/fileB.pak"), b"CORRUPTED!!").unwrap();
    std::fs::write(game.join("Client/Content/Paks/fileC.pak"), file_c).unwrap();
    // fileA.pak intentionally missing

    let server = common::spawn_http_server(vec![
        ("/zip/Client/Content/Paks/fileA.pak".into(), file_a.to_vec()),
        ("/zip/Client/Content/Paks/fileB.pak".into(), file_b.to_vec()),
        ("/zip/Client/Content/Paks/fileC.pak".into(), file_c.to_vec()),
    ])
    .await;

    let full_index = PatchIndex {
        resource: vec![
            res("Client/Content/Paks/fileA.pak", file_a),
            res("Client/Content/Paks/fileB.pak", file_b),
            res("Client/Content/Paks/fileC.pak", file_c),
        ],
        delete_files: vec![],
        group_infos: vec![],
        apply_types: vec![],
    };

    let mgr = GameManager::open(game.clone()).await.unwrap();
    let report = mgr.sync_inner(&full_index, &[server.as_str()], "zip").await.unwrap();

    assert_eq!(report.checked, 3);
    assert_eq!(report.ok, 1, "only fileC was intact");
    assert_eq!(report.repaired, 2, "A + B repaired");
    assert!(report.failed.is_empty(), "no failures: {report:?}");

    // all three now correct
    assert_eq!(std::fs::read(game.join("Client/Content/Paks/fileA.pak")).unwrap(), file_a);
    assert_eq!(std::fs::read(game.join("Client/Content/Paks/fileB.pak")).unwrap(), file_b);
    assert_eq!(std::fs::read(game.join("Client/Content/Paks/fileC.pak")).unwrap(), file_c);

    let _ = std::fs::remove_dir_all(&base);
}

#[tokio::test]
async fn sync_removes_files_not_in_manifest() {
    let base = std::env::temp_dir().join(format!("kuro-orphan-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();

    let game = setup_game(&base);

    // the manifest knows about fileA.pak only
    let file_a = b"FILE-A-CONTENT";
    std::fs::write(game.join("Client/Content/Paks/fileA.pak"), file_a).unwrap();

    // orphan: an old pakchunk the manifest dropped, in a directory the manifest
    // still ships .pak into — a stale build artifact by the sweep's definition
    let orphan_chunk = b"OLD-PAKCHUNK-3.5-CONTENT";
    std::fs::write(
        game.join("Client/Content/Paks/pakchunk18-WindowsNoEditor.pak"),
        orphan_chunk,
    )
    .unwrap();
    std::fs::write(game.join("Client/Content/Paks/stray.tmp"), b"STRAY").unwrap();

    // protected files the sweep must NOT touch (per the chosen exclusion rules)
    std::fs::write(
        game.join("Client/Content/Paks/inflight.pak.sync.tmp"),
        b"IN-FLIGHT",
    )
    .unwrap();
    std::fs::create_dir_all(game.join("Client/Content/Paks/.incremental_download")).unwrap();
    std::fs::write(
        game.join("Client/Content/Paks/.incremental_download/staged.pak"),
        b"STAGED",
    )
    .unwrap();

    // live game state: not in the manifest, and never this sweep's to delete.
    // The client's own resource channel, its settings/saves/local storage, and
    // the SDK / anti-cheat / crash-reporter files it writes for itself.
    let live = [
        "Client/Saved/Resources/Video/Paks/188_0/Video_188_0-WindowsNoEditor.pak",
        "Client/Saved/Resources/3.6.0/Lang_en/Base/pakchunk10-WindowsNoEditor.pak",
        "Client/Saved/Config/WindowsNoEditor/GameUserSettings.ini",
        "Client/Saved/LocalStorage/LocalStorage.db",
        "Client/Binaries/Win64/KDData-data.db",
        "Client/Binaries/Win64/AntiCheatExpert/pld.dat",
        "Client/Binaries/Win64/CrashSightLog/CrashSight.1.336.log",
    ];
    for rel in live {
        let p = game.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, b"LIVE-GAME-STATE").unwrap();
    }

    // an unknown directory (not in the manifest) with a manifest-looking
    // extension must survive too
    std::fs::create_dir_all(game.join("Client/Content/Paks/OldLocale/en-US")).unwrap();
    std::fs::write(
        game.join("Client/Content/Paks/OldLocale/en-US/old.bin"),
        b"OLD-LOCALE",
    )
    .unwrap();

    let server = common::spawn_http_server(vec![(
        "/zip/Client/Content/Paks/fileA.pak".into(),
        file_a.to_vec(),
    )])
    .await;

    let full_index = PatchIndex {
        resource: vec![res("Client/Content/Paks/fileA.pak", file_a)],
        delete_files: vec![],
        group_infos: vec![],
        apply_types: vec![],
    };

    let mgr = GameManager::open(game.clone()).await.unwrap();
    let report = mgr.sync_inner(&full_index, &[server.as_str()], "zip").await.unwrap();

    assert_eq!(report.checked, 1, "only manifest files are verified");
    assert_eq!(report.ok, 1);
    assert_eq!(report.repaired, 0);
    assert!(report.failed.is_empty(), "no failures: {report:?}");
    // exactly one true orphan: the dropped pakchunk. The .tmp files and the
    // .incremental_download/ tree are excluded by rule, and every file under
    // Client/Saved (plus the SDK/anti-cheat/crash state) is live game state.
    assert_eq!(
        report.orphans_removed, 1,
        "only the stale pakchunk is removed: {report:?}"
    );

    // manifest file still present, untouched
    assert_eq!(std::fs::read(game.join("Client/Content/Paks/fileA.pak")).unwrap(), file_a);

    // the stale artifact is gone
    assert!(!game.join("Client/Content/Paks/pakchunk18-WindowsNoEditor.pak").exists());

    // live state survived
    for rel in live {
        assert!(game.join(rel).exists(), "{rel} must not be swept");
    }
    assert!(game.join("Client/Content/Paks/OldLocale/en-US/old.bin").exists());

    // protected files still present
    assert!(game.join("Client/Content/Paks/inflight.pak.sync.tmp").exists());
    assert!(game.join("Client/Content/Paks/stray.tmp").exists());
    assert!(game.join("Client/Content/Paks/.incremental_download/staged.pak").exists());
    assert!(game.join("launcherDownloadConfig.json").exists());

    let _ = std::fs::remove_dir_all(&base);
}

fn file_mtime_ns(p: &Path) -> u64 {
    std::fs::metadata(p)
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64
}

/// Rewrite the md5 cache entry for `dest` to claim the given (size, mtime)
/// stat — keeping the hash recorded on the first sync, so the entry reads as
/// "this stat is the file that hashed to the manifest md5".
fn patch_cache_entry(game: &Path, dest: &str, size: u64, mtime_ns: u64) {
    let path = kuro_core::state::cache_dir(game).join(kuro_core::state::MD5_CACHE_FILE);
    let mut map: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let entry = map.get_mut(dest).expect("entry recorded by the first sync");
    entry["size"] = size.into();
    entry["mtime_ns"] = mtime_ns.into();
    std::fs::write(&path, serde_json::to_string(&map).unwrap()).unwrap();
}

/// A cache hit must be trusted without reading the file: same-size corruption
/// with the recorded hash pointed at the file's *current* stat passes verify
/// and is never repaired — proving the bytes were never hashed.
#[tokio::test]
async fn md5_cache_trusts_recorded_hash_without_reread() {
    let base = std::env::temp_dir().join(format!("kuro-md5cache-trust-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let game = setup_game(&base);

    let good = b"FILE-C-CONTENT";
    std::fs::write(game.join("Client/Content/Paks/fileC.pak"), good).unwrap();

    let server = common::spawn_http_server(vec![(
        "/zip/Client/Content/Paks/fileC.pak".into(),
        good.to_vec(),
    )])
    .await;
    let full_index = PatchIndex {
        resource: vec![res("Client/Content/Paks/fileC.pak", good)],
        delete_files: vec![],
        group_infos: vec![],
        apply_types: vec![],
    };

    let mgr = GameManager::open(game.clone()).await.unwrap();
    let report = mgr.sync_inner(&full_index, &[server.as_str()], "zip").await.unwrap();
    assert_eq!(report.ok, 1);
    assert_eq!(report.repaired, 0);

    // Corrupt in place with the same size (only a hash can see it), then teach
    // the cache that its recorded hash is current by pointing the entry at the
    // file's new stat. If verify re-read the file it would repair it.
    let corrupt = vec![b'X'; good.len()];
    let p = game.join("Client/Content/Paks/fileC.pak");
    std::fs::write(&p, &corrupt).unwrap();
    patch_cache_entry(
        &game,
        "Client/Content/Paks/fileC.pak",
        corrupt.len() as u64,
        file_mtime_ns(&p),
    );

    let report = mgr.sync_inner(&full_index, &[server.as_str()], "zip").await.unwrap();
    assert!(report.failed.is_empty(), "{report:?}");
    assert_eq!(report.repaired, 0, "cache hit must skip hashing: {report:?}");
    assert_eq!(report.ok, 1);
    assert_eq!(
        std::fs::read(&p).unwrap(),
        corrupt,
        "bytes must have been trusted untouched"
    );

    let _ = std::fs::remove_dir_all(&base);
}

/// A stale cache entry is never trusted: a stat mismatch forces a re-hash,
/// which catches same-size corruption and repairs it.
#[tokio::test]
async fn md5_cache_rehashes_when_stat_is_stale() {
    let base = std::env::temp_dir().join(format!("kuro-md5cache-stale-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let game = setup_game(&base);

    let good = b"FILE-C-CONTENT";
    std::fs::write(game.join("Client/Content/Paks/fileC.pak"), good).unwrap();

    let server = common::spawn_http_server(vec![(
        "/zip/Client/Content/Paks/fileC.pak".into(),
        good.to_vec(),
    )])
    .await;
    let full_index = PatchIndex {
        resource: vec![res("Client/Content/Paks/fileC.pak", good)],
        delete_files: vec![],
        group_infos: vec![],
        apply_types: vec![],
    };

    let mgr = GameManager::open(game.clone()).await.unwrap();
    let report = mgr.sync_inner(&full_index, &[server.as_str()], "zip").await.unwrap();
    assert_eq!(report.ok, 1);

    // Same-size corruption, cache entry left claiming a bogus mtime: the stat
    // mismatch forces the re-hash that catches the corruption.
    let corrupt = vec![b'X'; good.len()];
    let p = game.join("Client/Content/Paks/fileC.pak");
    std::fs::write(&p, &corrupt).unwrap();
    patch_cache_entry(
        &game,
        "Client/Content/Paks/fileC.pak",
        corrupt.len() as u64,
        1,
    );

    let report = mgr.sync_inner(&full_index, &[server.as_str()], "zip").await.unwrap();
    assert!(report.failed.is_empty(), "{report:?}");
    assert_eq!(report.repaired, 1, "stale entry must re-hash and repair: {report:?}");
    assert_eq!(std::fs::read(&p).unwrap(), good.to_vec());

    let _ = std::fs::remove_dir_all(&base);
}
