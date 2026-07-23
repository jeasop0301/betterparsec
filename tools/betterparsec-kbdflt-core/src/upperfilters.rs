//! Pure UpperFilters ordering logic — the correctness-critical part of the
//! install. Ported byte-for-byte from `install-test.ps1`'s
//! `Resolve-UpperFilterOrder` + its `-SelfTest` cases so the native installer
//! and the PowerShell script agree on exactly one placement rule: our filter
//! sits immediately ABOVE `kbdclass`, every vendor filter is preserved in
//! order, and there is always exactly one `kbdclass`.

/// The keyboard class-filter service name (the driver).
pub const DRIVER_SERVICE: &str = "BetterParsecKbdFlt";
const KBDCLASS: &str = "kbdclass";

/// Compute the UpperFilters list to write on install: drop any prior copy of
/// our driver, then re-insert it immediately before `kbdclass`. Returns an
/// error (write is refused) unless there is exactly one `kbdclass` entry.
pub fn resolve_install_order(current: &[String]) -> Result<Vec<String>, String> {
    let preserved: Vec<String> = current
        .iter()
        .filter(|f| !f.eq_ignore_ascii_case(DRIVER_SERVICE))
        .cloned()
        .collect();
    let kbd_count = preserved.iter().filter(|f| f.eq_ignore_ascii_case(KBDCLASS)).count();
    if kbd_count != 1 {
        return Err(format!("expected exactly one kbdclass entry; found {kbd_count}"));
    }
    let idx = preserved
        .iter()
        .position(|f| f.eq_ignore_ascii_case(KBDCLASS))
        .expect("kbdclass present (count checked above)");
    let mut ordered = Vec::with_capacity(preserved.len() + 1);
    ordered.extend_from_slice(&preserved[..idx]);
    ordered.push(DRIVER_SERVICE.to_string());
    ordered.extend_from_slice(&preserved[idx..]);
    Ok(ordered)
}

/// Compute the UpperFilters list to write on uninstall: drop our driver,
/// preserving everything else. Refuses (errors) if that would leave the list
/// without `kbdclass`, mirroring the PowerShell guard.
pub fn resolve_uninstall_order(current: &[String]) -> Result<Vec<String>, String> {
    let filtered: Vec<String> = current
        .iter()
        .filter(|f| !f.eq_ignore_ascii_case(DRIVER_SERVICE))
        .cloned()
        .collect();
    if !filtered.iter().any(|f| f.eq_ignore_ascii_case(KBDCLASS)) {
        return Err("refusing to write UpperFilters without kbdclass".to_string());
    }
    Ok(filtered)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn install_places_driver_immediately_above_kbdclass() {
        // The three canonical cases from install-test.ps1 -SelfTest.
        assert_eq!(resolve_install_order(&v(&["kbdclass"])).expect("valid"), v(&[DRIVER_SERVICE, "kbdclass"]));
        assert_eq!(
            resolve_install_order(&v(&["VendorA", "kbdclass", "VendorB"])).expect("valid"),
            v(&["VendorA", DRIVER_SERVICE, "kbdclass", "VendorB"])
        );
        // Idempotent: an already-installed filter is removed then re-inserted.
        assert_eq!(
            resolve_install_order(&v(&[DRIVER_SERVICE, "kbdclass"])).expect("valid"),
            v(&[DRIVER_SERVICE, "kbdclass"])
        );
    }

    #[test]
    fn install_is_case_insensitive_for_our_service() {
        assert_eq!(
            resolve_install_order(&v(&["betterparsecKBDFLT", "kbdclass"])).expect("valid"),
            v(&[DRIVER_SERVICE, "kbdclass"])
        );
    }

    #[test]
    fn install_refuses_without_exactly_one_kbdclass() {
        assert!(resolve_install_order(&v(&["VendorA"])).is_err());
        assert!(resolve_install_order(&v(&["kbdclass", "kbdclass"])).is_err());
        assert!(resolve_install_order(&v(&[])).is_err());
    }

    #[test]
    fn uninstall_drops_only_our_driver() {
        assert_eq!(
            resolve_uninstall_order(&v(&["VendorA", DRIVER_SERVICE, "kbdclass", "VendorB"])).expect("valid"),
            v(&["VendorA", "kbdclass", "VendorB"])
        );
        // No-op when the driver is absent.
        assert_eq!(resolve_uninstall_order(&v(&["kbdclass"])).expect("valid"), v(&["kbdclass"]));
    }

    #[test]
    fn uninstall_refuses_to_drop_kbdclass() {
        assert!(resolve_uninstall_order(&v(&[DRIVER_SERVICE])).is_err());
    }
}
