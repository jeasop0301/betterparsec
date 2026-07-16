//! `betterparsec-updater` — G006 packaging/update CLI.
//!
//! HARD CONSTRAINT: this tool never publishes or deploys anything. It
//! only signs (dev key) and atomically swaps directories that are
//! already local to the machine it runs on.
//!
//! - `sign`: hash every component listed in a spec file, build a
//!   complete-set manifest, sign it with the dev key, write it out. Used
//!   by `tools/package-portable.ps1 -HostBundle`.
//! - `swap`: verify a staged set against its signed manifest (never
//!   trusting that verification already happened) and, only if it
//!   passes, atomically install it.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use serde::Deserialize;

use bp_updater::manifest::{ComponentEntry, Manifest, ProtocolVersions, Role, SignedManifest};
use bp_updater::{dev_signing_key, dev_trust_root};

#[derive(Parser)]
#[command(
    name = "betterparsec-updater",
    about = "G006 complete-set manifest signing and atomic swap (packaging tool only -- never publishes or deploys anything)"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Hash every component named in `--spec`, build a manifest, sign it
    /// with the dev key, and write the signed manifest JSON to `--out`.
    /// Dev key only -- release signing happens outside this repo.
    Sign(SignArgs),
    /// Re-verify a staged directory against a signed manifest (signature,
    /// internal consistency, protocol compatibility, per-file hash/size)
    /// and, only if every check passes, atomically swap it into the
    /// install directory.
    Swap(SwapArgs),
}

#[derive(clap::Args)]
struct SignArgs {
    /// Directory containing the already-staged component files.
    #[arg(long)]
    staged: PathBuf,
    /// JSON array of `{"name": "...", "role": "client|host|shared"}`.
    #[arg(long)]
    spec: PathBuf,
    #[arg(long)]
    set_version: String,
    #[arg(long, default_value = "dev-trust-root-v1")]
    signing_key_id: String,
    /// Where to write the signed manifest JSON.
    #[arg(long)]
    out: PathBuf,
}

#[derive(clap::Args)]
struct SwapArgs {
    #[arg(long)]
    staged: PathBuf,
    #[arg(long)]
    install: PathBuf,
    #[arg(long)]
    manifest: PathBuf,
}

#[derive(Deserialize)]
struct ComponentSpecEntry {
    name: String,
    role: Role,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Sign(args) => run_sign(args),
        Command::Swap(args) => run_swap(args),
    }
}

fn run_sign(args: SignArgs) -> Result<()> {
    let spec_text = fs::read_to_string(&args.spec)
        .with_context(|| format!("reading component spec {}", args.spec.display()))?;
    let spec: Vec<ComponentSpecEntry> = serde_json::from_str(&spec_text)
        .with_context(|| format!("parsing component spec {}", args.spec.display()))?;
    if spec.is_empty() {
        bail!("component spec {} lists no components", args.spec.display());
    }

    let mut components = Vec::with_capacity(spec.len());
    for entry in spec {
        bp_updater::manifest::validate_component_name(&entry.name)
            .with_context(|| format!("component spec entry {:?}", entry.name))?;
        let path = args.staged.join(&entry.name);
        let size = fs::metadata(&path)
            .with_context(|| format!("stat {}", path.display()))?
            .len();
        let sha256 = bp_updater::verify::sha256_file(&path)
            .with_context(|| format!("hash {}", path.display()))?;
        components.push(ComponentEntry {
            name: entry.name,
            role: entry.role,
            sha256,
            size,
        });
    }
    // Stable order: canonicalization sorts object keys but not array
    // elements, so a manifest's byte identity should not depend on the
    // spec file's incidental ordering.
    components.sort_by(|a, b| a.name.cmp(&b.name));

    let manifest = Manifest {
        set_version: args.set_version,
        protocol: ProtocolVersions::default(),
        components,
        signing_key_id: args.signing_key_id,
    };
    let signed = bp_updater::manifest::sign(manifest, &dev_signing_key())
        .context("canonicalizing manifest for signing")?;
    let json = serde_json::to_string_pretty(&signed)?;
    fs::write(&args.out, json)
        .with_context(|| format!("writing manifest {}", args.out.display()))?;
    println!(
        "dev-signed manifest: {} ({} components, set_version {:?}) -- DEV KEY ONLY, release signing happens outside this repo",
        args.out.display(),
        signed.manifest.components.len(),
        signed.manifest.set_version,
    );
    Ok(())
}

fn run_swap(args: SwapArgs) -> Result<()> {
    let manifest_text = fs::read_to_string(&args.manifest)
        .with_context(|| format!("reading manifest {}", args.manifest.display()))?;
    let signed: SignedManifest = serde_json::from_str(&manifest_text)
        .with_context(|| format!("parsing manifest {}", args.manifest.display()))?;

    // Never trust the caller: re-verify signature, internal consistency,
    // protocol compatibility, and every component's hash/size before
    // touching the filesystem -- regardless of whatever verification
    // already happened to produce `--staged`.
    let verified = bp_updater::verify::verify_staged_set(
        &args.staged,
        &signed,
        &dev_trust_root(),
        &ProtocolVersions::default(),
    )
    .context("staged set failed verification -- refusing to swap")?;

    let outgoing_version =
        read_installed_version(&args.install).unwrap_or_else(|| "unknown".to_string());
    bp_updater::swap::perform_swap(&args.staged, &args.install, &outgoing_version, &verified)
        .context("atomic swap failed")?;

    // Keep a copy of the manifest inside the install so the next swap
    // (and the in-app status API, `app-native/src/update.rs`) can read
    // back "what version is currently installed" without re-deriving it
    // from file hashes.
    let manifest_copy = args.install.join("update-manifest.json");
    fs::write(&manifest_copy, manifest_text)
        .with_context(|| format!("writing {}", manifest_copy.display()))?;

    println!(
        "swapped in set_version {:?} at {}",
        signed.manifest.set_version,
        args.install.display()
    );
    Ok(())
}

fn read_installed_version(install: &std::path::Path) -> Option<String> {
    let text = fs::read_to_string(install.join("update-manifest.json")).ok()?;
    let signed: SignedManifest = serde_json::from_str(&text).ok()?;
    Some(signed.manifest.set_version)
}
