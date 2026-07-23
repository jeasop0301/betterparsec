//! Minimal native HKLM registry helpers. Native (not `reg.exe`) because the
//! UpperFilters value is a `REG_MULTI_SZ` we must read-modify-write while
//! preserving order and exact entries — parsing `reg query` text is fragile and
//! locale-dependent. No PowerShell, no child process.

use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Registry::{
    HKEY, HKEY_LOCAL_MACHINE, KEY_READ, KEY_SET_VALUE, REG_DWORD, REG_MULTI_SZ, REG_VALUE_TYPE,
    RegCloseKey, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
};
use windows::core::PCWSTR;

/// The keyboard device-class key whose `UpperFilters` we edit.
pub const KEYBOARD_CLASS_KEY: &str =
    r"SYSTEM\CurrentControlSet\Control\Class\{4D36E96B-E325-11CE-BFC1-08002BE10318}";
/// The UEFI Secure Boot state key.
pub const SECURE_BOOT_KEY: &str = r"SYSTEM\CurrentControlSet\Control\SecureBoot\State";

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Whether the current process can open `subkey` for writing (a cheap, side
/// effect-free elevation probe: fails with access-denied when not elevated).
pub fn can_write(subkey: &str) -> bool {
    let sub = wide(subkey);
    let mut hkey = HKEY::default();
    unsafe {
        let status = RegOpenKeyExW(HKEY_LOCAL_MACHINE, PCWSTR(sub.as_ptr()), None, KEY_SET_VALUE, &mut hkey);
        if status == ERROR_SUCCESS {
            let _ = RegCloseKey(hkey);
            true
        } else {
            false
        }
    }
}

/// Read a `REG_DWORD` from HKLM, or `None` if absent / wrong type.
pub fn read_dword(subkey: &str, value: &str) -> Option<u32> {
    let sub = wide(subkey);
    let name = wide(value);
    let mut hkey = HKEY::default();
    unsafe {
        if RegOpenKeyExW(HKEY_LOCAL_MACHINE, PCWSTR(sub.as_ptr()), None, KEY_READ, &mut hkey) != ERROR_SUCCESS {
            return None;
        }
        let mut ty = REG_VALUE_TYPE(0);
        let mut data: u32 = 0;
        let mut cb: u32 = size_of::<u32>() as u32;
        let status = RegQueryValueExW(
            hkey,
            PCWSTR(name.as_ptr()),
            None,
            Some(&mut ty),
            Some((&mut data as *mut u32).cast::<u8>()),
            Some(&mut cb),
        );
        let _ = RegCloseKey(hkey);
        if status == ERROR_SUCCESS && ty == REG_DWORD {
            Some(data)
        } else {
            None
        }
    }
}

/// Read a `REG_MULTI_SZ` value from HKLM as a list of strings. Returns
/// `Some(vec![])` when the value is absent (a keyboard class with no filters),
/// and `None` only when the key itself cannot be opened.
pub fn read_multi_sz(subkey: &str, value: &str) -> Option<Vec<String>> {
    let sub = wide(subkey);
    let name = wide(value);
    let mut hkey = HKEY::default();
    unsafe {
        if RegOpenKeyExW(HKEY_LOCAL_MACHINE, PCWSTR(sub.as_ptr()), None, KEY_READ, &mut hkey) != ERROR_SUCCESS {
            return None;
        }
        let mut ty = REG_VALUE_TYPE(0);
        let mut cb: u32 = 0;
        let status = RegQueryValueExW(hkey, PCWSTR(name.as_ptr()), None, Some(&mut ty), None, Some(&mut cb));
        if status != ERROR_SUCCESS {
            // Value not present: treat as an empty filter list (key exists).
            let _ = RegCloseKey(hkey);
            return Some(Vec::new());
        }
        if ty != REG_MULTI_SZ || cb < 2 {
            let _ = RegCloseKey(hkey);
            return Some(Vec::new());
        }
        let mut buf = vec![0u8; cb as usize];
        let status = RegQueryValueExW(
            hkey,
            PCWSTR(name.as_ptr()),
            None,
            Some(&mut ty),
            Some(buf.as_mut_ptr()),
            Some(&mut cb),
        );
        let _ = RegCloseKey(hkey);
        if status != ERROR_SUCCESS {
            return None;
        }
        let words: &[u16] = std::slice::from_raw_parts(buf.as_ptr().cast::<u16>(), (cb as usize) / 2);
        Some(betterparsec_kbdflt_core::multi_sz::parse(words))
    }
}

/// Write a `REG_MULTI_SZ` value to HKLM. Requires write access (elevation).
pub fn write_multi_sz(subkey: &str, value: &str, values: &[String]) -> Result<(), String> {
    let sub = wide(subkey);
    let name = wide(value);
    let mut hkey = HKEY::default();
    unsafe {
        let status = RegOpenKeyExW(HKEY_LOCAL_MACHINE, PCWSTR(sub.as_ptr()), None, KEY_SET_VALUE, &mut hkey);
        if status != ERROR_SUCCESS {
            return Err(format!("open {subkey} for write failed (error {})", status.0));
        }
        let encoded = betterparsec_kbdflt_core::multi_sz::encode(values);
        let bytes = std::slice::from_raw_parts(encoded.as_ptr().cast::<u8>(), encoded.len() * 2);
        let status = RegSetValueExW(hkey, PCWSTR(name.as_ptr()), None, REG_MULTI_SZ, Some(bytes));
        let _ = RegCloseKey(hkey);
        if status != ERROR_SUCCESS {
            return Err(format!("write {value} failed (error {})", status.0));
        }
        Ok(())
    }
}