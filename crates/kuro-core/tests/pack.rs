//! Quality packs: the tier a channel's *manifest* cannot serve, and the two
//! launcher records that make it count as installed.

mod common;

use common::md5_bytes;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use kuro_api::{LocalConfig, PackBundle, PackCdnNode, PackConfig, PackEntry};
use kuro_core::{pack, GameManager, Quality};

fn setup_game(base: &Path, app_id: &str) -> PathBuf {
    let game = base.join("game");
    std::fs::create_dir_all(game.join("Client/Content/Paks")).unwrap();
    let mut cfg = LocalConfig::new("3.7.1", app_id);
    // What the official launcher leaves behind: another key we must not drop.
    cfg.extra.insert(
        "versionFileMd5".to_string(),
        serde_json::Value::String("deadbeef".into()),
    );
    std::fs::write(
        game.join("launcherDownloadConfig.json"),
        serde_json::to_string_pretty(&cfg).unwrap(),
    )
    .unwrap();
    game
}

fn manifest_json(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let resource: Vec<serde_json::Value> = entries
        .iter()
        .map(|(dest, data)| {
            serde_json::json!({
                "dest": dest,
                "md5": md5_bytes(data),
                "size": data.len(),
            })
        })
        .collect();
    serde_json::to_vec(&serde_json::json!({ "resource": resource })).unwrap()
}

fn config(base_url: &str, tier_version: &str) -> PackConfig {
    let mut cfg = PackConfig::default();
    cfg.cdn_list.push(PackCdnNode {
        p: 1,
        url: base_url.to_string(),
        k1: Some(1),
        k2: Some(1),
    });
    for (name, index) in [("common", "common/indexFile.json"), ("hd", "hd/indexFile.json")] {
        cfg.resource_packs.insert(
            name.to_string(),
            PackEntry {
                version: tier_version.to_string(),
                index_file: index.to_string(),
                base_url: "zip".to_string(),
                ..Default::default()
            },
        );
    }
    cfg.bundles.insert(
        "HD".to_string(),
        PackBundle {
            resource_packs: vec!["common".into(), "hd".into()],
        },
    );
    cfg
}

/// A tier install writes the files, then both launcher records — and leaves
/// every key it does not model alone.
#[tokio::test]
async fn installs_a_tier_and_registers_it_with_the_launcher() {
    let base = tempfile::Builder::new().prefix("kuro-pack-install-").tempdir().unwrap();
    let game = setup_game(base.path(), "10003");

    let common = b"CLIENT-BINARY".to_vec();
    let hd = vec![7u8; 9000];

    let server = common::spawn_http_server(vec![
        (
            "/common/indexFile.json".into(),
            manifest_json(&[("Client/Binaries/Win64/Client-Win64-Shipping.exe", &common)]),
        ),
        (
            "/hd/indexFile.json".into(),
            manifest_json(&[("Client/Content/HD/pakchunk0-HD-WindowsNoEditor.pak", &hd)]),
        ),
        (
            "/zip/Client/Binaries/Win64/Client-Win64-Shipping.exe".into(),
            common.clone(),
        ),
        (
            "/zip/Client/Content/HD/pakchunk0-HD-WindowsNoEditor.pak".into(),
            hd.clone(),
        ),
    ])
    .await;

    let mgr = GameManager::open(game.clone()).await.unwrap();
    let cfg = config(&server, "3.7.1");
    let mut channel = HashMap::new();
    channel.insert(
        "client/binaries/win64/client-win64-shipping.exe".to_string(),
        (common.len() as u64, md5_bytes(&common)),
    );

    let report = mgr
        .sync_pack_inner(Quality::Hd, &cfg, std::slice::from_ref(&server), &channel, true, None)
        .await
        .unwrap();

    assert!(report.broken.is_empty(), "unexpected failures: {:?}", report.broken);
    assert!(report.registered);
    assert_eq!(
        std::fs::read(game.join("Client/Content/HD/pakchunk0-HD-WindowsNoEditor.pak")).unwrap(),
        hd
    );
    assert_eq!(
        std::fs::read(game.join("Client/Binaries/Win64/Client-Win64-Shipping.exe")).unwrap(),
        common
    );

    // Both launcher records, or the launcher offers "repair game".
    let root: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(game.join("launcherDownloadConfig.json")).unwrap()).unwrap();
    let bundle = &root["bundles"]["HD"];
    assert_eq!(bundle["version"], "3.7.1");
    assert_eq!(bundle["resourcePacks"], serde_json::json!(["common", "hd"]));
    assert_eq!(root["versionFileMd5"], "deadbeef", "unmodelled key was dropped");
    assert_eq!(root["appId"], "10003");

    let marker: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(game.join("launcherDownloadConfig/hd.json")).unwrap()).unwrap();
    assert_eq!(marker["packName"], "hd");
    assert_eq!(marker["version"], "3.7.1");

    let state = pack::state_of(&game, Quality::Hd);
    assert!(state.registered());
    assert_eq!(state.recorded_version.as_deref(), Some("3.7.1"));
}

