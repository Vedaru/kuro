//! Install-from-zero flow: config written, then full sync pulls the client.

mod common;

use kuro_api::{LocalConfig, PatchIndex, ResourceItem};

fn md5_bytes(data: &[u8]) -> String {
    format!("{:x}", md5::compute(data))
}

fn full_index(file_a: &[u8], file_b: &[u8]) -> PatchIndex {
    PatchIndex {
        resource: vec![
            ResourceItem {
                dest: "Client/a.dat".into(),
                md5: md5_bytes(file_a),
                size: file_a.len() as u64,
                from_folder: None,
                chunk_infos: vec![],
            },
            ResourceItem {
                dest: "Client/b.dat".into(),
                md5: md5_bytes(file_b),
                size: file_b.len() as u64,
                from_folder: None,
                chunk_infos: vec![],
            },
        ],
        delete_files: vec![],
        group_infos: vec![],
        apply_types: vec![],
    }
}

#[tokio::test]
async fn install_flow_writes_config_and_downloads_everything() {
    let base = std::env::temp_dir().join(format!("kuro-install-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let folder = base.join("game");

    let file_a = b"CLIENT-FILE-A";
    let file_b = b"CLIENT-FILE-B";

    // 1. like install(): write the local config for the target version
    std::fs::create_dir_all(&folder).unwrap();
    let cfg = LocalConfig {
        version: "1.0.0".to_string(),
        app_id: "50015".to_string(), // PGR global appId
        group: "default".to_string(),
    };
    std::fs::write(
        folder.join("launcherDownloadConfig.json"),
        serde_json::to_string_pretty(&cfg).unwrap(),
    )
    .unwrap();

    // 2. like install(): sync the full client from the server
    let server = common::spawn_http_server(vec![
        (
            "/indexFile.json".into(),
            serde_json::to_vec(&full_index(file_a, file_b)).unwrap(),
        ),
        ("/zip/Client/a.dat".into(), file_a.to_vec()),
        ("/zip/Client/b.dat".into(), file_b.to_vec()),
    ])
    .await;

    let mgr = kuro_core::GameManager::open(folder.clone()).await.unwrap();
    assert_eq!(format!("{}", mgr.game), "punishing-gray-raven");
    assert_eq!(format!("{}", mgr.server), "global");

    let report = mgr
        .sync_inner(&full_index(file_a, file_b), &server, "zip")
        .await
        .unwrap();
    assert_eq!(report.checked, 2);
    assert_eq!(report.repaired, 2);
    assert!(report.failed.is_empty());

    assert_eq!(std::fs::read(folder.join("Client/a.dat")).unwrap(), file_a);
    assert_eq!(std::fs::read(folder.join("Client/b.dat")).unwrap(), file_b);

    let _ = std::fs::remove_dir_all(&base);
}
