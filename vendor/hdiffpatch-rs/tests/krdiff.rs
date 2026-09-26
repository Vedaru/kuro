use std::fs;
use std::path::{Path, PathBuf};

use hdiffpatch_rs::patchers::{DiffFormat, KrDiff};

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("hdiffpatch-rs-kr-{}", name));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn write(root: &Path, rel: &str, data: &[u8]) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, data).unwrap();
}

fn rnd(n: usize, seed: u64) -> Vec<u8> {
    let mut s = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
    (0..n)
        .map(|_| {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (s >> 33) as u8
        })
        .collect()
}

fn walk(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(p) = stack.pop() {
        for e in fs::read_dir(&p).into_iter().flatten().flatten() {
            let path = e.path();
            if path.is_dir() { stack.push(path); } else { out.push(path); }
        }
    }
    out.sort();
    out
}

fn round_trip(name: &str, build: impl Fn(&Path, &Path)) {
    let base = scratch(name);
    let old = base.join("old");
    let new = base.join("new");
    let out = base.join("out");
    fs::create_dir_all(&old).unwrap();
    fs::create_dir_all(&new).unwrap();
    build(&old, &new);

    let diff = base.join("patch.krpdiff");
    let mut maker = KrDiff::new(
        old.to_string_lossy().into_owned(),
        diff.to_string_lossy().into_owned(),
        new.to_string_lossy().into_owned(),
    );
    assert!(maker.create(), "{}: create() failed", name);

    let info = maker.info().unwrap_or_else(|e| panic!("{}: info() failed: {}", name, e));
    assert_eq!(info.format, DiffFormat::KrDiff, "{}", name);
    assert!(info.is_directory, "{}", name);

    let mut patcher = KrDiff::new(
        old.to_string_lossy().into_owned(),
        diff.to_string_lossy().into_owned(),
        out.to_string_lossy().into_owned(),
    );
    assert!(patcher.apply(), "{}: apply() failed", name);

    let mut checked = 0;
    for f in walk(&new) {
        let rel = f.strip_prefix(&new).unwrap();
        let got = fs::read(out.join(rel)).unwrap_or_else(|e| panic!("{}: missing {}: {}", name, rel.display(), e));
        assert!(got == fs::read(&f).unwrap(), "{}: content mismatch for {}", name, rel.display());
        checked += 1;
    }
    assert!(checked > 0, "{}: nothing compared", name);
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn round_trip_single_file() {
    round_trip("single", |old, new| {
        let base = rnd(200_000, 1);
        write(old, "a.bin", &base);
        let mut edited = base.clone();
        edited[50_000..50_500].copy_from_slice(&rnd(500, 2));
        write(new, "a.bin", &edited);
    });
}

#[test]
fn round_trip_nested_dirs() {
    round_trip("nested", |old, new| {
        for (rel, seed) in [("a.bin", 3u64), ("sub/b.bin", 4), ("sub/deep/c.dat", 5)] {
            let base = rnd(120_000, seed);
            write(old, rel, &base);
            let mut edited = base.clone();
            edited[1000..1400].copy_from_slice(&rnd(400, seed + 100));
            write(new, rel, &edited);
        }
    });
}

#[test]
fn round_trip_added_and_removed_files() {
    round_trip("addremove", |old, new| {
        let shared = rnd(80_000, 6);
        write(old, "keep.bin", &shared);
        write(new, "keep.bin", &shared);
        write(old, "gone.bin", &rnd(40_000, 7));
        write(new, "added.bin", &rnd(60_000, 8));
    });
}

#[test]
fn round_trip_identical_trees() {
    round_trip("identical", |old, new| {
        let a = rnd(150_000, 9);
        write(old, "same.bin", &a);
        write(new, "same.bin", &a);
    });
}

#[test]
fn round_trip_new_file_larger() {
    round_trip("grow", |old, new| {
        let base = rnd(90_000, 10);
        write(old, "g.bin", &base);
        let mut bigger = base.clone();
        bigger.extend_from_slice(&rnd(70_000, 11));
        write(new, "g.bin", &bigger);
    });
}

#[test]
fn round_trip_unrelated_content() {
    round_trip("unrelated", |old, new| {
        write(old, "x.bin", &rnd(100_000, 12));
        write(new, "x.bin", &rnd(100_000, 13));
    });
}

/// Real WutheringWaves patches, if present. Parse-only: applying needs the
/// matching game install, which is not part of the corpus.
#[test]
fn real_samples_parse() {
    let dir = PathBuf::from(std::env::var("HOME").unwrap_or_default()).join("Downloads");
    let samples: Vec<PathBuf> = fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().map(|e| e == "krpdiff" || e == "krdiff").unwrap_or(false))
        .collect();

    if samples.is_empty() {
        eprintln!("no .krpdiff samples in {}; skipping", dir.display());
        return;
    }

    for path in samples {
        let kr = KrDiff::new(String::new(), path.to_string_lossy().into_owned(), String::new());
        let info = kr.info().unwrap_or_else(|e| panic!("{}: info() failed: {}", path.display(), e));
        assert_eq!(info.format, DiffFormat::KrDiff);
        assert!(info.is_directory);
        assert!(info.old_size > 0, "{}: old_size", path.display());
        assert!(info.new_size > 0, "{}: new_size", path.display());
        assert!(info.cover_count > 0, "{}: cover_count", path.display());
        assert_eq!(info.compression, "zstd", "{}", path.display());
        assert_eq!(info.checksum, "fadler64", "{}", path.display());
        assert!(info.old_file_count.unwrap_or(0) > 0, "{}", path.display());
        eprintln!("{}: {}", path.file_name().unwrap().to_string_lossy(), info);
    }
}