/// Since 3.8.0 a tier ships optional map/event packs under
/// `Client/Saved/Resources/<version>/`. They live outside the body, are absent
/// from the channel manifest, and must still be fetched by the tier that owns
/// them: the common copy by `common`, the `_HD` copy by `hd`.
#[tokio::test]
async fn optional_pack_resources_are_installed_outside_the_body() {
    let base = tempfile::Builder::new()
        .prefix("kuro-pack-optional-")
        .tempdir()
        .unwrap();
    let game = setup_game(base.path(), "10003");

    let common_bin = b"CLIENT-BINARY".to_vec();
    let common_pack = b"COMMON-OPTION-PACK".to_vec();
    let hd = b"HD-PAK".to_vec();
    let hd_pack = b"HD-OPTION-PACK".to_vec();

    let common_dest = "Client/Saved/Resources/3.8.0/Option_Pack_Map/Base/pakchunk50.pak";
    let hd_dest = "Client/Saved/Resources/3.8.0/Option_Pack_Map_HD/Base/pakchunk50-HD.pak";

    let server = common::spawn_http_server(vec![
        (
            "/common/indexFile.json".into(),
            manifest_json(&[
                ("Client/Binaries/Win64/Client-Win64-Shipping.exe", &common_bin),
                (common_dest, &common_pack),
            ]),
        ),
        (
            "/hd/indexFile.json".into(),
            manifest_json(&[
                ("Client/Content/HD/pakchunk0-HD-WindowsNoEditor.pak", &hd),
                (hd_dest, &hd_pack),
            ]),
        ),
        (
            "/zip/Client/Binaries/Win64/Client-Win64-Shipping.exe".into(),
            common_bin.clone(),
        ),
        (format!("/zip/{common_dest}"), common_pack.clone()),
        (
            "/zip/Client/Content/HD/pakchunk0-HD-WindowsNoEditor.pak".into(),
            hd.clone(),
        ),
        (format!("/zip/{hd_dest}"), hd_pack.clone()),
    ])
    .await;

    let mgr = GameManager::open(game.clone()).await.unwrap();
    let cfg = config(&server, "3.8.0");
    let mut channel = HashMap::new();
    channel.insert(
        "client/binaries/win64/client-win64-shipping.exe".to_string(),
        (common_bin.len() as u64, md5_bytes(&common_bin)),
    );

    let report = mgr
        .sync_pack_inner(Quality::Hd, &cfg, std::slice::from_ref(&server), &channel, true, None)
        .await
        .unwrap();

    assert!(report.broken.is_empty(), "unexpected failures: {:?}", report.broken);
    assert!(report.registered);
    assert_eq!(
        report.parts[0].skipped, 0,
        "the common optional pack was skipped as a channel difference"
    );
    assert_eq!(std::fs::read(game.join(common_dest)).unwrap(), common_pack);
    assert_eq!(std::fs::read(game.join(hd_dest)).unwrap(), hd_pack);
}

/// A failed download must not leave a record claiming the tier is installed.
#[tokio::test]
async fn failed_download_does_not_register_the_tier() {
    let base = tempfile::Builder::new().prefix("kuro-pack-fail-").tempdir().unwrap();
    let game = setup_game(base.path(), "10003");

    let hd = b"PAK".to_vec();
    // The manifest is served, the file it lists is not: the CDN 404s.
    let server = common::spawn_http_server(vec![
        ("/common/indexFile.json".into(), manifest_json(&[])),
        (
            "/hd/indexFile.json".into(),
            manifest_json(&[("Client/Content/HD/pakchunk0-HD-WindowsNoEditor.pak", &hd)]),
        ),
    ])
    .await;

    let mgr = GameManager::open(game.clone()).await.unwrap();
    let cfg = config(&server, "3.7.1");
    let report = mgr
        .sync_pack_inner(
            Quality::Hd,
            &cfg,
            std::slice::from_ref(&server),
            &HashMap::new(),
            true,
            None,
        )
        .await
        .unwrap();

    assert!(!report.broken.is_empty(), "a 404 must be reported");
    assert!(!report.registered, "an incomplete tier must not be registered");
    assert!(!pack::state_of(&game, Quality::Hd).registered());
    assert!(
        !game.join("launcherDownloadConfig/hd.json").exists(),
        "marker written for an uninstalled tier"
    );
}

