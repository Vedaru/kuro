mod common;
use common::{run_all, select, skip_no_corpus};

#[test]
fn window_counts_across_meta_ring_boundaries() {
    if skip_no_corpus("window geometry") { return; }
    let vectors = select(|v| v.name.starts_with("wingeom_"));
    let counts: Vec<u64> = vectors.iter().map(|v| v.windows).collect();
    for boundary in [32u64, 64, 65] {
        assert!(
            counts.contains(&boundary),
            "corpus lacks a windowCount={} vector; refill boundaries would go untested (saw {:?})",
            boundary, counts
        );
    }
    run_all("window geometry", &vectors);
}

#[test]
fn step_sizes() {
    run_all("step sizes", &select(|v| v.name.starts_with("winstep_")));
}

#[test]
fn window_sizes() {
    run_all("window sizes", &select(|v| v.name.starts_with("winsize_")));
}

#[test]
fn multi_window_vectors_exist() {
    if skip_no_corpus("multi-window") { return; }
    let multi = select(|v| v.format == "W26" && v.windows > 64);
    assert!(!multi.is_empty(), "no vector exceeds windowMetaCount; ring wrap is untested");
    run_all("multi-window", &multi);
}
