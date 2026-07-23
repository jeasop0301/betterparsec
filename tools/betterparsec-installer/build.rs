// Embed an application manifest so a double-click raises a UAC prompt without an
// external .manifest file or PowerShell.
//
// The requireAdministrator level is embedded ONLY for release builds (the actual
// deliverable installer). Debug builds — which include the test harness cargo
// runs under `cargo test --all-targets` — get asInvoker instead: an elevation
// manifest is still present (so Windows' "installer" filename heuristic does not
// force elevation on the harness), but it does not require admin, so the harness
// launches normally instead of failing with os error 740.
fn main() {
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        use embed_manifest::manifest::ExecutionLevel;
        use embed_manifest::{embed_manifest, new_manifest};
        let release = std::env::var("PROFILE").as_deref() == Ok("release");
        let level = if release { ExecutionLevel::RequireAdministrator } else { ExecutionLevel::AsInvoker };
        embed_manifest(new_manifest("BetterParsec.Installer").requested_execution_level(level))
            .expect("embed application manifest");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