/// `common` describes the base client, which the channel also serves. Where the
/// two disagree the channel wins — otherwise installing a tier would replace
/// channel-specific SDK binaries with the other channel's.
#[tokio::test]
async fn common_defers_to_the_channel_where_they_disagree() {
    let base = tempfile::Builder::new().prefix("kuro-pack-common-").tempdir().unwrap();
    let game = setup_game(base.path(), "10004");

    let sdk_pack_doc = b"BILIBILI-SDK-FROM-PACK-DOC".to_vec();
    let channel_sdk = b"THE-CHANNEL-VERSION-OF-IT".to_vec();
    let agreed = b"AGREED-FILE".to_vec();
    let cn_only = b"CN-ONLY-SDK".to_vec();
    let hd_pak = b"HD-PAK".to_vec();
    std::fs::create_dir_all(game.join("Client/Binaries/Win64")).unwrap();
    std::fs::write(game.join("Client/Binaries/Win64/bilibili_sdk.dll"), &channel_sdk).unwrap();

    let server = common::spawn_http_server(vec![
        (
            "/common/indexFile.json".into(),
            manifest_json(&[
                ("Client/Binaries/Win64/bilibili_sdk.dll", &sdk_pack_doc),
                ("Client/Binaries/Win64/shared.dll", &agreed),
                ("Client/Binaries/Win64/thirdparty/krpcsdk_mainland/krsdk.dll", &cn_only),
            ]),
        ),
        (
            "/hd/indexFile.json".into(),
            manifest_json(&[("Client/Content/HD/pakchunk0.pak", &hd_pak)]),
        ),
        (
            "/zip/Client/Binaries/Win64/bilibili_sdk.dll".into(),
            sdk_pack_doc.clone(),
        ),
        ("/zip/Client/Binaries/Win64/shared.dll".into(), agreed.clone()),
        ("/zip/Client/Content/HD/pakchunk0.pak".into(), hd_pak.clone()),
    ])
    .await;

    let mgr = GameManager::open(game.clone()).await.unwrap();
    let cfg = config(&server, "3.7.1");
    let mut channel = HashMap::new();
    channel.insert(
        "client/binaries/win64/bilibili_sdk.dll".to_string(),
        (channel_sdk.len() as u64, md5_bytes(&channel_sdk)),
    );
    channel.insert(
        "client/binaries/win64/shared.dll".to_string(),
        (agreed.len() as u64, md5_bytes(&agreed)),
    );

    let report = mgr
        .sync_pack_inner(Quality::Hd, &cfg, std::slice::from_ref(&server), &channel, true, None)
        .await
        .unwrap();

    assert!(report.broken.is_empty(), "{:?}", report.broken);
    assert_eq!(
        std::fs::read(game.join("Client/Binaries/Win64/bilibili_sdk.dll")).unwrap(),
        channel_sdk,
        "the pack document's copy replaced the channel's SDK binary"
    );
    assert_eq!(
        std::fs::read(game.join("Client/Binaries/Win64/shared.dll")).unwrap(),
        agreed,
        "a file both sides agree on should still be installed"
    );
    assert_eq!(
        report.parts[0].skipped, 2,
        "the differing binary and the entry this channel does not list are both the channel's"
    );
    assert!(
        !game
            .join("Client/Binaries/Win64/thirdparty/krpcsdk_mainland/krsdk.dll")
            .exists(),
        "a CN-only SDK dll was written into a Bilibili install"
    );
}

