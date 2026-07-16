fn main() {
    // Windows UAC "installer detection" heuristically demands elevation for any
    // executable whose name contains "update"/"updater"/"setup"/"patch" unless it
    // embeds an application manifest. Without this, even the cargo test harness
    // (bp_updater-<hash>.exe) fails to spawn with os error 740. Embed an
    // asInvoker manifest so the updater and its test binaries run unelevated;
    // the swap path never requires elevation because it only touches
    // user-writable install directories.
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if os == "windows" && env == "msvc" {
        println!("cargo:rustc-link-arg=/MANIFEST:EMBED");
        println!("cargo:rustc-link-arg=/MANIFESTUAC:level='asInvoker' uiAccess='false'");
    }
}
