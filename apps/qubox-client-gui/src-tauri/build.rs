use std::env;
use std::fs;
use std::path::PathBuf;

/// Tauri requires externalBin paths to exist at build time.
/// CI stages real sidecars before `tauri build`; for local `cargo check` /
/// `cargo run` we create empty placeholders so the graph still compiles.
fn ensure_external_bin_placeholders() {
    let triple = env::var("TARGET").unwrap_or_else(|_| {
        env::var("TAURI_ENV_TARGET_TRIPLE").unwrap_or_else(|_| "unknown".into())
    });
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let binaries_dir = manifest_dir.join("binaries");
    let _ = fs::create_dir_all(&binaries_dir);

    let ext = if triple.contains("windows") {
        ".exe"
    } else {
        ""
    };

    for name in ["qubox-daemon", "qubox-host-agent", "qubox-client-cli"] {
        let path = binaries_dir.join(format!("{name}-{triple}{ext}"));
        if !path.exists() {
            let _ = fs::write(&path, []);
            println!("cargo:warning=created placeholder sidecar {}", path.display());
        }
        println!("cargo:rerun-if-changed={}", path.display());
    }
}

fn main() {
    ensure_external_bin_placeholders();
    tauri_build::build()
}
