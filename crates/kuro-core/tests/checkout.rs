//! Checkout: swap channel-specific files + appId against a (local) target
//! server's API.

mod common;

use std::path::{Path, PathBuf};

use kuro_api::{LauncherIndex, LocalConfig, PatchIndex, ResourceItem, Server};
use kuro_core::GameManager;

fn md5_bytes(data: &[u8]) -> String {
    format!("{:x}", md5::compute(data))
}

fn setup_game(base: &Path) -> PathBuf {
    let game = base.join("game");
    std::fs::create_dir_all(game.join("Client/Binaries/Win64")).unwrap();
    std::fs::create_dir_all(game.join("Client/Content/Paks")).unwrap();
    std::fs::write(game.join("Client/Binaries/Win64/bilibili_sdk.dll"), b"CN-SDK").unwrap();
    std::fs::write(
        game.join("Client/Content/Paks/pakchunk1-Bilibili-Win64-Shipping.pak"),
        b"CN-PAK",
    )
    .unwrap();
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

fn bilibili_index(cdn: &str) -> LauncherIndex {
    // minimal target-server index: cdnList -> our local server, config points
    // at the full index + zip base
    let body = format!(
        r#"{{
  "default": {{
    "cdnList": [{{"P": 1, "K1": 1, "K2": 1, "url": "{cdn}"}}],
    "config": {{
      "version": "3.6.0",
      "indexFile": "indexFile.json",
      "indexFileMd5": "",
      "baseUrl": "zip/",
      "size": 0,
      "patchType": "patch",
      "patchConfig": []
    }},
    "resourcesBasePath": "",
    "version": "3.6.0"
  }}
}}"#
    );
    serde_json::from_str(&body).unwrap()
}

#[tokio::test]
async fn checkout_swaps_diff_files_and_updates_appid() {
    let base = std::env::temp_dir().join(format!("kuro-checkout-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();

    let game = setup_game(&base);

    let bili_login = b"BILIBILI-SDK";
    let bili_pak = b"BILIBILI-PAK";

    // bind the file-serving endpoints first so we learn the address
    let probe = common::spawn_http_server(vec![
        (
            "/indexFile.json".into(),
            serde_json::to_vec(&PatchIndex {
                resource: vec![
                    ResourceItem {
                        dest: "Client/Binaries/Win64/bilibili_sdk.dll".into(),
                        md5: md5_bytes(bili_login),
                        size: bili_login.len() as u64,
                        from_folder: None,
                        chunk_infos: vec![],
                    },
                    ResourceItem {
                        dest: "Client/Content/Paks/pakchunk1-Bilibili-Win64-Shipping.pak".into(),
                        md5: md5_bytes(bili_pak),
                        size: bili_pak.len() as u64,
                        from_folder: None,
                        chunk_infos: vec![],
                    },
                ],
                delete_files: vec![],
                group_infos: vec![],
                apply_types: vec![],
            })
            .unwrap(),
        ),
        (
            "/zip/Client/Binaries/Win64/bilibili_sdk.dll".into(),
            bili_login.to_vec(),
        ),
        (
            "/zip/Client/Content/Paks/pakchunk1-Bilibili-Win64-Shipping.pak".into(),
            bili_pak.to_vec(),
        ),
    ])
    .await;

    // now serve the full set; the index's cdnList points at this server
    let server = common::spawn_http_server(vec![
        ("/index.json".into(), serde_json::to_vec(&bilibili_index(&probe)).unwrap()),
        (
            "/indexFile.json".into(),
            serde_json::to_vec(&PatchIndex {
                resource: vec![
                    ResourceItem {
                        dest: "Client/Binaries/Win64/bilibili_sdk.dll".into(),
                        md5: md5_bytes(bili_login),
                        size: bili_login.len() as u64,
                        from_folder: None,
                        chunk_infos: vec![],
                    },
                    ResourceItem {
                        dest: "Client/Content/Paks/pakchunk1-Bilibili-Win64-Shipping.pak".into(),
                        md5: md5_bytes(bili_pak),
                        size: bili_pak.len() as u64,
                        from_folder: None,
                        chunk_infos: vec![],
                    },
                ],
                delete_files: vec![],
                group_infos: vec![],
                apply_types: vec![],
            })
            .unwrap(),
        ),
        (
            "/zip/Client/Binaries/Win64/bilibili_sdk.dll".into(),
            bili_login.to_vec(),
        ),
        (
            "/zip/Client/Content/Paks/pakchunk1-Bilibili-Win64-Shipping.pak".into(),
            bili_pak.to_vec(),
        ),
    ])
    .await;

    let mgr = GameManager::open(game.clone()).await.unwrap();
    let report = mgr
        .checkout_inner(Server::Bilibili, Some(&format!("{server}/index.json")))
        .await
        .unwrap();

    assert_eq!(report.swapped_files, 2);
    assert_eq!(report.new_version, "3.6.0");

    assert_eq!(
        std::fs::read(game.join("Client/Binaries/Win64/bilibili_sdk.dll")).unwrap(),
        bili_login
    );
    assert_eq!(
        std::fs::read(game.join("Client/Content/Paks/pakchunk1-Bilibili-Win64-Shipping.pak")).unwrap(),
        bili_pak
    );

    let cfg = kuro_core::state::read_local_config(&game).unwrap().unwrap();
    assert_eq!(cfg.app_id, "10004", "appId switched to bilibili");

    let _ = std::fs::remove_dir_all(&base);
}
