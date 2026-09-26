//! Applies a patch: `apply [--threads N] [--budget MiB] <old> <diff> <out>`
//!
//! Works for any supported format (HDIFF13, HDIFF19 directory diffs,
//! HDIFFSF20, HDIFFW26) — the format is detected from the diff header.

use hdiffpatch_rs::patchers::{HDiff, PatchOptions};

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mut opts = PatchOptions::default();

    while args.len() > 1 && args[0].starts_with("--") {
        let flag = args.remove(0);
        let value = args.remove(0);
        match flag.as_str() {
            "--threads" => opts = opts.with_threads(value.parse().expect("--threads N")),
            "--budget" => opts = opts.with_memory_budget(value.parse::<usize>().expect("--budget MiB") << 20),
            other => panic!("unknown flag {}", other),
        }
    }

    if args.len() != 3 {
        eprintln!("usage: apply [--threads N] [--budget MiB] <old> <diff> <out>");
        std::process::exit(2);
    }
    let ok = HDiff::new(args[0].clone(), args[1].clone(), args[2].clone()).with_options(opts).apply();
    if !ok { std::process::exit(1); }
}
