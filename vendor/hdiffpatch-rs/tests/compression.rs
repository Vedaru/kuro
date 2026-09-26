mod common;
use common::{run_all, select};

fn by_comp(comp: &str) {
    run_all(comp, &select(|v| !v.is_corrupt() && v.comp == comp));
}

#[test]
fn nocomp() { by_comp("no"); }

#[test]
fn zstd() { by_comp("zstd"); }

#[test]
fn zlib() { by_comp("zlib"); }

#[test]
fn bz2() { by_comp("bz2"); }

#[test]
fn lzma() { by_comp("lzma"); }

#[test]
fn lzma2() { by_comp("lzma2"); }
