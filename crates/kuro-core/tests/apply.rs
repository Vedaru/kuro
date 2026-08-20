//! End-to-end test of the apply pipeline with a synthetic KrDiff patch:
//! merge -> verify -> atomic swap -> delete -> version bump -> cleanup.

use std::path::{Path, PathBuf};

use kuro_api::{FileRef, GroupInfo, LocalConfig, PatchConfig, PatchIndex, ResourceItem};
use kuro_core::GameManager;

fn md5_bytes(data: &[u8]) -> String {
    format!("{:x}", md5::compute(data))
}

fn md5_file(p: &Path) -> String {
    kuro_patch::md5_file(p).unwrap()
}

fn setup_game(base: &Path) -> PathBuf {
    let game = base.join("game");
    let client = game.join("Client");
    std::fs::create_dir_all(client.join("Content/Paks")).unwrap();
    std::fs::create_dir_all(client.join("Binaries/Win64")).unwrap();

    let old_pak = b"PAK-OLD-DATA".repeat(1000);
    std::fs::write(game.join("Client/Content/Paks/pakchunk0.pak"), &old_pak).unwrap();
    std::fs::write(game.join("Client/Binaries/Win64/game.dll"), b"dll-old").unwrap();
    std::fs::write(game.join("config.json"), b"{\"v\":1}").unwrap();
    std::fs::write(game.join("obsolete.txt"), b"bye").unwrap();

    let cfg = LocalConfig {
        version: "0.9.0".to_string(),
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

fn setup_new_tree(base: &Path) -> PathBuf {
    let newtree = base.join("new");
    std::fs::create_dir_all(newtree.join("Client/Content/Paks")).unwrap();
    std::fs::create_dir_all(newtree.join("Client/Binaries/Win64")).unwrap();
    std::fs::write(
        newtree.join("Client/Content/Paks/pakchunk0.pak"),
        b"PAK-NEW-DATA".repeat(1200),
    )
    .unwrap();
    std::fs::write(newtree.join("Client/Binaries/Win64/game.dll"), b"dll-new").unwrap();
    std::fs::write(newtree.join("config.json"), b"{\"v\":2}").unwrap();
    std::fs::write(newtree.join("added.bin"), b"new-file").unwrap();
    newtree
}

fn file_ref(dest: &str, data: &[u8]) -> FileRef {
    FileRef {
        dest: dest.to_string(),
        md5: md5_bytes(data),
        size: data.len() as u64,
        chunk_infos: vec![],
    }
}

#[tokio::test]
async fn apply_merges_verifies_swaps_and_cleans() {
    let base = std::env::temp_dir().join(format!("kuro-apply-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();

    let game = setup_game(&base);
    let newtree = setup_new_tree(&base);

    // real KrDiff between old and new trees
    let diff_path = base.join("test.krpdiff");
    kuro_patch::create_krdiff(&game, &newtree, &diff_path).unwrap();

    // "predownload done": stage the krpdiff into the incremental dir
    let inc = game.join(".incremental_download");
    std::fs::create_dir_all(&inc).unwrap();
    std::fs::copy(&diff_path, inc.join("group_1.krpdiff")).unwrap();

    // synthetic patch index describing the group
    let new_pak = b"PAK-NEW-DATA".repeat(1200);
    let group = GroupInfo {
        dest: "group_1.krpdiff".to_string(),
        src_files: vec![
            file_ref("Client/Content/Paks/pakchunk0.pak", &b"PAK-OLD-DATA".repeat(1000)),
            file_ref("Client/Binaries/Win64/game.dll", b"dll-old"),
            file_ref("config.json", b"{\"v\":1}"),
        ],
        dst_files: vec![
            file_ref("Client/Content/Paks/pakchunk0.pak", &new_pak),
            file_ref("Client/Binaries/Win64/game.dll", b"dll-new"),
            file_ref("config.json", b"{\"v\":2}"),
            file_ref("added.bin", b"new-file"),
        ],
    };
    let patch_index = PatchIndex {
        resource: vec![ResourceItem {
            dest: "group_1.krpdiff".to_string(),
            md5: md5_file(&diff_path),
            size: std::fs::metadata(&diff_path).unwrap().len(),
            from_folder: None,
            chunk_infos: vec![],
        }],
        delete_files: vec!["obsolete.txt".to_string()],
        group_infos: vec![group],
        apply_types: vec![],
    };
    let patch_cfg = PatchConfig {
        version: "0.9.0".to_string(),
        index_file: String::new(),
        base_url: String::new(),
        size: 0,
    };

    let mgr = GameManager::open(game.clone()).await.unwrap();
    let report = mgr
        .apply_inner(&patch_index, "https://cdn.invalid", &patch_cfg, "1.0.0")
        .await
        .unwrap();

    assert_eq!(report.merged, 1, "one group merged");
    assert_eq!(report.swapped, 4, "four files swapped");
    assert_eq!(report.deleted, vec!["obsolete.txt".to_string()]);

    // game tree now matches the new tree
    assert_eq!(
        md5_file(&game.join("Client/Content/Paks/pakchunk0.pak")),
        md5_file(&newtree.join("Client/Content/Paks/pakchunk0.pak"))
    );
    assert_eq!(
        md5_file(&game.join("Client/Binaries/Win64/game.dll")),
        md5_file(&newtree.join("Client/Binaries/Win64/game.dll"))
    );
    assert_eq!(
        md5_file(&game.join("config.json")),
        md5_file(&newtree.join("config.json"))
    );
    assert_eq!(
        md5_file(&game.join("added.bin")),
        md5_file(&newtree.join("added.bin"))
    );
    assert!(!game.join("obsolete.txt").exists(), "deleteFiles removed");

    // version bumped + staging cleaned
    let cfg = kuro_core::state::read_local_config(&game).unwrap().unwrap();
    assert_eq!(cfg.version, "1.0.0");
    assert!(!inc.exists(), "staging cleaned up");

    let _ = std::fs::remove_dir_all(&base);
}

#[tokio::test]
async fn apply_without_predownload_errors_cleanly() {
    let base = std::env::temp_dir().join(format!("kuro-apply-notest-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();

    let game = setup_game(&base);
    let mgr = GameManager::open(game.clone()).await.unwrap();
    let err = mgr
        .apply_inner(
            &PatchIndex {
                resource: vec![],
                delete_files: vec![],
                group_infos: vec![],
                apply_types: vec![],
            },
            "https://cdn.invalid",
            &PatchConfig {
                version: "0.9.0".to_string(),
                index_file: String::new(),
                base_url: String::new(),
                size: 0,
            },
            "1.0.0",
        )
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("predownload"),
        "expected a predownload hint, got: {err}"
    );

    let _ = std::fs::remove_dir_all(&base);
}
