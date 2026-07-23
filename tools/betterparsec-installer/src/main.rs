//! BetterParsec keyboard-capture installer.
//!
//! A native, UAC-elevated installer (the manifest forces an elevation prompt on
//! launch, exactly like the Parsec installer) that stands in for
//! `sign-test.ps1` + `install-test.ps1` on hardened test machines where
//! PowerShell is blocked. It performs every privileged step with native Win32
//! (registry + Service Control Manager) plus `certutil`/`bcdedit`, never
//! PowerShell.
//!
//! Payload (place next to this exe): `betterparsec-kbdflt.sys` (pre-signed on
//! the build machine), `betterparsec-kbdflt.cer` (its public test cert), and
//! `input-broker.exe`.
//!
//! Usage:
//!   betterparsec-installer                 install (default)
//!   betterparsec-installer status          read-only diagnostics (changes nothing)
//!   betterparsec-installer uninstall       remove services + filter
//!   betterparsec-installer selftest        UpperFilters ordering self-test
//! Flags:
//!   --production-signed   the .sys is Microsoft/attestation-signed; skip TESTSIGNING
//!   --yes                 do not prompt (assume yes for reboot)
//!
//! IMPORTANT: our test-signed `.sys` still requires Windows TESTSIGNING mode
//! (reboot) and Secure Boot OFF. Only a Microsoft attestation/WHQL-signed `.sys`
//! (pass `--production-signed`) loads with Secure Boot on and no test mode.

// Pure logic lives in the library target (betterparsec_installer); the Windows
// FFI modules stay in the bin.
#[cfg(windows)]
mod registry;
#[cfg(windows)]
mod services;

#[cfg(not(windows))]
fn main() {
    eprintln!("betterparsec-installer is Windows-only.");
    std::process::exit(1);
}

#[cfg(windows)]
fn main() {
    std::process::exit(windows_impl::run());
}

#[cfg(windows)]
mod windows_impl {
    use crate::registry::{self, KEYBOARD_CLASS_KEY, SECURE_BOOT_KEY};
    use crate::services;
    use betterparsec_kbdflt_core::upperfilters::{self, DRIVER_SERVICE};
    use std::io::Write;
    use std::path::{Path, PathBuf};

    const BROKER_SERVICE: &str = "BetterParsecInput";
    const UPPERFILTERS_VALUE: &str = "UpperFilters";
    const DRIVER_SYS_NAME: &str = "betterparsec-kbdflt.sys";
    const DRIVER_CER_NAME: &str = "betterparsec-kbdflt.cer";
    const BROKER_EXE_NAME: &str = "input-broker.exe";

    struct Opts {
        production_signed: bool,
        assume_yes: bool,
    }

    pub fn run() -> i32 {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let opts = Opts {
            production_signed: args.iter().any(|a| a == "--production-signed"),
            assume_yes: args.iter().any(|a| a == "--yes" || a == "-y"),
        };
        let command = args.iter().find(|a| !a.starts_with('-')).map(String::as_str).unwrap_or("install");

        let code = match command {
            "install" => install(&opts),
            "uninstall" => uninstall(&opts),
            "status" => status(),
            "selftest" => selftest(),
            other => {
                eprintln!("unknown command: {other} (use install | uninstall | status | selftest)");
                2
            }
        };
        if !opts.assume_yes {
            pause();
        }
        code
    }

    // ── paths ────────────────────────────────────────────────────────────────

    fn env_or(name: &str, fallback: &str) -> String {
        std::env::var(name).unwrap_or_else(|_| fallback.to_string())
    }

