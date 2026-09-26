mod common;
use common::{select, skip_no_corpus, tv_dir, walk, Vector};

use hdiffpatch_rs::patchers::{HDiff, PatchOptions};

fn apply_with(v: &Vector, opts: PatchOptions, tag: &str) -> Option<std::path::PathBuf> {
    let data = tv_dir().join("data");
    let diff = data.join(format!("{}.hdiff", v.name));
    let out = if v.targets_tree() {
        let p = data.join(format!("{}_{}_mt", v.name, tag));
        let _ = std::fs::remove_dir_all(&p);
        p
    } else {
        data.join(format!("{}.{}.mt", v.name, tag))
    };
    let ok = HDiff::new(
        data.join(&v.old).to_string_lossy().into_owned(),
        diff.to_string_lossy().into_owned(),
        out.to_string_lossy().into_owned(),
    )
    .with_options(opts)
    .apply();
    if ok { Some(out) } else { None }
}

fn digest(path: &std::path::Path) -> Vec<(String, Vec<u8>)> {
    if path.is_dir() {
        walk(path)
            .into_iter()
            .map(|p| {
                let rel = p.strip_prefix(path).unwrap().to_string_lossy().into_owned();
                (rel, std::fs::read(&p).unwrap())
            })
            .collect()
    } else {
        vec![(String::new(), std::fs::read(path).unwrap())]
    }
}

fn compare_thread_counts(label: &str, vectors: &[Vector]) {
    if skip_no_corpus(label) { return; }
    assert!(!vectors.is_empty(), "{}: no vectors matched", label);
    let mut failures = Vec::new();

    for v in vectors {
        let single = apply_with(v, PatchOptions::single_threaded(), &format!("{}_st", label));
        let multi = apply_with(v, PatchOptions::default().with_threads(8), &format!("{}_mt", label));
        match (single, multi) {
            (Some(a), Some(b)) => {
                if digest(&a) != digest(&b) {
                    failures.push(format!("  {}: single- and multi-threaded output differ", v.name));
                }
            }
            (a, b) => failures.push(format!(
                "  {}: apply mismatch (single ok={} multi ok={})",
                v.name,
                a.is_some(),
                b.is_some()
            )),
        }
    }

    assert!(
        failures.is_empty(),
        "{}: {}/{} differed:\n{}",
        label,
        failures.len(),
        vectors.len(),
        failures.join("\n")
    );
    eprintln!("{}: {} vectors identical across thread counts", label, vectors.len());
}

#[test]
fn w26_identical_across_thread_counts() {
    compare_thread_counts("W26", &select(|v| !v.is_corrupt() && v.format == "W26"));
}

#[test]
fn sf20_identical_across_thread_counts() {
    compare_thread_counts("SF20", &select(|v| !v.is_corrupt() && v.format == "SF20"));
}

#[test]
fn hdiff13_identical_across_thread_counts() {
    compare_thread_counts("H13", &select(|v| !v.is_corrupt() && v.format == "H13"));
}

#[test]
fn tiny_memory_budget_still_correct() {
    if skip_no_corpus("tight budget") { return; }
    let vectors = select(|v| !v.is_corrupt() && v.format == "W26" && v.windows > 32);
    assert!(!vectors.is_empty(), "no multi-window vectors");
    let mut failures = Vec::new();
    for v in &vectors {
        let base = apply_with(v, PatchOptions::single_threaded(), "budget_base");
        let tight = apply_with(v, PatchOptions::default().with_threads(8).with_memory_budget(64 * 1024), "budget_tight");
        match (base, tight) {
            (Some(a), Some(b)) if digest(&a) == digest(&b) => {}
            _ => failures.push(format!("  {}: tight budget changed the result", v.name)),
        }
    }
    assert!(failures.is_empty(), "tight budget:\n{}", failures.join("\n"));
}