/// `--check` reports what is missing without downloading or writing records.
#[tokio::test]
async fn check_reports_without_touching_anything() {
    let base = tempfile::Builder::new().prefix("kuro-pack-check-").tempdir().unwrap();
    let game = setup_game(base.path(), "10003");

    let hd = b"PAK".to_vec();
    let server = common::spawn_http_server(vec![
        ("/common/indexFile.json".into(), manifest_json(&[])),
        (
            "/hd/indexFile.json".into(),
            manifest_json(&[("Client/Content/HD/pakchunk0-HD-WindowsNoEditor.pak", &hd)]),
        ),
        (
            "/zip/Client/Content/HD/pakchunk0-HD-WindowsNoEditor.pak".into(),
            hd.clone(),
        ),
    ])
    .await;

    let mgr = GameManager::open(game.clone()).await.unwrap();
    let cfg = config(&server, "3.7.1");
    let report = mgr
        .sync_pack_inner(
            Quality::Hd,
            &cfg,
            std::slice::from_ref(&server),
            &HashMap::new(),
            false,
            None,
        )
        .await
        .unwrap();

    assert_eq!(report.broken.len(), 1);
    assert!(!report.registered);
    assert!(
        !game.join("Client/Content/HD/pakchunk0-HD-WindowsNoEditor.pak").exists(),
        "--check downloaded a file"
    );
    assert!(!game.join("launcherDownloadConfig/hd.json").exists());
}

/// A zero-length manifest entry is a marker file. `--check` must report it as
/// missing without creating it: it used to be created during the scan, so even
/// a read-only run wrote into the install.
#[tokio::test]
async fn check_does_not_create_marker_files() {
    let base = tempfile::Builder::new().prefix("kuro-pack-marker-").tempdir().unwrap();
    let game = setup_game(base.path(), "10003");

    let mut manifest: serde_json::Value = serde_json::from_slice(&manifest_json(&[])).unwrap();
    manifest["resource"].as_array_mut().unwrap().push(serde_json::json!({
        "dest": "Client/Content/HD/.keep",
        "md5": "",
        "size": 0,
    }));

    let server = common::spawn_http_server(vec![
        ("/common/indexFile.json".into(), manifest_json(&[])),
        ("/hd/indexFile.json".into(), serde_json::to_vec(&manifest).unwrap()),
    ])
    .await;

    let mgr = GameManager::open(game.clone()).await.unwrap();
    let cfg = config(&server, "3.7.1");
    let report = mgr
        .sync_pack_inner(
            Quality::Hd,
            &cfg,
            std::slice::from_ref(&server),
            &HashMap::new(),
            false,
            None,
        )
        .await
        .unwrap();

    let marker = game.join("Client/Content/HD/.keep");
    assert_eq!(report.broken, vec!["Client/Content/HD/.keep".to_string()]);
    assert!(!marker.exists(), "--check created a marker file");
}

/// A rerun after a completed install is a no-op, not a re-download.
#[tokio::test]
async fn rerun_is_idempotent() {
    let base = tempfile::Builder::new().prefix("kuro-pack-rerun-").tempdir().unwrap();
    let game = setup_game(base.path(), "10003");

    let hd = vec![3u8; 5000];
    let server = common::spawn_http_server(vec![
        ("/common/indexFile.json".into(), manifest_json(&[])),
        (
            "/hd/indexFile.json".into(),
            manifest_json(&[("Client/Content/HD/pakchunk0-HD-WindowsNoEditor.pak", &hd)]),
        ),
        (
            "/zip/Client/Content/HD/pakchunk0-HD-WindowsNoEditor.pak".into(),
            hd.clone(),
        ),
    ])
    .await;

    let mgr = GameManager::open(game.clone()).await.unwrap();
    let cfg = config(&server, "3.7.1");
    let first = mgr
        .sync_pack_inner(
            Quality::Hd,
            &cfg,
            std::slice::from_ref(&server),
            &HashMap::new(),
            true,
            None,
        )
        .await
        .unwrap();
    assert_eq!(first.parts[1].downloaded, 1);

    let second = mgr
        .sync_pack_inner(
            Quality::Hd,
            &cfg,
            std::slice::from_ref(&server),
            &HashMap::new(),
            true,
            None,
        )
        .await
        .unwrap();
    assert_eq!(second.parts[1].downloaded, 0, "a verified tier was re-downloaded");
    assert!(second.registered);
}