    fn driver_dest() -> PathBuf {
        Path::new(&env_or("SystemRoot", r"C:\Windows")).join("System32").join("drivers").join(DRIVER_SYS_NAME)
    }
    fn broker_dir() -> PathBuf {
        Path::new(&env_or("ProgramFiles", r"C:\Program Files")).join("BetterParsec").join("Input")
    }
    fn backup_path() -> PathBuf {
        Path::new(&env_or("ProgramData", r"C:\ProgramData")).join("BetterParsec").join("keyboard-upperfilters.json")
    }
    fn payload_dir() -> PathBuf {
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(Path::to_path_buf))
            .unwrap_or_else(|| PathBuf::from("."))
    }

    // ── commands ──────────────────────────────────────────────────────────────

    fn install(opts: &Opts) -> i32 {
        println!("BetterParsec keyboard-capture installer\n");
        if !registry::can_write(KEYBOARD_CLASS_KEY) {
            eprintln!("Not elevated: right-click the installer and choose 'Run as administrator'.");
            return 1;
        }

        let dir = payload_dir();
        let sys = dir.join(DRIVER_SYS_NAME);
        let cer = dir.join(DRIVER_CER_NAME);
        let broker = dir.join(BROKER_EXE_NAME);
        for (label, path) in [("driver", &sys), ("certificate", &cer), ("broker", &broker)] {
            if !path.is_file() {
                eprintln!("Missing {label}: {}. Put all three payload files next to the installer.", path.display());
                return 1;
            }
        }

        // TESTSIGNING gate: a test-signed driver needs test mode + Secure Boot off.
        let need_testsigning = !opts.production_signed;
        let secure_boot_on = registry::read_dword(SECURE_BOOT_KEY, "UEFISecureBootEnabled") == Some(1);
        if need_testsigning && secure_boot_on {
            eprintln!("Secure Boot is ON, which blocks TESTSIGNING for our test-signed driver.");
            eprintln!("Fix one of:");
            eprintln!("  1) Disable Secure Boot in the BIOS/UEFI, reboot, then re-run this installer, or");
            eprintln!("  2) Use a Microsoft attestation/WHQL-signed .sys and pass --production-signed.");
            return 2;
        }

        // 1) Trust the driver's test certificate (Root + TrustedPublisher).
        let cer_s = cer.to_string_lossy().to_string();
        println!("[1/5] Trusting driver certificate");
        run_cmd("certutil", &["-addstore", "-f", "Root", &cer_s]);
        run_cmd("certutil", &["-addstore", "-f", "TrustedPublisher", &cer_s]);

        // 2) Enable TESTSIGNING (skipped for a production-signed driver).
        if need_testsigning {
            println!("[2/5] Enabling test-signing boot mode");
            if !run_cmd("bcdedit", &["/set", "testsigning", "on"]) {
                eprintln!("  bcdedit failed. If Secure Boot is on, disable it in the BIOS first.");
                return 2;
            }
        } else {
            println!("[2/5] Production-signed driver: TESTSIGNING not required");
        }

        // 3) Copy payload into place.
        println!("[3/5] Installing files");
        let d_dest = driver_dest();
        let b_dir = broker_dir();
        let b_dest = b_dir.join(BROKER_EXE_NAME);
        if let Err(e) = std::fs::copy(&sys, &d_dest) {
            eprintln!("  copy driver -> {}: {e}", d_dest.display());
            return 1;
        }
        if let Err(e) = std::fs::create_dir_all(&b_dir) {
            eprintln!("  create {}: {e}", b_dir.display());
            return 1;
        }
        if let Err(e) = std::fs::copy(&broker, &b_dest) {
            eprintln!("  copy broker -> {}: {e}", b_dest.display());
            return 1;
        }

        // 4) Create the services (replace any prior copies).
        println!("[4/5] Creating services");
        for svc in [DRIVER_SERVICE, BROKER_SERVICE] {
            if let Err(e) = services::delete_service(svc) {
                eprintln!("  warning: could not remove existing {svc}: {e}");
            }
        }
        if let Err(e) = services::create_kernel_driver(DRIVER_SERVICE, "BetterParsec Keyboard Filter", &d_dest.to_string_lossy()) {
            eprintln!("  {e}");
            return 1;
        }
        let broker_cmd = format!("\"{}\" --service", b_dest.to_string_lossy());
        if let Err(e) = services::create_localsystem_service(BROKER_SERVICE, "BetterParsec Input Broker", &broker_cmd) {
            eprintln!("  {e}");
            return 1;
        }

        // 5) Insert the filter into the keyboard-class UpperFilters (with backup).
        println!("[5/5] Registering keyboard filter");
        let current = match registry::read_multi_sz(KEYBOARD_CLASS_KEY, UPPERFILTERS_VALUE) {
            Some(v) => v,
            None => {
                eprintln!("  cannot read keyboard-class UpperFilters");
                return 1;
            }
        };
        write_backup(&current);
        let ordered = match upperfilters::resolve_install_order(&current) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("  {e}");
                return 1;
            }
        };
        if let Err(e) = registry::write_multi_sz(KEYBOARD_CLASS_KEY, UPPERFILTERS_VALUE, &ordered) {
            eprintln!("  {e}");
            return 1;
        }
        println!("  UpperFilters: {}", ordered.join(", "));

        println!("\nInstall complete. A REBOOT is required to load the keyboard filter.");
        println!("After reboot: verify the 'BetterParsecInput' service is Running and the app logs");
        println!("'keyboard capture armed through LocalSystem broker'.");
        maybe_reboot(opts);
        0
    }

    fn uninstall(opts: &Opts) -> i32 {
        println!("BetterParsec keyboard-capture uninstaller\n");
        if !registry::can_write(KEYBOARD_CLASS_KEY) {
            eprintln!("Not elevated: right-click and choose 'Run as administrator'.");
            return 1;
        }
        if let Err(e) = services::delete_service(BROKER_SERVICE) {
            eprintln!("warning: {e}");
        }
        // Remove the filter from UpperFilters (refuses to drop kbdclass).
        if let Some(current) = registry::read_multi_sz(KEYBOARD_CLASS_KEY, UPPERFILTERS_VALUE) {
            match upperfilters::resolve_uninstall_order(&current) {
                Ok(filtered) => {
                    if let Err(e) = registry::write_multi_sz(KEYBOARD_CLASS_KEY, UPPERFILTERS_VALUE, &filtered) {
                        eprintln!("warning: {e}");
                    }
                }
                Err(e) => eprintln!("warning: {e}"),
            }
        }
        if let Err(e) = services::delete_service(DRIVER_SERVICE) {
            eprintln!("warning: {e}");
        }
        let _ = std::fs::remove_file(broker_dir().join(BROKER_EXE_NAME));
        let _ = std::fs::remove_file(driver_dest());
        println!("Uninstalled services and keyboard filter.");
        println!("To leave test mode, run in an elevated console: bcdedit /set testsigning off");
        println!("A REBOOT is required to unload the keyboard filter.");
        maybe_reboot(opts);
        0
    }

    fn status() -> i32 {
        println!("BetterParsec install status (read-only)\n");
        println!("elevated (can write class key): {}", registry::can_write(KEYBOARD_CLASS_KEY));
        let sb = registry::read_dword(SECURE_BOOT_KEY, "UEFISecureBootEnabled");
        println!(
            "secure boot: {}",
            match sb {
                Some(1) => "ON (blocks test-signed driver load)",
                Some(_) => "off",
                None => "unknown (likely legacy/BIOS boot)",
            }
        );
        print!("test-signing: ");
        let _ = std::io::stdout().flush();
        match std::process::Command::new("bcdedit").args(["/enum", "{current}"]).output() {
            Ok(out) => {
                let text = String::from_utf8_lossy(&out.stdout);
                let line = text.lines().find(|l| l.to_lowercase().contains("testsigning"));
                println!("{}", line.map(str::trim).unwrap_or("not set (default: off)"));
            }
            Err(e) => println!("could not run bcdedit: {e}"),
        }
        println!("service {DRIVER_SERVICE}: {}", if services::service_exists(DRIVER_SERVICE) { "present" } else { "absent" });
        println!("service {BROKER_SERVICE}: {}", if services::service_exists(BROKER_SERVICE) { "present" } else { "absent" });
        match registry::read_multi_sz(KEYBOARD_CLASS_KEY, UPPERFILTERS_VALUE) {
            Some(f) => {
                println!("UpperFilters: {}", if f.is_empty() { "(none)".to_string() } else { f.join(", ") });
                let installed = f.iter().any(|s| s.eq_ignore_ascii_case(DRIVER_SERVICE));
                println!("filter installed: {installed}");
            }
            None => println!("UpperFilters: <cannot read>"),
        }
        0
    }

    fn selftest() -> i32 {
        let cases: [(&[&str], &[&str]); 3] = [
            (&["kbdclass"], &[DRIVER_SERVICE, "kbdclass"]),
            (&["VendorA", "kbdclass", "VendorB"], &["VendorA", DRIVER_SERVICE, "kbdclass", "VendorB"]),
            (&[DRIVER_SERVICE, "kbdclass"], &[DRIVER_SERVICE, "kbdclass"]),
        ];
        let mut ok = true;
        for (input, expected) in cases {
            let input_v: Vec<String> = input.iter().map(|s| s.to_string()).collect();
            let expected_v: Vec<String> = expected.iter().map(|s| s.to_string()).collect();
            let pass = matches!(upperfilters::resolve_install_order(&input_v), Ok(v) if v == expected_v);
            println!("{} {:?}", if pass { "ok  " } else { "FAIL" }, input);
            ok &= pass;
        }
        if ok {
            println!("\nUpperFilters ordering self-test passed.");
            0
        } else {
            eprintln!("\nUpperFilters ordering self-test FAILED.");
            1
        }
    }

    // ── helpers ───────────────────────────────────────────────────────────────

    fn run_cmd(cmd: &str, args: &[&str]) -> bool {
        println!("  > {cmd} {}", args.join(" "));
        match std::process::Command::new(cmd).args(args).status() {
            Ok(s) => s.success(),
            Err(e) => {
                eprintln!("    could not launch {cmd}: {e}");
                false
            }
        }
    }

    fn write_backup(current: &[String]) {
        let path = backup_path();
        if path.exists() {
            return; // never overwrite the original pre-install state
        }
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let json = to_json_array(current);
        if let Err(e) = std::fs::write(&path, json) {
            eprintln!("  warning: could not write UpperFilters backup {}: {e}", path.display());
        }
    }

    fn to_json_array(items: &[String]) -> String {
        let parts: Vec<String> =
            items.iter().map(|s| format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))).collect();
        format!("[{}]", parts.join(","))
    }

    fn maybe_reboot(opts: &Opts) {
        let go = opts.assume_yes || confirm("Reboot now?");
        if go {
            run_cmd("shutdown", &["/r", "/t", "0"]);
        } else {
            println!("Reboot later to apply.");
        }
    }

    fn confirm(prompt: &str) -> bool {
        print!("{prompt} [y/N]: ");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).is_ok() {
            matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
        } else {
            false
        }
    }

    fn pause() {
        print!("\nPress Enter to exit...");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
    }
}
