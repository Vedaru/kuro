/// Prints info about a `HDIFF`

use hdiffpatch_rs::patchers::{HDiff, KrDiff};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() != 1 {
        eprintln!("usage: info <diff>");
        std::process::exit(2);
    }
    let path = args[0].clone();
    let hd = HDiff::new(String::new(), path.clone(), String::new());
    match hd.info() {
        Ok(info) => println!("{}", info),
        Err(e) => {
            let kr = KrDiff::new(String::new(), path, String::new());
            match kr.info() {
                Ok(info) => println!("{}", info),
                Err(_) => {
                    eprintln!("not a recognised diff: {}", e);
                    std::process::exit(1);
                }
            }
        }
    }
}
