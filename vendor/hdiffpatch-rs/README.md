# hdiffpatch-rs

Rust library for applying HDiffPatch-style patches, with a small compatibility layer for the `KrDiff` format.

<!-- TOC -->
* [hdiffpatch-rs](#hdiffpatch-rs)
  * [Format support](#format-support)
  * [Compression modes](#compression-modes)
  * [Checksums](#checksums)
  * [Installing](#installing)
  * [Usage](#usage)
  * [Testing](#testing)
  * [Credits](#credits)
  * [Contributing](#contributing)
<!-- TOC -->

## Format support

| Format      | Status    | Notes                                                                                       |
|-------------|-----------|---------------------------------------------------------------------------------------------|
| `HDIFF13`   | Supported | Single-file patching                                                                        |
| `HDIFF19`   | Supported | Directory patching; payload may be `HDIFF13`, `HDIFFSF20` or `HDIFFW26`                     |
| `HDIFFSF20` | Supported | Single-compressed patching                                                                  |
| `HDIFFW26`  | Supported | Window diff                                                                                 |
| `KrDiff`    | Supported | Modified `HDIFF19`-style directory patch format from KuroGames made for WutheringWaves game |

## Compression modes

| Compression                                 | Status          |
|---------------------------------------------|-----------------|
| `zstd`                                      | Supported       |
| `zlib` / `pzlib`                            | Supported       |
| `bz2` / `bzip2` / `pbz2` / `pbzip2`         | Supported       |
| `lzma`                                      | Supported       |
| `lzma2`                                     | Supported       |
| `lz4` / `brotli` / `7zXZ` / `lzham` / `tuz` | Not implemented |

## Checksums

| Checksum                                     | Status          |
|----------------------------------------------|-----------------|
| `crc32`                                      | Supported       |
| `adler32` / `adler64`                        | Supported       |
| `fadler32` / `fadler64` / `fadler128`        | Supported       |
| `md5` / `sha1` / `sha256` / `sha512`         | Supported       |
| `blake3`                                     | Supported       |
| `xxh3` / `xxh128`                            | Supported       |

New-data digests are verified for `HDIFFW26` and directory diffs. Old and diff digests are parsed but not verified, same as upstream `hpatchz`. Standalone `HDIFF13`/`HDIFFSF20` carry no checksum field at all.

## Installing

```toml
[dependencies]
hdiffpatch-rs = { git = "https://github.com/TwintailTeam/hdiffpatch-rs", branch = "master" }
```

## Usage

```rust
use hdiffpatch_rs::patchers::HDiff;
use hdiffpatch_rs::patchers::KrDiff;

// Pass directory for source path and output path if it is a directory diff (HDIFF19)
fn main() {
    let source_path = String::from("./old_file.bin");
    let patch_path = String::from("./update.hdiff");
    let output_path = String::from("./new_file.bin");

    let mut patcher = HDiff::new(source_path, patch_path, output_path);

  // multi threading!
  let mut multi_threaded = HDiff::new(source_path, patch_path, output_path).with_options(PatchOptions::default().with_threads(8).with_memory_budget(64 << 20));

  // You can also get information about the diff
  let diff_info = patcher.info();

    if patcher.apply() {
        println!("Patch applied successfully");
    } else {
        eprintln!("Patch failed");
    }
}

// KrDiff sample
fn main() {
    let source_dir = String::from("./game");
    let patch_path = String::from("./update.krpdiff"); // .krdiff also works!
    let output_dir = String::from("./patched");

    let mut patcher = KrDiff::new(source_dir, patch_path, output_dir);
  
  // multi threading!
  let mut multi_threaded = KrDiff::new(source_dir, patch_path, output_dir).with_options(PatchOptions::default().with_threads(8).with_memory_budget(64 << 20));
  
  // You can also get information about the diff
  let diff_info = patcher.info();
  
  // KrDiff also has create mode due to its cursed nature
  // we dont know if kuro's patcher would accept this!
  let create_diff = patcher.create();

    if patcher.apply() {
        println!("KrDiff patch applied successfully");
    } else {
        eprintln!("KrDiff patch failed");
    }
}
```

## Testing

```sh
./tests/vectors/generate.sh /tmp/hdiffvec /path/to/hdiffz
HDIFFPATCH_TV=/tmp/hdiffvec cargo test --release
```

## Credits

This project exists because of the original [HDiffPatch](https://github.com/sisong/HDiffPatch) project by sisong. `hdiffpatch-rs` is a Rust implementation patch applier for compatible formats, credit for the original HDiffPatch format and tooling belongs upstream.

Additional credit goes to CollapseLauncher's [SharpHDiffPatch.Core](https://github.com/CollapseLauncher/SharpHDiffPatch.Core), a C# HDiffPatch port whose structure helped guide parts of this crate's parser and patcher logic.

## Contributing

Contributions and issues are welcome this port is not perfect and for sure will have some issues people can find! if you do find them do not be scared to open an issue or a pull request with a fix!
