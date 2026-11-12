//! Quality packs: the tier a channel's *manifest* cannot serve, and the two
//! launcher records that make it count as installed.

mod common;

use common::md5_bytes;

use std::path::{Path, PathBuf};

use kuro_api::{LocalConfig, PackBundle, PackCdnNode, PackConfig, PackEntry, PackPatch};
use kuro_core::{pack, quality, GameManager, Quality};

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

    let report = mgr
        .sync_pack_inner(Quality::Hd, &cfg, std::slice::from_ref(&server), true, None)
        .await
        .unwrap();

    assert!(report.broken.is_empty(), "unexpected failures: {:?}", report.broken);
    assert!(report.registered);
    assert_eq!(
        std::fs::read(game.join("Client/Content/HD/pakchunk0-HD-WindowsNoEditor.pak")).unwrap(),
        hd
    );
    // `pack` installs the tier, not `common`: the base client is the
    // game-version path's job, exactly as in the official launcher.
    assert!(
        !game.join("Client/Binaries/Win64/Client-Win64-Shipping.exe").exists(),
        "a tier install wrote a common file"
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
/// `Client/Saved/Resources/<version>/`. They live outside the body and must be
/// fetched by the tier install, with the version directory as the only rule.
#[tokio::test]
async fn optional_pack_resources_are_installed_outside_the_body() {
    let base = tempfile::Builder::new()
        .prefix("kuro-pack-optional-")
        .tempdir()
        .unwrap();
    let game = setup_game(base.path(), "10003");

    let hd = b"HD-PAK".to_vec();
    let hd_pack = b"HD-OPTION-PACK".to_vec();
    let hd_dest = "Client/Saved/Resources/3.8.0/Option_Pack_Map_HD/Base/pakchunk50-HD.pak";

    let server = common::spawn_http_server(vec![
        (
            "/hd/indexFile.json".into(),
            manifest_json(&[
                ("Client/Content/HD/pakchunk0-HD-WindowsNoEditor.pak", &hd),
                (hd_dest, &hd_pack),
            ]),
        ),
        (
            "/zip/Client/Content/HD/pakchunk0-HD-WindowsNoEditor.pak".into(),
            hd.clone(),
        ),
        (format!("/zip/{hd_dest}"), hd_pack.clone()),
    ])
    .await;

    let mgr = GameManager::open(game.clone()).await.unwrap();
    let cfg = config(&server, "3.8.0");
    let report = mgr
        .sync_pack_inner(Quality::Hd, &cfg, std::slice::from_ref(&server), true, None)
        .await
        .unwrap();

    assert!(report.broken.is_empty(), "unexpected failures: {:?}", report.broken);
    assert!(report.registered);
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
        .sync_pack_inner(Quality::Hd, &cfg, std::slice::from_ref(&server), true, None)
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
        .sync_pack_inner(Quality::Hd, &cfg, std::slice::from_ref(&server), false, None)
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
        .sync_pack_inner(Quality::Hd, &cfg, std::slice::from_ref(&server), false, None)
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
        .sync_pack_inner(Quality::Hd, &cfg, std::slice::from_ref(&server), true, None)
        .await
        .unwrap();
    assert_eq!(first.parts[0].downloaded, 1);

    let second = mgr
        .sync_pack_inner(Quality::Hd, &cfg, std::slice::from_ref(&server), true, None)
        .await
        .unwrap();
    assert_eq!(second.parts[0].downloaded, 0, "a verified tier was re-downloaded");
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
        .sync_pack_inner(Quality::Hd, &cfg, std::slice::from_ref(&server), true, None)
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
        .sync_pack_inner(Quality::Hd, &cfg, std::slice::from_ref(&server), true, None)
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
    mgr.sync_pack_inner(Quality::Hd, &cfg, std::slice::from_ref(&server), true, None)
        .await
        .unwrap();

    // The launcher rewrote the marker while the game ran; kuro must leave it be.
    let marker = game.join("launcherDownloadConfig/hd.json");
    let launcher_written = serde_json::json!({ "packName": "hd", "version": "3.7.1" });
    std::fs::write(&marker, serde_json::to_string_pretty(&launcher_written).unwrap()).unwrap();
    let stamp = std::fs::metadata(&marker).unwrap().modified().unwrap();

    let second = mgr
        .sync_pack_inner(Quality::Hd, &cfg, std::slice::from_ref(&server), true, None)
        .await
        .unwrap();
    assert_eq!(second.parts[0].downloaded, 0);
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
        .sync_pack_inner(Quality::Hd, &cfg, std::slice::from_ref(&first_server), true, None)
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
        .sync_pack_inner(Quality::Hd, &cfg, std::slice::from_ref(&second_server), true, None)
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
        .sync_pack_inner(Quality::Hd, &cfg, std::slice::from_ref(&good), true, None)
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
        .sync_pack_inner(Quality::Hd, &cfg, std::slice::from_ref(&empty), true, None)
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
        .sync_pack_inner(Quality::Hd, &cfg, std::slice::from_ref(&server), true, None)
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
        .sync_pack_inner(Quality::Hd, &cfg, std::slice::from_ref(&server), true, None)
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
        .sync_pack_inner(Quality::Hd, &cfg, std::slice::from_ref(&server), true, None)
        .await
        .unwrap();

    assert!(report.registered);
    let pak = game.join("Client/Content/HD/pakchunk1-HD-WindowsNoEditor.pak");
    assert_eq!(std::fs::read(&pak).unwrap(), big, "ranged download reassembled wrong");
    // A rerun trusts the recorded hash instead of re-reading 5 MiB.
    let again = mgr
        .sync_pack_inner(Quality::Hd, &cfg, std::slice::from_ref(&server), true, None)
        .await
        .unwrap();
    assert_eq!(again.parts[0].downloaded, 0);
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
        .sync_pack_inner(Quality::Hd, &cfg, std::slice::from_ref(&server), true, None)
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

fn patch_config(server: &str) -> PackConfig {
    let mut cfg = PackConfig::default();
    cfg.cdn_list.push(PackCdnNode {
        p: 1,
        url: server.to_string(),
        k1: Some(1),
        k2: Some(1),
    });
    for name in ["common", "hd"] {
        cfg.resource_packs.insert(
            name.to_string(),
            PackEntry {
                version: "3.8.0".to_string(),
                patch_config: vec![PackPatch {
                    version: "3.7.1".to_string(),
                    index_file: format!("{name}/patch.json"),
                    base_url: "zip".to_string(),
                    ..Default::default()
                }],
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

/// The official update path: `common` and the active tier each apply their own
/// 3.7.1 → 3.8.0 delta, and only then is the new version recorded. The full
/// 3.8.0 body is never fetched.
#[tokio::test]
async fn pack_update_applies_common_and_tier_deltas_together() {
    let base = tempfile::Builder::new().prefix("kuro-pack-update-").tempdir().unwrap();
    let game = setup_game(base.path(), "10003");

    let common_bin = b"NEW-COMMON-BIN".to_vec();
    let common_pack = b"NEW-COMMON-OPTION-PACK".to_vec();
    let hd_pak = b"NEW-HD-PAK".to_vec();
    let hd_pack = b"NEW-HD-OPTION-PACK".to_vec();

    let common_dest = "Client/Binaries/Win64/Client-Win64-Shipping.exe";
    let common_pack_dest = "Client/Saved/Resources/3.8.0/Option_Pack_Map/Base/pakchunk50.pak";
    let hd_dest = "Client/Content/HD/pakchunk0-HD-WindowsNoEditor.pak";
    let hd_pack_dest = "Client/Saved/Resources/3.8.0/Option_Pack_Map_HD/Base/pakchunk50-HD.pak";

    let server = common::spawn_http_server(vec![
        (
            "/common/patch.json".into(),
            manifest_json(&[(common_dest, &common_bin), (common_pack_dest, &common_pack)]),
        ),
        (
            "/hd/patch.json".into(),
            manifest_json(&[(hd_dest, &hd_pak), (hd_pack_dest, &hd_pack)]),
        ),
        (format!("/zip/{common_dest}"), common_bin.clone()),
        (format!("/zip/{common_pack_dest}"), common_pack.clone()),
        (format!("/zip/{hd_dest}"), hd_pak.clone()),
        (format!("/zip/{hd_pack_dest}"), hd_pack.clone()),
    ])
    .await;

    let mgr = GameManager::open(game.clone()).await.unwrap();
    let cfg = patch_config(&server);
    let report = mgr
        .pack_update_inner(Quality::Hd, &cfg, std::slice::from_ref(&server), None)
        .await
        .unwrap();

    assert_eq!(report.version, "3.8.0");
    assert_eq!(report.parts.len(), 2);
    assert_eq!(report.parts[0].name, "common", "common must apply first");
    assert_eq!(report.parts[1].name, "hd");
    assert_eq!(std::fs::read(game.join(common_dest)).unwrap(), common_bin);
    assert_eq!(std::fs::read(game.join(common_pack_dest)).unwrap(), common_pack);
    assert_eq!(std::fs::read(game.join(hd_dest)).unwrap(), hd_pak);
    assert_eq!(std::fs::read(game.join(hd_pack_dest)).unwrap(), hd_pack);

    let root: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(game.join("launcherDownloadConfig.json")).unwrap()).unwrap();
    assert_eq!(root["version"], "3.8.0");
    assert_eq!(root["bundles"]["HD"]["version"], "3.8.0");
    let marker: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(game.join("launcherDownloadConfig/hd.json")).unwrap()).unwrap();
    assert_eq!(marker["packName"], "hd");
    assert_eq!(marker["version"], "3.8.0");
}

/// No matching source-version delta is a refusal, not a full-body fallback.
#[tokio::test]
async fn pack_update_refuses_without_a_matching_patch() {
    let base = tempfile::Builder::new().prefix("kuro-pack-nopatch-").tempdir().unwrap();
    let game = setup_game(base.path(), "10003");

    let server = common::spawn_http_server(vec![]).await;
    let mut cfg = patch_config(&server);
    cfg.resource_packs.get_mut("hd").unwrap().patch_config.clear();

    let mgr = GameManager::open(game.clone()).await.unwrap();
    let err = mgr
        .pack_update_inner(Quality::Hd, &cfg, std::slice::from_ref(&server), None)
        .await
        .expect_err("a missing patchConfig entry was accepted");
    assert!(err.to_string().contains("no 3.7.1"), "unexpected error: {err}");
}

/// The predownload phase stages the deltas and writes a resumable plan. The
/// game folder is untouched until `pack_apply`, which is what makes the TUI's
/// `d` / `a` pair safe across a maintenance window.
#[tokio::test]
async fn pack_predownload_stages_without_touching_the_game() {
    let base = tempfile::Builder::new()
        .prefix("kuro-pack-predownload-")
        .tempdir()
        .unwrap();
    let game = setup_game(base.path(), "10003");

    let old_bin = b"OLD-COMMON-BIN".to_vec();
    let new_bin = b"NEW-COMMON-BIN".to_vec();
    let new_hd = b"NEW-HD-PAK".to_vec();
    let common_dest = "Client/Binaries/Win64/Client-Win64-Shipping.exe";
    let hd_dest = "Client/Content/HD/pakchunk0-HD-WindowsNoEditor.pak";
    std::fs::create_dir_all(game.join("Client/Binaries/Win64")).unwrap();
    std::fs::write(game.join(common_dest), &old_bin).unwrap();

    let server = common::spawn_http_server(vec![
        ("/common/patch.json".into(), manifest_json(&[(common_dest, &new_bin)])),
        ("/hd/patch.json".into(), manifest_json(&[(hd_dest, &new_hd)])),
        (format!("/zip/{common_dest}"), new_bin.clone()),
        (format!("/zip/{hd_dest}"), new_hd.clone()),
    ])
    .await;

    let mgr = GameManager::open(game.clone()).await.unwrap();
    let cfg = patch_config(&server);
    let plan = mgr
        .pack_download_inner(Quality::Hd, &cfg, std::slice::from_ref(&server), None)
        .await
        .unwrap();
    assert_eq!(plan.parts.len(), 2);
    assert!(!plan.predownload);
    assert_eq!(
        std::fs::read(game.join(common_dest)).unwrap(),
        old_bin,
        "the download touched the game"
    );
    assert!(!game.join(hd_dest).exists(), "the download created a target file");

    let plan_path = game.join(".incremental_download/packs/plan.json");
    let plan_json: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&plan_path).unwrap()).unwrap();
    assert_eq!(plan_json["ready"], true);
    assert!(
        game.join(".incremental_download/packs/common").is_dir(),
        "common staging directory missing"
    );
    assert!(
        game.join(".incremental_download/packs/hd").is_dir(),
        "hd staging directory missing"
    );

    let report = mgr.pack_apply(None).await.unwrap();
    assert_eq!(report.version, "3.8.0");
    assert_eq!(std::fs::read(game.join(common_dest)).unwrap(), new_bin);
    assert_eq!(std::fs::read(game.join(hd_dest)).unwrap(), new_hd);
    assert!(!plan_path.exists(), "the applied plan was left behind");
}

/// Applying with no downloaded plan is an error, not a full-body fallback.
#[tokio::test]
async fn pack_apply_refuses_without_a_predownload() {
    let base = tempfile::Builder::new().prefix("kuro-pack-noapply-").tempdir().unwrap();
    let game = setup_game(base.path(), "10003");
    let mgr = GameManager::open(game.clone()).await.unwrap();
    let err = mgr.pack_apply(None).await.expect_err("an empty apply was accepted");
    assert!(
        err.to_string().contains("no downloaded pack update"),
        "unexpected error: {err}"
    );
}

fn config_with_predownload(server: &str, formal: &str, pre: &str) -> PackConfig {
    let pack = |version: &str, prefix: &str| PackEntry {
        version: version.to_string(),
        patch_config: vec![PackPatch {
            version: "3.7.1".to_string(),
            index_file: format!("{prefix}/patch.json"),
            base_url: "zip".to_string(),
            ..Default::default()
        }],
        ..Default::default()
    };
    let mut cfg = PackConfig::default();
    cfg.cdn_list.push(PackCdnNode {
        p: 1,
        url: server.to_string(),
        k1: Some(1),
        k2: Some(1),
    });
    for name in ["common", "hd"] {
        cfg.resource_packs.insert(name.to_string(), pack(formal, name));
    }
    cfg.bundles.insert(
        "HD".to_string(),
        PackBundle {
            resource_packs: vec!["common".into(), "hd".into()],
        },
    );
    let mut pre_cfg = PackConfig::default();
    for name in ["common", "hd"] {
        pre_cfg
            .resource_packs
            .insert(name.to_string(), pack(pre, &format!("pre-{name}")));
    }
    pre_cfg.bundles.insert(
        "HD".to_string(),
        PackBundle {
            resource_packs: vec!["common".into(), "hd".into()],
        },
    );
    cfg.predownload = Some(Box::new(pre_cfg));
    cfg
}

/// `ww-manager`'s rule: the nested `predownload` block is the next version,
/// and it is consumed only once the formal packs are current. While the formal
/// update is still pending, the formal packs win.
#[tokio::test]
async fn nested_predownload_is_used_only_after_the_formal_update() {
    let base = tempfile::Builder::new()
        .prefix("kuro-pack-nested-pre-")
        .tempdir()
        .unwrap();
    let game = setup_game(base.path(), "10003");
    let server = common::spawn_http_server(vec![
        (
            "/common/patch.json".into(),
            manifest_json(&[("Client/Binaries/Win64/a.dll", b"a")]),
        ),
        (
            "/hd/patch.json".into(),
            manifest_json(&[("Client/Content/HD/a.pak", b"b")]),
        ),
        (
            "/pre-common/patch.json".into(),
            manifest_json(&[("Client/Binaries/Win64/a.dll", b"c")]),
        ),
        (
            "/pre-hd/patch.json".into(),
            manifest_json(&[("Client/Content/HD/a.pak", b"d")]),
        ),
    ])
    .await;
    let mgr = GameManager::open(game.clone()).await.unwrap();

    // Formal 3.7.1 is current, so the nested 3.8.0 block is the target.
    let pre_cfg = config_with_predownload(&server, "3.7.1", "3.8.0");
    let plan = mgr
        .pack_update_plan_inner(Quality::Hd, &pre_cfg, std::slice::from_ref(&server))
        .await
        .unwrap();
    assert!(plan.predownload, "the nested predownload block was ignored");
    assert_eq!(plan.version, "3.8.0");
    assert_eq!(plan.parts.len(), 2);

    // Formal 3.8.0 is pending, so the nested 3.9.0 block is not consumed yet.
    let formal_cfg = config_with_predownload(&server, "3.8.0", "3.9.0");
    let plan = mgr
        .pack_update_plan_inner(Quality::Hd, &formal_cfg, std::slice::from_ref(&server))
        .await
        .unwrap();
    assert!(!plan.predownload, "the formal update was skipped for a preinstall");
    assert_eq!(plan.version, "3.8.0");
    assert_eq!(plan.parts.len(), 2);
}

#[tokio::test]
async fn remove_pack_deletes_the_tier_and_clears_the_records() {
    let base = tempfile::Builder::new().prefix("kuro-pack-remove-").tempdir().unwrap();
    let game = setup_game(base.path(), "10003");
    let tier_dir = game.join("Client/Content/HD");
    std::fs::create_dir_all(&tier_dir).unwrap();
    std::fs::write(tier_dir.join("a.pak"), vec![1u8; 100]).unwrap();
    std::fs::write(tier_dir.join("b.pak"), vec![2u8; 200]).unwrap();

    let marker_dir = game.join("launcherDownloadConfig");
    std::fs::create_dir_all(&marker_dir).unwrap();
    std::fs::write(
        marker_dir.join("hd.json"),
        serde_json::json!({ "packName": "hd", "version": "3.7.1" }).to_string(),
    )
    .unwrap();
    let mut cfg: LocalConfig =
        serde_json::from_str(&std::fs::read_to_string(game.join("launcherDownloadConfig.json")).unwrap()).unwrap();
    cfg.extra.insert(
        "bundles".to_string(),
        serde_json::json!({ "HD": { "version": "3.7.1", "resourcePacks": ["common", "hd"] } }),
    );
    std::fs::write(
        game.join("launcherDownloadConfig.json"),
        serde_json::to_string_pretty(&cfg).unwrap(),
    )
    .unwrap();
    let mut record = pack::PackRecord::default();
    std::fs::create_dir_all(game.join(".kuro_cache")).unwrap();
    record.installed.insert("hd".into(), "3.7.1".into());
    record.files.insert(
        "hd".into(),
        vec!["Client/Content/HD/a.pak".into(), "Client/Content/HD/b.pak".into()],
    );
    std::fs::write(pack::record_path(&game), serde_json::to_string_pretty(&record).unwrap()).unwrap();
    quality::set_selected(&game, Quality::Hd).unwrap();

    let mgr = GameManager::open(game.clone()).await.unwrap();
    let report = mgr.remove_pack(Quality::Hd).await.unwrap();
    assert_eq!(report.files, 2, "{report:?}");
    assert_eq!(report.bytes, 300, "{report:?}");
    assert!(!tier_dir.exists(), "the tier directory survived");
    assert!(!marker_dir.join("hd.json").exists(), "the launcher marker survived");
    let root: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(game.join("launcherDownloadConfig.json")).unwrap()).unwrap();
    assert!(root["bundles"].get("HD").is_none(), "the bundle record survived");
    let record = pack::read_record(&game);
    assert!(!record.installed.contains_key("hd"));
    assert!(!record.files.contains_key("hd"));
    assert_eq!(quality::selected(&game), None, "the removed tier stayed selected");
}
