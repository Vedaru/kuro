mod common;
use common::{run_all, select};

#[test]
fn hdiff13() {
    run_all("HDIFF13", &select(|v| v.is_file() && v.format == "H13"));
}

#[test]
fn hdiff_sf20() {
    run_all("HDIFFSF20", &select(|v| v.is_file() && v.format == "SF20"));
}

#[test]
fn hdiff_w26() {
    run_all("HDIFFW26", &select(|v| v.is_file() && v.format == "W26"));
}