/// The tier pack may not write outside its own body directory. Files are the
/// game's to lose: a manifest that reached `Client/Binaries` from the `hd` pack
/// would overwrite the client binary.
#[tokio::test]
async fn a_tier_pack_cannot_write_outside_its_body() {
    let base = tempfile::Builder::new().prefix("kuro-pack-escape-").tempdir().unwrap();
    let game = setup_game(base.path(), "10003");

    let payload = b"NOT-A-PAK".to_vec();
    let server = common::spawn_http_server(vec![
        ("/common/indexFile.json".into(), manifest_json(&[])),
        (
            "/hd/indexFile.json".into(),
            manifest_json(&[("Client/Binaries/Win64/Client-Win64-Shipping.exe", &payload)]),
        ),
        (
            "/zip/Client/Binaries/Win64/Client-Win64-Shipping.exe".into(),
            payload.clone(),
        ),
    ])
    .await;

    let mgr = GameManager::open(game.clone()).await.unwrap();
    let cfg = config(&server, "3.7.1");
    let err = mgr
        .sync_pack_inner(
            Quality::Hd,
            &cfg,
            std::slice::from_ref(&server),
            &HashMap::new(),
            true,
            None,
        )
        .await
        .unwrap_err();

    assert!(
        err.to_string().contains("manifest entry outside Client/Content/HD"),
        "unexpected error: {err}"
    );
    assert!(!game.join("Client/Binaries/Win64/Client-Win64-Shipping.exe").exists());
}

/// An interrupted install must not leave the launcher believing the tier is
/// complete: the records are withdrawn before the first byte is written, so a
/// failure leaves "not installed" rather than "repair game" with no way out.
#[tokio::test]
async fn a_failed_install_withdraws_the_records_it_cannot_vouch_for() {
    let base = tempfile::Builder::new().prefix("kuro-pack-fail-").tempdir().unwrap();
    let game = setup_game(base.path(), "10003");

    let hd = vec![7u8; 4096];
    let server = common::spawn_http_server(vec![
        ("/common/indexFile.json".into(), manifest_json(&[])),
        (
            "/hd/indexFile.json".into(),
            manifest_json(&[("Client/Content/HD/pakchunk0-HD-WindowsNoEditor.pak", &hd)]),
        ),
    ])
    .await;

    let mgr = GameManager::open(game.clone()).await.unwrap();
    let cfg = config(&server, "3.7.1");
    // The file URL is not registered, so the download fails after the records
    // have already been withdrawn.
    let first = mgr
        .sync_pack_inner(
            Quality::Hd,
            &cfg,
            std::slice::from_ref(&server),
            &HashMap::new(),
            true,
            None,
        )
        .await
        .unwrap();
    assert!(!first.registered);
    assert_eq!(first.broken.len(), 1);
    assert!(
        !pack::state_of(&game, Quality::Hd).registered(),
        "a failed install claimed a complete tier"
    );
}

/// A rerun with nothing to download leaves a healthy registration in place. It
/// must not withdraw and rewrite records the launcher may have touched since.
#[tokio::test]
async fn a_noop_rerun_does_not_disturb_a_healthy_registration() {
    let base = tempfile::Builder::new().prefix("kuro-pack-noop-").tempdir().unwrap();
    let game = setup_game(base.path(), "10003");

    let hd = vec![9u8; 2048];
    let server = common::spawn_http_server(vec![
        ("/common/indexFile.json".into(), manifest_json(&[])),
        (
            "/hd/indexFile.json".into(),
            manifest_json(&[("Client/Content/HD/pakchunk0-HD-WindowsNoEditor.pak", &hd)]),
        ),
        (
            "/zip/Client/Content/HD/pakchunk0-HD-WindowsNoEditor.pak".into(),
            hd.clone(),
        ),
    ])
    .await;

    let mgr = GameManager::open(game.clone()).await.unwrap();
    let cfg = config(&server, "3.7.1");
    mgr.sync_pack_inner(
        Quality::Hd,
        &cfg,
        std::slice::from_ref(&server),
        &HashMap::new(),
        true,
        None,
    )
    .await
    .unwrap();

    // The launcher rewrote the marker while the game ran; kuro must leave it be.
    let marker = game.join("launcherDownloadConfig/hd.json");
    let launcher_written = serde_json::json!({ "packName": "hd", "version": "3.7.1" });
    std::fs::write(&marker, serde_json::to_string_pretty(&launcher_written).unwrap()).unwrap();
    let stamp = std::fs::metadata(&marker).unwrap().modified().unwrap();

    let second = mgr
        .sync_pack_inner(
            Quality::Hd,
            &cfg,
            std::slice::from_ref(&server),
            &HashMap::new(),
            true,
            None,
        )
        .await
        .unwrap();
    assert_eq!(second.parts[1].downloaded, 0);
    assert!(second.registered, "a complete tier must report as registered");
    assert_eq!(
        std::fs::metadata(&marker).unwrap().modified().unwrap(),
        stamp,
        "a no-op rerun rewrote a record it did not need to touch"
    );
}

