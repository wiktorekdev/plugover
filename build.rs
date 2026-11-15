//! Puts the punktfunk plugin package inside the binary when PLUGOVER_PLUGIN points at it (as
//! scripts/build.sh does), so `plugover setup` installs everything from one file. PLUGOVER_UI
//! points at the built console page (plugin/dist/ui), which the host daemon also serves itself
//! for `plugover ui`.

use std::{env, fs, path::Path, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-env-changed=PLUGOVER_PLUGIN");
    println!("cargo:rerun-if-env-changed=PLUGOVER_UI");
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
    let out = out_dir.join("plugin.tgz");
    match env::var_os("PLUGOVER_PLUGIN").filter(|p| !p.is_empty()) {
        Some(p) => {
            println!("cargo:rerun-if-changed={}", p.to_string_lossy());
            fs::copy(&p, &out).expect("PLUGOVER_PLUGIN points at the plugin package");
        }
        None => fs::write(&out, b"").expect("writing to OUT_DIR"),
    }

    // `pub const UI: &[(&str, &[u8])]`: each file of the page by its path, index.html included.
    let mut files = Vec::new();
    if let Some(dir) = env::var_os("PLUGOVER_UI").filter(|p| !p.is_empty()).map(PathBuf::from) {
        println!("cargo:rerun-if-changed={}", dir.display());
        collect(&dir, &dir, &mut files);
        assert!(files.iter().any(|(name, _)| name == "index.html"), "PLUGOVER_UI has no index.html");
    }
    let entries: String = files.iter().map(|(name, path)| format!("    ({name:?}, include_bytes!({path:?})),\n")).collect();
    fs::write(out_dir.join("ui.rs"), format!("pub const UI: &[(&str, &[u8])] = &[\n{entries}];\n")).expect("writing to OUT_DIR");
}

fn collect(root: &Path, dir: &Path, files: &mut Vec<(String, String)>) {
    for e in fs::read_dir(dir).expect("reading PLUGOVER_UI").flatten() {
        let path = e.path();
        if path.is_dir() {
            collect(root, &path, files);
        } else {
            let name = path.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
            files.push((name, path.canonicalize().unwrap().to_string_lossy().into_owned()));
        }
    }
}
