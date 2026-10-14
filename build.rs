//! With the `console` feature, embeds the web console's build (`web/dist`): writes
//! `$OUT_DIR/console_assets.rs`, a table of each file's path under `/` and its bytes, for
//! `api::console`. Without the feature it does nothing, so a default build needs no Node.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::{env, fs};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if env::var_os("CARGO_FEATURE_CONSOLE").is_none() {
        return;
    }
    println!("cargo:rerun-if-changed=web/dist");
    let dist = Path::new(&env::var("CARGO_MANIFEST_DIR").unwrap()).join("web/dist");
    assert!(
        dist.join("index.html").is_file(),
        "the console feature needs web/dist: run `npm ci && npm run build` in web/ first"
    );
    let mut files = Vec::new();
    walk(&dist, &mut files);
    files.sort();
    let mut table = String::from("static ASSETS: &[(&str, &[u8])] = &[\n");
    for file in &files {
        let path = file.strip_prefix(&dist).unwrap().components().fold(String::new(), |mut path, part| {
            path.push('/');
            path.push_str(part.as_os_str().to_str().expect("web/dist's file names are UTF-8"));
            path
        });
        let file = file.to_str().expect("web/dist's path is UTF-8");
        writeln!(table, "    ({path:?}, include_bytes!({file:?})),").unwrap();
    }
    table.push_str("];\n");
    fs::write(Path::new(&env::var("OUT_DIR").unwrap()).join("console_assets.rs"), table).unwrap();
}

/// Adds the files under `dir` to `files`, recursively.
fn walk(dir: &Path, files: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            walk(&path, files);
        } else {
            files.push(path);
        }
    }
}
