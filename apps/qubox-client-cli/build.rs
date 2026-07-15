//! P0-3 build script.
//!
//! When the `hw-decode` feature is enabled, the `ffmpeg-next` crate's
//! build script runs `bindgen`, which requires the C interface of
//! `libclang` (the `libclang.so` / `libclang-*.so` shared object) to
//! be locatable on `LIBCLANG_PATH` or in a system search path.
//!
//! On Debian / Ubuntu the package providing the C interface is
//! `libclang-18-dev` (or matching the LLVM major version). The
//! `libclang-cpp18` package that is sometimes preinstalled only ships
//! the C++ interface (`libclang-cpp.so`), which is **not** sufficient
//! for `bindgen`.
//!
//! This build script does two things:
//!  1. Detects whether `LIBCLANG_PATH` is set and points at a directory
//!     containing `libclang.so` (or a versioned variant). Emits a
//!     `cargo:warning=` if the feature is on but libclang cannot be
//!     found, so the build error from `ffmpeg-next`/`bindgen` has
//!     actionable context.
//!  2. Re-emits the `LIBCLANG_PATH` env var as a `cargo:rerun-if-env-changed=`
//!     so cargo rebuilds when the user fixes the path.
//!
//! The actual `get_format` + `av_hwframe_transfer_data` wiring lives
//! in `decoder_hw.rs` and is intentionally a scaffold for now — see
//! `research/roadmap/p0-03-hw-decode.md` and ADR-003 for the full plan.

use std::path::Path;

fn main() {
    println!("cargo:rerun-if-env-changed=LIBCLANG_PATH");
    println!("cargo:rerun-if-env-changed=CLANG_PATH");

    let hw_decode_enabled = std::env::var("CARGO_FEATURE_HW_DECODE").is_ok();
    if !hw_decode_enabled {
        return;
    }

    let libclang_path = std::env::var("LIBCLANG_PATH").ok();
    let libclang_resolved = libclang_path.as_deref().and_then(libclang_dir_has_so);

    if libclang_resolved.is_none() {
        let env_hint = match libclang_path.as_deref() {
            Some(p) if !p.is_empty() => format!("LIBCLANG_PATH={p}"),
            _ => "LIBCLANG_PATH is not set".to_string(),
        };
        println!(
            "cargo:warning=P0-3 hw-decode feature is enabled but the C interface of libclang \
             was not found ({env_hint}). bindgen (invoked by ffmpeg-next) needs `libclang.so` \
             (or libclang-*.so) on the linker path. On Debian/Ubuntu install `libclang-18-dev` \
             (or match the system's LLVM major) and point LIBCLANG_PATH at the directory \
             containing it. Without libclang, fall back to the ffmpeg subprocess decoder \
             (the default)."
        );
    }
}

fn libclang_dir_has_so(dir: &str) -> Option<()> {
    let p = Path::new(dir);
    let entries = std::fs::read_dir(p).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name == "libclang.so" || name.starts_with("libclang-") && name.ends_with(".so") {
            return Some(());
        }
    }
    None
}
