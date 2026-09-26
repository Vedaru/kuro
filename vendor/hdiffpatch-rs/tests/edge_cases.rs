mod common;
use common::{run_all, select, skip_no_corpus};

#[test]
fn empty_old_file() {
    run_all("empty old", &select(|v| v.name.starts_with("edge_empty_")));
}

#[test]
fn single_byte_files() {
    run_all("one byte", &select(|v| v.name.starts_with("edge_one_")));
}

#[test]
fn identical_old_and_new() {
    run_all("identical", &select(|v| v.name.starts_with("edge_same_")));
}

#[test]
fn empty_new_file() {
    run_all("empty new", &select(|v| v.name.starts_with("edge_shrink_")));
}

#[test]
fn extra_data_prefix() {
    if skip_no_corpus("extraData") { return; }
    let vectors = select(|v| v.name == "w26_extradata");
    assert!(
        !vectors.is_empty(),
        "no extraDataSize>0 vector; that skip path in PatchW::patch is untested"
    );
    run_all("extraData", &vectors);
}
