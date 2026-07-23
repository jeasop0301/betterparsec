//! Pure `REG_MULTI_SZ` encode/parse (null-separated, double-null terminated
//! wide strings). Split out from the registry FFI so it is unit-testable
//! without Windows or the bin's elevation manifest.

/// Parse a wide `REG_MULTI_SZ` buffer into strings, stopping at the double null
/// and dropping empty trailing entries.
pub fn parse(words: &[u16]) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur: Vec<u16> = Vec::new();
    for &w in words {
        if w == 0 {
            if cur.is_empty() {
                break; // double null → end of data
            }
            out.push(String::from_utf16_lossy(&cur));
            cur.clear();
        } else {
            cur.push(w);
        }
    }
    if !cur.is_empty() {
        out.push(String::from_utf16_lossy(&cur));
    }
    out
}

/// Encode strings as a wide `REG_MULTI_SZ` buffer (each null-terminated, plus a
/// final extra null; an empty list encodes as a single null).
pub fn encode(values: &[String]) -> Vec<u16> {
    let mut data: Vec<u16> = Vec::new();
    for v in values {
        data.extend(v.encode_utf16());
        data.push(0);
    }
    data.push(0);
    data
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let values = vec!["VendorA".to_string(), "BetterParsecKbdFlt".to_string(), "kbdclass".to_string()];
        let encoded = encode(&values);
        assert_eq!(*encoded.last().expect("non-empty"), 0u16);
        assert_eq!(parse(&encoded), values);
    }

    #[test]
    fn parse_stops_at_double_null_and_ignores_trailing() {
        let words: Vec<u16> = [b'a' as u16, 0, b'b' as u16, 0, 0, b'x' as u16, 0].to_vec();
        assert_eq!(parse(&words), vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn empty_list_encodes_to_single_null() {
        assert_eq!(encode(&[]), vec![0u16]);
        assert!(parse(&[0u16]).is_empty());
    }
}
