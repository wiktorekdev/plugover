//! Puts the punktfunk plugin package inside the binary when PLUGOVER_PLUGIN points at it (as
//! scripts/build.sh does), so `plugover setup` installs everything from one file.

use std::{env, fs, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-env-changed=PLUGOVER_PLUGIN");
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("cargo sets OUT_DIR")).join("plugin.tgz");
    match env::var_os("PLUGOVER_PLUGIN").filter(|p| !p.is_empty()) {
        Some(p) => {
            println!("cargo:rerun-if-changed={}", p.to_string_lossy());
            fs::copy(&p, &out).expect("PLUGOVER_PLUGIN points at the plugin package");
        }
        None => fs::write(&out, b"").expect("writing to OUT_DIR"),
    }
}
