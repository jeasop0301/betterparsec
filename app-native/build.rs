//! A0 slice 2 (`video` feature): stage the pinned FFmpeg shared DLLs next
//! to the produced binaries so `cargo run` / `cargo test` work without
//! PATH surgery. `FFMPEG_DIR` comes from `.cargo/config.toml`
//! (`tools/bootstrap-ffmpeg.ps1` populates `third-party/ffmpeg`).

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    println!("cargo:rerun-if-env-changed=FFMPEG_DIR");
    if env::var_os("CARGO_FEATURE_VIDEO").is_none() {
        return;
    }
    let Some(dir) = env::var_os("FFMPEG_DIR") else {
        println!(
            "cargo:warning=video feature enabled but FFMPEG_DIR is unset; \
             run tools/bootstrap-ffmpeg.ps1"
        );
        return;
    };
    let bin = PathBuf::from(dir).join("bin");
    if !bin.is_dir() {
        println!(
            "cargo:warning=FFMPEG_DIR has no bin/ directory ({}); \
             run tools/bootstrap-ffmpeg.ps1",
            bin.display()
        );
        return;
    }

    // OUT_DIR = target/<profile>/build/<pkg>-<hash>/out; ancestor 3 is the
    // profile dir. Unofficial but the only practical hook for staging
    // runtime DLLs (bins land in <profile>/, test executables in
    // <profile>/deps/).
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"));
    let Some(profile_dir) = out.ancestors().nth(3) else {
        return;
    };
    for dest_dir in [profile_dir.to_path_buf(), profile_dir.join("deps")] {
        let _ = fs::create_dir_all(&dest_dir);
        stage_dlls(&bin, &dest_dir);
    }
}

fn stage_dlls(bin: &Path, dest_dir: &Path) {
    let Ok(entries) = fs::read_dir(bin) else {
        return;
    };
    for entry in entries.flatten() {
        let src = entry.path();
        if src
            .extension()
            .is_none_or(|e| !e.eq_ignore_ascii_case("dll"))
        {
            continue;
        }
        let dest = dest_dir.join(entry.file_name());
        if up_to_date(&src, &dest) {
            continue;
        }
        if let Err(e) = fs::copy(&src, &dest) {
            println!(
                "cargo:warning=failed to stage {} -> {}: {e}",
                src.display(),
                dest.display()
            );
        }
    }
}

fn up_to_date(src: &Path, dest: &Path) -> bool {
    let (Ok(s), Ok(d)) = (fs::metadata(src), fs::metadata(dest)) else {
        return false;
    };
    s.len() == d.len()
        && match (s.modified(), d.modified()) {
            (Ok(sm), Ok(dm)) => dm >= sm,
            _ => false,
        }
}