/// A tier whose pak set shrank must not leave the dropped files behind, but
/// anything the user put there themselves is not ours to delete.
#[tokio::test]
async fn a_shrinking_tier_removes_its_own_stale_files_only() {
    let base = tempfile::Builder::new().prefix("kuro-pack-prune-").tempdir().unwrap();
    let game = setup_game(base.path(), "10003");

    let keep = vec![1u8; 1024];
    let drop = vec![2u8; 1024];
    let first_server = common::spawn_http_server(vec![
        ("/common/indexFile.json".into(), manifest_json(&[])),
        (
            "/hd/indexFile.json".into(),
            manifest_json(&[
                ("Client/Content/HD/pakchunk0-HD-WindowsNoEditor.pak", &keep),
                ("Client/Content/HD/removed-HD-WindowsNoEditor.pak", &drop),
            ]),
        ),
        (
            "/zip/Client/Content/HD/pakchunk0-HD-WindowsNoEditor.pak".into(),
            keep.clone(),
        ),
        (
            "/zip/Client/Content/HD/removed-HD-WindowsNoEditor.pak".into(),
            drop.clone(),
        ),
    ])
    .await;
    let mgr = GameManager::open(game.clone()).await.unwrap();
    let cfg = config(&first_server, "3.7.1");
    let first = mgr
        .sync_pack_inner(
            Quality::Hd,
            &cfg,
            std::slice::from_ref(&first_server),
            &HashMap::new(),
            true,
            None,
        )
        .await
        .unwrap();
    assert!(first.registered);
    let stale = game.join("Client/Content/HD/removed-HD-WindowsNoEditor.pak");
    assert!(stale.is_file());

    // A file the user dropped into the body directory by hand.
    let users_own = game.join("Client/Content/HD/my-own-note.txt");
    std::fs::write(&users_own, b"keep me").unwrap();

    let second_server = common::spawn_http_server(vec![
        ("/common/indexFile.json".into(), manifest_json(&[])),
        (
            "/hd/indexFile.json".into(),
            manifest_json(&[("Client/Content/HD/pakchunk0-HD-WindowsNoEditor.pak", &keep)]),
        ),
    ])
    .await;
    let cfg = config(&second_server, "3.7.2");
    let second = mgr
        .sync_pack_inner(
            Quality::Hd,
            &cfg,
            std::slice::from_ref(&second_server),
            &HashMap::new(),
            true,
            None,
        )
        .await
        .unwrap();
    assert_eq!(second.pruned, 1, "the dropped pak was not removed");
    assert!(!stale.exists());
    assert!(users_own.is_file(), "a file kuro never wrote was deleted");
    assert!(game
        .join("Client/Content/HD/pakchunk0-HD-WindowsNoEditor.pak")
        .is_file());
}

/// A pack document that lists no files is not "this tier is now empty": it is
/// one that failed to publish (or is half-uploaded). Installing it would prune
/// the whole installed tier and then register the empty result as healthy, so
/// it is refused before anything is touched.
#[tokio::test]
async fn an_empty_tier_manifest_does_not_wipe_the_installed_tier() {
    let base = tempfile::Builder::new().prefix("kuro-pack-empty-").tempdir().unwrap();
    let game = setup_game(base.path(), "10003");

    let pak = vec![7u8; 4096];
    let good = common::spawn_http_server(vec![
        ("/common/indexFile.json".into(), manifest_json(&[])),
        (
            "/hd/indexFile.json".into(),
            manifest_json(&[("Client/Content/HD/pakchunk0-HD-WindowsNoEditor.pak", &pak)]),
        ),
        (
            "/zip/Client/Content/HD/pakchunk0-HD-WindowsNoEditor.pak".into(),
            pak.clone(),
        ),
    ])
    .await;
    let mgr = GameManager::open(game.clone()).await.unwrap();
    let cfg = config(&good, "3.7.1");
    let installed = mgr
        .sync_pack_inner(
            Quality::Hd,
            &cfg,
            std::slice::from_ref(&good),
            &HashMap::new(),
            true,
            None,
        )
        .await
        .unwrap();
    assert!(installed.registered);
    let body = game.join("Client/Content/HD/pakchunk0-HD-WindowsNoEditor.pak");
    assert!(body.is_file());

    // The next publish is empty.
    let empty = common::spawn_http_server(vec![
        ("/common/indexFile.json".into(), manifest_json(&[])),
        ("/hd/indexFile.json".into(), manifest_json(&[])),
    ])
    .await;
    let cfg = config(&empty, "3.7.2");
    let err = mgr
        .sync_pack_inner(
            Quality::Hd,
            &cfg,
            std::slice::from_ref(&empty),
            &HashMap::new(),
            true,
            None,
        )
        .await
        .expect_err("an empty tier manifest was accepted");
    assert!(err.to_string().contains("lists no files"), "unexpected error: {err}");

    assert!(body.is_file(), "the installed tier was wiped by an empty manifest");
    assert!(
        game.join("launcherDownloadConfig/hd.json").is_file(),
        "a refused run withdrew the tier's launcher record"
    );
}

