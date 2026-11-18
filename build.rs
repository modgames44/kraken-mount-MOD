//! Copies vendor/ampr-emu-ng/libSceAmpr-debug.sprx into OUT_DIR for main.rs's
//! BUNDLED_AMPR_DEBUG, or an empty file when the vendor folder has none (an upstream
//! release ships only the release build): the debug feature then stays off at run time.
use std::path::Path;

fn main() {
    let src = Path::new("vendor/ampr-emu-ng/libSceAmpr-debug.sprx");
    println!("cargo:rerun-if-changed={}", src.display());
    let out = Path::new(&std::env::var("OUT_DIR").expect("OUT_DIR")).join("ampr-debug.sprx");
    let bytes = std::fs::read(src).unwrap_or_default();
    std::fs::write(&out, bytes).expect("write ampr-debug.sprx");
}
