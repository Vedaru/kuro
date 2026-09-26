mod common;
use common::{run_all, select, skip_no_corpus, tv_dir, walk};

#[test]
fn dir_hdiff13_payload() {
    run_all("dir HDIFF13", &select(|v| v.is_dir() && v.format == "H13"));
}

#[test]
fn dir_sf20_payload() {
    run_all("dir HDIFFSF20", &select(|v| v.is_dir() && v.format == "SF20"));
}

#[test]
fn dir_w26_payload() {
    run_all("dir HDIFFW26", &select(|v| v.is_dir() && v.format == "W26"));
}

#[test]
fn fixture_exercises_same_file_pairs() {
    if skip_no_corpus("dir fixture") { return; }
    let tree = tv_dir().join("data").join("tree");
    let old_root = tree.join("old");
    let new_root = tree.join("new");

    let mut identical = 0;
    let mut empty = 0;
    for entry in walk(&new_root) {
        let rel = entry.strip_prefix(&new_root).unwrap();
        let new_bytes = std::fs::read(&entry).unwrap();
        if new_bytes.is_empty() {
            empty += 1;
        }
        if let Ok(old_bytes) = std::fs::read(old_root.join(rel)) {
            if old_bytes == new_bytes && !new_bytes.is_empty() {
                identical += 1;
            }
        }
    }
    assert!(identical > 0, "fixture has no identical old/new pair; the same-file path never runs");
    assert!(empty > 0, "fixture has no zero-byte output file");
}