/// The launcher can update a tier without kuro ever seeing it, writing its own
/// marker. The downgrade guard must read that too, or kuro later "downgrades" a
/// newer install back to whatever the pack document advertises.
#[tokio::test]
async fn a_launcher_side_update_is_not_downgraded() {
    let base = tempfile::Builder::new()
        .prefix("kuro-pack-downgrade-")
        .tempdir()
        .unwrap();
    let game = setup_game(base.path(), "10003");

    // The launcher updated HD to 3.8.0 and wrote its records.
    let marker_dir = game.join("launcherDownloadConfig");
    std::fs::create_dir_all(&marker_dir).unwrap();
    std::fs::write(
        marker_dir.join("hd.json"),
        serde_json::json!({ "packName": "hd", "version": "3.8.0" }).to_string(),
    )
    .unwrap();
    let mut cfg_on_disk: LocalConfig =
        serde_json::from_str(&std::fs::read_to_string(game.join("launcherDownloadConfig.json")).unwrap()).unwrap();
    cfg_on_disk.extra.insert(
        "bundles".to_string(),
        serde_json::json!({ "HD": { "version": "3.8.0", "packs": ["common", "hd"] } }),
    );
    std::fs::write(
        game.join("launcherDownloadConfig.json"),
        serde_json::to_string_pretty(&cfg_on_disk).unwrap(),
    )
    .unwrap();

    // The pack document still advertises 3.7.1.
    let pak = vec![9u8; 128];
    let server = common::spawn_http_server(vec![
        ("/common/indexFile.json".into(), manifest_json(&[])),
        (
            "/hd/indexFile.json".into(),
            manifest_json(&[("Client/Content/HD/pakchunk0-HD-WindowsNoEditor.pak", &pak)]),
        ),
    ])
    .await;

    let mgr = GameManager::open(game.clone()).await.unwrap();
    let cfg = config(&server, "3.7.1");
    let err = mgr
        .sync_pack_inner(
            Quality::Hd,
            &cfg,
            std::slice::from_ref(&server),
            &HashMap::new(),
            true,
            None,
        )
        .await
        .expect_err("a launcher-side 3.8.0 install was downgraded to 3.7.1");
    assert!(
        err.to_string().contains("refusing to downgrade"),
        "unexpected error: {err}"
    );
    assert!(
        !game.join("Client/Content/HD/pakchunk0-HD-WindowsNoEditor.pak").exists(),
        "the refused run still wrote files"
    );
}

/// A manifest that does not match the digest the channel published for it is
/// refused before anything is written: the alternative is downloading 80 GiB
/// against a file list nobody vouched for.
#[tokio::test]
async fn a_manifest_that_fails_its_published_digest_is_refused() {
    let base = tempfile::Builder::new().prefix("kuro-pack-digest-").tempdir().unwrap();
    let game = setup_game(base.path(), "10003");

    let hd = vec![5u8; 512];
    let server = common::spawn_http_server(vec![
        ("/common/indexFile.json".into(), manifest_json(&[])),
        (
            "/hd/indexFile.json".into(),
            manifest_json(&[("Client/Content/HD/pakchunk0-HD-WindowsNoEditor.pak", &hd)]),
        ),
    ])
    .await;

    let mgr = GameManager::open(game.clone()).await.unwrap();
    let mut cfg = config(&server, "3.7.1");
    cfg.resource_packs.get_mut("hd").unwrap().index_file_md5 = "0".repeat(32);

    let err = mgr
        .sync_pack_inner(
            Quality::Hd,
            &cfg,
            std::slice::from_ref(&server),
            &HashMap::new(),
            true,
            None,
        )
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("but the channel published"),
        "unexpected error: {err}"
    );
    assert!(!game.join("Client/Content/HD/pakchunk0-HD-WindowsNoEditor.pak").exists());
}

