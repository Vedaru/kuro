//! Checkout: derive the channel delta from the two manifests, swap those
//! files + appId against (local) channel APIs.

mod common;

use common::md5_bytes;

use std::path::{Path, PathBuf};

use kuro_api::{LauncherIndex, LocalConfig, PatchIndex, ResourceItem, Server};
use kuro_core::GameManager;

fn setup_game(base: &Path) -> PathBuf {
    let game = base.join("game");
    std::fs::create_dir_all(game.join("Client/Binaries/Win64")).unwrap();
    std::fs::create_dir_all(game.join("Client/Content/Paks")).unwrap();
    std::fs::write(game.join("Client/Binaries/Win64/cn_sdk.dll"), b"CN-SDK").unwrap();
    std::fs::write(game.join("Client/Content/Paks/shared.pak"), b"SHARED").unwrap();
    let cfg = LocalConfig::new("3.6.0", "10003");
    std::fs::write(
        game.join("launcherDownloadConfig.json"),
        serde_json::to_string_pretty(&cfg).unwrap(),
    )
    .unwrap();
    game
}

fn channel_index(cdn: &str, index_file: &str, version: &str) -> Vec<u8> {
    let body = format!(
        r#"{{
  "default": {{
    "cdnList": [{{"P": 1, "K1": 1, "K2": 1, "url": "{cdn}"}}],
    "config": {{
      "version": "{version}",
      "indexFile": "{index_file}",
      "indexFileMd5": "",
      "baseUrl": "zip/",
      "size": 0,
      "patchType": "patch",
      "patchConfig": []
    }},
    "resourcesBasePath": "",
    "version": "{version}"
  }}
}}"#
    );
    serde_json::to_vec(&serde_json::from_str::<LauncherIndex>(&body).unwrap()).unwrap()
}

fn manifest(files: &[(&str, &[u8])]) -> Vec<u8> {
    let resource = files
        .iter()
        .map(|(dest, body)| ResourceItem {
            dest: (*dest).into(),
            md5: md5_bytes(body),
            size: body.len() as u64,
            from_folder: None,
            chunk_infos: vec![],
        })
        .collect();
    serde_json::to_vec(&PatchIndex {
        resource,
        delete_files: vec![],
        group_infos: vec![],
        apply_types: vec![],
    })
    .unwrap()
}

#[tokio::test]
async fn checkout_swaps_the_manifest_delta_and_updates_appid() {
    let base = tempfile::Builder::new()
        .prefix("kuro-checkout-test-")
        .tempdir()
        .unwrap();
    let game = setup_game(base.path());

    let bili_sdk = b"BILIBILI-SDK";
    let shared = b"SHARED";
    let cn_sdk = b"CN-SDK";

    let source_manifest = manifest(&[
        ("Client/Binaries/Win64/cn_sdk.dll", cn_sdk),
        ("Client/Content/Paks/shared.pak", shared),
    ]);
    let target_manifest = manifest(&[
        ("Client/Binaries/Win64/bilibili_sdk.dll", bili_sdk),
        ("Client/Content/Paks/shared.pak", shared),
    ]);

    // Probe first so the channel indexes can point their cdnList at it.
    let probe = common::spawn_http_server(vec![
        ("/source-manifest.json".into(), source_manifest),
        ("/target-manifest.json".into(), target_manifest),
        ("/zip/Client/Binaries/Win64/bilibili_sdk.dll".into(), bili_sdk.to_vec()),
    ])
    .await;
    let server = common::spawn_http_server(vec![
        (
            "/source.json".into(),
            channel_index(&probe, "source-manifest.json", "3.6.0"),
        ),
        (
            "/target.json".into(),
            channel_index(&probe, "target-manifest.json", "3.6.0"),
        ),
    ])
    .await;

    let mgr = GameManager::open(game.clone()).await.unwrap();
    let report = mgr
        .checkout_inner(
            Server::Bilibili,
            Some(&format!("{server}/target.json")),
            Some(&format!("{server}/source.json")),
        )
        .await
        .unwrap();

    assert_eq!(report.swapped_files, 1);
    assert_eq!(report.removed_files, 1, "the CN-only SDK file must go");
    assert_eq!(report.new_version, "3.6.0");

    assert_eq!(
        std::fs::read(game.join("Client/Binaries/Win64/bilibili_sdk.dll")).unwrap(),
        bili_sdk
    );
    assert!(
        !game.join("Client/Binaries/Win64/cn_sdk.dll").exists(),
        "the source channel's SDK file is still on disk"
    );
    assert_eq!(
        std::fs::read(game.join("Client/Content/Paks/shared.pak")).unwrap(),
        shared,
        "files the channels share must not be touched"
    );

    let cfg = kuro_core::state::read_local_config(&game).unwrap().unwrap();
    assert_eq!(cfg.app_id, "10004", "appId switched to bilibili");
}

#[tokio::test]
async fn checkout_refuses_a_version_skew() {
    let base = tempfile::Builder::new()
        .prefix("kuro-checkout-skew-")
        .tempdir()
        .unwrap();
    let game = setup_game(base.path());
    std::fs::write(game.join("Client/Binaries/Win64/cn_sdk.dll"), b"CN-SDK").unwrap();

    let probe = common::spawn_http_server(vec![(
        "/manifest.json".into(),
        manifest(&[("Client/Binaries/Win64/cn_sdk.dll", b"CN-SDK")]),
    )])
    .await;
    let server = common::spawn_http_server(vec![(
        "/index.json".into(),
        channel_index(&probe, "manifest.json", "3.6.0"),
    )])
    .await;

    // The target advertises 3.7.1 while the install is 3.6.0.
    let target = common::spawn_http_server(vec![(
        "/index.json".into(),
        channel_index(&probe, "manifest.json", "3.7.1"),
    )])
    .await;

    let mgr = GameManager::open(game.clone()).await.unwrap();
    let err = mgr
        .checkout_inner(
            Server::Bilibili,
            Some(&format!("{target}/index.json")),
            Some(&format!("{server}/index.json")),
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("same version"), "unexpected error: {err}");
    assert_eq!(
        std::fs::read(game.join("Client/Binaries/Win64/cn_sdk.dll")).unwrap(),
        b"CN-SDK",
        "a refused checkout must not touch the install"
    );
}
