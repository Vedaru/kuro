//! Sync: verify the whole tree, repair missing/corrupt files, leave good
//! files untouched.

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
    let report = mgr.sync_inner(&full_index, &server, "zip").await.unwrap();

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

    // orphans: an old pakchunk the manifest dropped, plus a stray file
    // in a directory the manifest still uses
    let orphan_chunk = b"OLD-PAKCHUNK-3.5-CONTENT";
    let stray = b"SOMETHING-PGR-PUT-HERE";
    std::fs::write(
        game.join("Client/Content/Paks/pakchunk18-WindowsNoEditor.pak"),
        orphan_chunk,
    )
    .unwrap();
    std::fs::write(game.join("Client/Content/Paks/stray.tmp"), stray).unwrap();

    // protected files the sweep must NOT touch (per the chosen exclusion rules)
    std::fs::write(
        game.join("Client/Content/Paks/inflight.sync.tmp"),
        b"IN-FLIGHT",
    )
    .unwrap();
    std::fs::create_dir_all(game.join("Client/Content/Paks/.incremental_download")).unwrap();
    std::fs::write(
        game.join("Client/Content/Paks/.incremental_download/staged.pak"),
        b"STAGED",
    )
    .unwrap();

    // also: a directory that becomes empty after its only file is removed
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
    let report = mgr.sync_inner(&full_index, &server, "zip").await.unwrap();

    assert_eq!(report.checked, 1, "only manifest files are verified");
    assert_eq!(report.ok, 1);
    assert_eq!(report.repaired, 0);
    assert!(report.failed.is_empty(), "no failures: {report:?}");
    // two true orphans: the dropped pakchunk and old.bin inside the directory
    // that will be pruned. The .sync.tmp and stray.tmp are protected by the
    // .tmp rule and the .incremental_download/ tree is skipped by the walker.
    assert_eq!(
        report.orphans_removed, 2,
        "pakchunk + old.bin removed; .tmp and incr preserved: {report:?}"
    );

    // manifest file still present, untouched
    assert_eq!(std::fs::read(game.join("Client/Content/Paks/fileA.pak")).unwrap(), file_a);

    // orphans gone
    assert!(!game.join("Client/Content/Paks/pakchunk18-WindowsNoEditor.pak").exists());
    assert!(!game.join("Client/Content/Paks/OldLocale/en-US/old.bin").exists());
    // empty directory pruned
    assert!(!game.join("Client/Content/Paks/OldLocale").exists());

    // protected files still present
    assert!(game.join("Client/Content/Paks/inflight.sync.tmp").exists());
    assert!(game.join("Client/Content/Paks/stray.tmp").exists());
    assert!(game.join("Client/Content/Paks/.incremental_download/staged.pak").exists());
    assert!(game.join("launcherDownloadConfig.json").exists());

    let _ = std::fs::remove_dir_all(&base);
}