#[test]
fn created_patch_digest_block_folds_to_new_ref() {
    let base = scratch("digestblock");
    let old = base.join("old");
    let new = base.join("new");
    fs::create_dir_all(&old).unwrap();
    fs::create_dir_all(&new).unwrap();
    for (rel, seed) in [("a.bin", 21u64), ("sub/b.bin", 22), ("sub/c.bin", 23)] {
        write(&old, rel, &rnd(50_000, seed));
        write(&new, rel, &rnd(60_000, seed + 50));
    }

    let diff = base.join("p.krpdiff");
    let mut maker = KrDiff::new(
        old.to_string_lossy().into_owned(),
        diff.to_string_lossy().into_owned(),
        new.to_string_lossy().into_owned(),
    );
    assert!(maker.create());

    let bytes = fs::read(&diff).unwrap();
    let mut i = 0usize;
    let mut take_delim = |d: u8, i: &mut usize| {
        let start = *i;
        while bytes[*i] != d { *i += 1; }
        let s = String::from_utf8_lossy(&bytes[start..*i]).into_owned();
        *i += 1;
        s
    };
    assert_eq!(take_delim(b'&', &mut i), "HDIFF19");
    take_delim(b'&', &mut i);
    assert_eq!(take_delim(0, &mut i), "fadler64");
    i += 2;

    let mut rd_var = |i: &mut usize| -> u64 {
        let b = bytes[*i]; *i += 1;
        let mut v = (b & 0x7F) as u64;
        if b & 0x80 != 0 {
            loop {
                let c = bytes[*i]; *i += 1;
                v = (v << 7) | (c & 0x7F) as u64;
                if c & 0x80 == 0 { break; }
            }
        }
        v
    };
    let mut fields = [0u64; 17];
    for f in fields.iter_mut() { *f = rd_var(&mut i); }
    let cks_bytes = fields[16] as usize;
    assert_eq!(cks_bytes, 8);

    let read_u64 = |off: usize| {
        let mut v = [0u8; 8];
        v.copy_from_slice(&bytes[off..off + 8]);
        u64::from_le_bytes(v)
    };
    let new_ref = read_u64(i + 8);
    let same = read_u64(i + 16);

    assert_ne!(new_ref, 0, "newRef digest must be populated");
    assert_eq!(same, 1, "sameFile digest must be ADLER_INITIAL with no same pairs");
    assert_eq!(fields[7], 180_000, "new_ref_size");

    let _ = fs::remove_dir_all(&base);
}

#[test]
fn corrupted_patch_is_rejected_by_file_digest() {
    let base = scratch("corrupt");
    let old = base.join("old");
    let new = base.join("new");
    let out = base.join("out");
    fs::create_dir_all(&old).unwrap();
    fs::create_dir_all(&new).unwrap();
    let a = rnd(150_000, 31);
    write(&old, "a.bin", &a);
    let mut b = a.clone();
    b[70_000..70_800].copy_from_slice(&rnd(800, 32));
    write(&new, "a.bin", &b);

    let diff = base.join("p.krpdiff");
    let mut maker = KrDiff::new(
        old.to_string_lossy().into_owned(),
        diff.to_string_lossy().into_owned(),
        new.to_string_lossy().into_owned(),
    );
    assert!(maker.create());

    let mut bytes = fs::read(&diff).unwrap();
    let n = bytes.len();
    bytes[n - 64] ^= 0xFF;
    fs::write(&diff, &bytes).unwrap();

    let mut patcher = KrDiff::new(
        old.to_string_lossy().into_owned(),
        diff.to_string_lossy().into_owned(),
        out.to_string_lossy().into_owned(),
    );
    assert!(!patcher.apply(), "corrupted patch accepted; per-file digest check is not running");

    let _ = fs::remove_dir_all(&base);
}
