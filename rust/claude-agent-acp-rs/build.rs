use std::{env, fs, path::Path};

fn main() {
    let dir = env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| panic!("CARGO_MANIFEST_DIR unset"));
    let pin = Path::new(&dir).join("../../porting/UPSTREAM_VERSION");
    println!("cargo:rerun-if-changed={}", pin.display());
    let raw = fs::read_to_string(&pin).unwrap_or_else(|e| panic!("read {}: {e}", pin.display()));
    let v = raw.trim();
    let ok = v.split('.').count() == 3
        && v.split('.')
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    if !ok {
        panic!("porting/UPSTREAM_VERSION must be MAJOR.MINOR.PATCH, got {v:?}");
    }
    let krate = env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| panic!("CARGO_PKG_VERSION unset"));
    println!("cargo:rustc-env=UPSTREAM_NPM_VERSION={v}");
    println!("cargo:rustc-env=ACP_VERSION_STRING={krate}+upstream.{v}");
}
