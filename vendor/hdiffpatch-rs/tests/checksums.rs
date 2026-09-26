mod common;
use common::{run_all, select};

fn by_checksum(name: &str) {
    run_all(name, &select(|v| !v.is_corrupt() && v.checksum == name));
}

#[test]
fn default_xxh128() { by_checksum("default"); }

#[test]
fn disabled() { by_checksum("no"); }

#[test]
fn crc32() { by_checksum("crc32"); }

#[test]
fn fadler64() { by_checksum("fadler64"); }

#[test]
fn xxh3() { by_checksum("xxh3"); }

#[test]
fn xxh128() { by_checksum("xxh128"); }

#[test]
fn corrupted_patches_are_rejected() {
    run_all("negative controls", &select(|v| v.is_corrupt()));
}
