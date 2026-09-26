use std::path::{Path, PathBuf};

use hdiffpatch_rs::patchers::HDiff;

#[derive(Debug, Clone)]
pub struct Vector {
    pub name: String,
    pub kind: String,
    pub format: String,
    pub comp: String,
    pub checksum: String,
    pub windows: u64,
    pub old: String,
    pub new: String,
}

impl Vector {
    pub fn is_file(&self) -> bool { self.kind == "file" }
    pub fn is_dir(&self) -> bool { self.kind == "dir" }
    pub fn is_corrupt(&self) -> bool { self.kind == "corrupt" }
    pub fn targets_tree(&self) -> bool { self.old.ends_with('/') }

    pub fn has_integrity_field(&self) -> bool {
        if self.checksum == "no" {
            return false;
        }
        self.targets_tree() || self.format == "W26"
    }
}

pub fn corpus_configured() -> bool {
    std::env::var("HDIFFPATCH_TV").is_ok()
}

pub fn skip_no_corpus(label: &str) -> bool {
    if corpus_configured() {
        return false;
    }
    eprintln!(
        "SKIPPED {}: HDIFFPATCH_TV is not set.\n  \
         Generate the corpus with ./tests/vectors/generate.sh <dir> <path-to-hdiffz>\n  \
         then re-run with HDIFFPATCH_TV=<dir>",
        label
    );
    true
}

pub fn tv_dir() -> PathBuf {
    let dir = PathBuf::from(std::env::var("HDIFFPATCH_TV").expect("HDIFFPATCH_TV is not set"));
    assert!(dir.is_dir(), "HDIFFPATCH_TV is set but not a directory: {}", dir.display());
    dir
}

pub fn manifest() -> Vec<Vector> {
    let path = tv_dir().join("manifest.tsv");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!("cannot read {}: {}\nregenerate with tests/vectors/generate.sh", path.display(), e)
    });

    let vectors: Vec<Vector> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            let f: Vec<&str> = line.split('\t').collect();
            assert!(f.len() >= 8, "malformed manifest row: {:?}", line);
            Vector {
                name: f[0].into(),
                kind: f[1].into(),
                format: f[2].into(),
                comp: f[3].into(),
                checksum: f[4].into(),
                windows: f[5].parse().unwrap_or(0),
                old: f[6].into(),
                new: f[7].into(),
            }
        })
        .collect();

    assert!(!vectors.is_empty(), "manifest is empty: {}", path.display());
    vectors
}

pub fn select(pred: impl Fn(&Vector) -> bool) -> Vec<Vector> {
    if !corpus_configured() {
        return Vec::new();
    }
    manifest().into_iter().filter(|v| pred(v)).collect()
}

pub fn run_all(label: &str, vectors: &[Vector]) {
    if !corpus_configured() {
        return;
    }
    assert!(!vectors.is_empty(), "{}: no vectors matched; is the corpus stale?", label);

    let mut failures = Vec::new();
    for v in vectors {
        if let Err(e) = run_one(v) {
            failures.push(format!(
                "  {} [{} {} cks={} win={}]: {}",
                v.name, v.format, v.comp, v.checksum, v.windows, e
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{}: {}/{} vectors failed:\n{}",
        label,
        failures.len(),
        vectors.len(),
        failures.join("\n")
    );
    eprintln!("{}: {} vectors ok", label, vectors.len());
}

pub fn run_one(v: &Vector) -> Result<(), String> {
    let data = tv_dir().join("data");
    let diff = data.join(format!("{}.hdiff", v.name));
    if !diff.exists() {
        return Err(format!("missing vector file {}", diff.display()));
    }

    if v.is_corrupt() {
        return run_corrupt(v, &data, &diff);
    }
    if v.targets_tree() {
        return run_dir(v, &data, &diff);
    }
    run_file(v, &data, &diff)
}

fn run_file(v: &Vector, data: &Path, diff: &Path) -> Result<(), String> {
    let out = data.join(format!("{}.out", v.name));
    if !apply(&data.join(&v.old), diff, &out) {
        return Err("apply() returned false".into());
    }
    let got = std::fs::read(&out).map_err(|e| e.to_string())?;
    let want = std::fs::read(data.join(&v.new)).map_err(|e| e.to_string())?;
    if got.len() != want.len() {
        return Err(format!("length mismatch: got {} want {}", got.len(), want.len()));
    }
    if got != want {
        return Err("content mismatch".into());
    }
    Ok(())
}

fn run_dir(v: &Vector, data: &Path, diff: &Path) -> Result<(), String> {
    let out = data.join(format!("{}_out", v.name));
    let _ = std::fs::remove_dir_all(&out);
    if !apply(&data.join(&v.old), diff, &out) {
        return Err("apply() returned false".into());
    }
    compare_trees(&data.join(&v.new), &out)
}

fn run_corrupt(v: &Vector, data: &Path, diff: &Path) -> Result<(), String> {
    let accepted_and_matched = if v.targets_tree() {
        let out = data.join(format!("{}_out", v.name));
        let _ = std::fs::remove_dir_all(&out);
        apply(&data.join(&v.old), diff, &out) && compare_trees(&data.join(&v.new), &out).is_ok()
    } else {
        let out = data.join(format!("{}.out", v.name));
        apply(&data.join(&v.old), diff, &out)
            && std::fs::read(&out).ok() == std::fs::read(data.join(&v.new)).ok()
    };

    if accepted_and_matched {
        return Err("corrupted patch reproduced the expected output".into());
    }
    if v.has_integrity_field() {
        return expect_rejected(v, data, diff);
    }
    Ok(())
}

fn expect_rejected(v: &Vector, data: &Path, diff: &Path) -> Result<(), String> {
    let rejected = if v.targets_tree() {
        let out = data.join(format!("{}_rej", v.name));
        let _ = std::fs::remove_dir_all(&out);
        !apply(&data.join(&v.old), diff, &out)
    } else {
        let out = data.join(format!("{}.rej", v.name));
        !apply(&data.join(&v.old), diff, &out)
    };
    if !rejected {
        return Err("corrupted patch was accepted; checksum verification is not running".into());
    }
    Ok(())
}

fn compare_trees(expected: &Path, actual: &Path) -> Result<(), String> {
    let mut checked = 0usize;
    for entry in walk(expected) {
        let rel = entry.strip_prefix(expected).unwrap();
        let got = std::fs::read(actual.join(rel)).map_err(|e| format!("missing {}: {}", rel.display(), e))?;
        let want = std::fs::read(&entry).map_err(|e| e.to_string())?;
        if got != want {
            return Err(format!("content mismatch for {}", rel.display()));
        }
        checked += 1;
    }
    if checked == 0 {
        return Err("nothing compared".into());
    }
    for entry in walk(actual) {
        let rel = entry.strip_prefix(actual).unwrap();
        if !expected.join(rel).exists() {
            return Err(format!("unexpected extra file {}", rel.display()));
        }
    }
    Ok(())
}

fn apply(old: &Path, diff: &Path, out: &Path) -> bool {
    HDiff::new(
        old.to_string_lossy().into_owned(),
        diff.to_string_lossy().into_owned(),
        out.to_string_lossy().into_owned(),
    )
    .apply()
}

pub fn walk(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(p) = stack.pop() {
        for e in std::fs::read_dir(&p).into_iter().flatten().flatten() {
            let path = e.path();
            if path.is_dir() { stack.push(path); } else { out.push(path); }
        }
    }
    out.sort();
    out
}