/// Packs are ~400 MiB each, so every real pak goes through the parallel-range
/// path. This pins that a ranged download assembles and lands at the manifest's
/// exact MD5, through the same scan/write split the small tests cover.
#[tokio::test]
async fn a_large_pak_is_fetched_as_ranges() {
    let base = tempfile::Builder::new().prefix("kuro-pack-ranges-").tempdir().unwrap();
    let game = setup_game(base.path(), "10003");

    // Larger than the 4 MiB synthetic chunk size, so `chunk_plan` splits it.
    let big: Vec<u8> = (0..(5 * 1024 * 1024)).map(|i| (i % 251) as u8).collect();
    let server = common::spawn_http_server(vec![
        ("/common/indexFile.json".into(), manifest_json(&[])),
        (
            "/hd/indexFile.json".into(),
            manifest_json(&[("Client/Content/HD/pakchunk1-HD-WindowsNoEditor.pak", &big)]),
        ),
        (
            "/zip/Client/Content/HD/pakchunk1-HD-WindowsNoEditor.pak".into(),
            big.clone(),
        ),
    ])
    .await;

    let mgr = GameManager::open(game.clone()).await.unwrap();
    let cfg = config(&server, "3.7.1");
    let report = mgr
        .sync_pack_inner(
            Quality::Hd,
            &cfg,
            std::slice::from_ref(&server),
            &HashMap::new(),
            true,
            None,
        )
        .await
        .unwrap();

    assert!(report.registered);
    let pak = game.join("Client/Content/HD/pakchunk1-HD-WindowsNoEditor.pak");
    assert_eq!(std::fs::read(&pak).unwrap(), big, "ranged download reassembled wrong");
    // A rerun trusts the recorded hash instead of re-reading 5 MiB.
    let again = mgr
        .sync_pack_inner(
            Quality::Hd,
            &cfg,
            std::slice::from_ref(&server),
            &HashMap::new(),
            true,
            None,
        )
        .await
        .unwrap();
    assert_eq!(again.parts[1].downloaded, 0);
}

/// An entry with a size but no digest cannot be verified, so it must not be
/// written on trust — and skipping it alone would report a hole as healthy.
/// The whole tier is refused instead, before anything is downloaded.
#[tokio::test]
async fn an_entry_without_a_digest_refuses_the_tier() {
    let base = tempfile::Builder::new()
        .prefix("kuro-pack-nodigest-")
        .tempdir()
        .unwrap();
    let game = setup_game(base.path(), "10003");

    let real = vec![3u8; 256];
    let mut manifest: serde_json::Value = serde_json::from_slice(&manifest_json(&[(
        "Client/Content/HD/pakchunk0-HD-WindowsNoEditor.pak",
        &real,
    )]))
    .unwrap();
    manifest["resource"].as_array_mut().unwrap().push(serde_json::json!({
        "dest": "Client/Content/HD/unverifiable.pak",
        "md5": "",
        "size": 1024,
    }));

    let server = common::spawn_http_server(vec![
        ("/common/indexFile.json".into(), manifest_json(&[])),
        ("/hd/indexFile.json".into(), serde_json::to_vec(&manifest).unwrap()),
        (
            "/zip/Client/Content/HD/pakchunk0-HD-WindowsNoEditor.pak".into(),
            real.clone(),
        ),
    ])
    .await;

    let mgr = GameManager::open(game.clone()).await.unwrap();
    let cfg = config(&server, "3.7.1");
    let err = mgr
        .sync_pack_inner(
            Quality::Hd,
            &cfg,
            std::slice::from_ref(&server),
            &HashMap::new(),
            true,
            None,
        )
        .await
        .expect_err("a manifest with an unverifiable entry was installed");
    assert!(
        err.to_string().contains("without a valid md5"),
        "unexpected error: {err}"
    );

    assert!(
        !game.join("Client/Content/HD/pakchunk0-HD-WindowsNoEditor.pak").exists(),
        "the refused run still wrote a file"
    );
    assert!(!game.join("Client/Content/HD/unverifiable.pak").exists());
    assert!(
        !game.join("launcherDownloadConfig/hd.json").exists(),
        "the refused run registered the tier"
    );
}
