//! Reliable file-transfer safety core (G013). The wire control messages
//! (offer/accept/cancel/progress) live in `desktop_control::file_transfer`; this
//! module adds the two safety-critical pieces: a destination-name sanitizer that
//! rejects path traversal / absolute paths / reserved device names, and a
//! transfer state machine that requires explicit consent before bytes flow,
//! enforces a size quota, and commits atomically only when every byte arrived
//! AND the content hash matches (no partial or unverified file is ever kept).
//!
//! Pure and headless. The actual reliable channel, disk I/O, and the physical
//! large-file/cancel/reconnect campaign are the remaining (and security-review-
//! gated) part of G013.

/// Windows reserved device base names that must never be used as a destination.
const RESERVED: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Unicode bidi/format controls (General_Category=Cf) that render invisibly or
/// reorder text, so a name like `photo\u{202E}gpj.exe` can display as a benign
/// `.jpg` while writing an `.exe`. `char::is_control` only covers Cc, not these.
const fn is_bidi_or_format(c: char) -> bool {
    matches!(
        c,
        '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{FEFF}'
    )
}
/// Sanitize a proposed destination file name to a single safe component, or
/// `None` if it is unsafe: empty, over-long, containing a path separator, NUL,
/// control byte, a Windows-illegal name char (`: < > " | ? *`), a Unicode
/// bidi/format control (which can spoof the displayed name), a `.`/`..`
/// traversal component, or a Windows reserved device name (case-insensitive,
/// with or without an extension).
pub fn safe_dest_name(name: &str) -> Option<String> {
    if name.is_empty() || name.len() > 255 {
        return None;
    }
    if name.chars().any(|c| {
        matches!(
            c,
            '/' | '\\' | '\0' | ':' | '<' | '>' | '"' | '|' | '?' | '*'
        ) || c.is_control()
            || is_bidi_or_format(c)
    }) {
        return None;
    }
    if name == "." || name == ".." {
        return None;
    }
    let stem = name.split('.').next().unwrap_or(name);
    if RESERVED.iter().any(|r| r.eq_ignore_ascii_case(stem)) {
        return None;
    }
    // A trailing space or dot is stripped by Windows and can defeat checks.
    if name.ends_with(' ') || name.ends_with('.') {
        return None;
    }
    Some(name.to_owned())
}

/// The phase of a receiving file transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferPhase {
    /// Offered, awaiting the user's consent.
    Offered,
    /// Consented; bytes may flow.
    Accepted,
    /// Fully received and hash-verified; the file may be committed.
    Complete,
    /// Rejected, cancelled, over-quota, overflowed, or hash-mismatched.
    Cancelled,
}

/// A receiving file transfer with consent, quota, and atomic-commit safety.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileTransfer {
    phase: TransferPhase,
    total: u64,
    received: u64,
}

impl FileTransfer {
    /// Begin an offered transfer of `total` bytes. If `total` exceeds `quota`
    /// the transfer is immediately cancelled (rejected) before any consent.
    pub fn offer(total: u64, quota: u64) -> Self {
        let phase = if total > quota {
            TransferPhase::Cancelled
        } else {
            TransferPhase::Offered
        };
        Self {
            phase,
            total,
            received: 0,
        }
    }

    /// The current phase.
    pub fn phase(&self) -> TransferPhase {
        self.phase
    }

    /// Bytes received so far.
    pub fn received(&self) -> u64 {
        self.received
    }

    /// Give explicit consent (Offered -> Accepted). Bytes only flow after this.
    pub fn accept(&mut self) -> bool {
        if self.phase == TransferPhase::Offered {
            self.phase = TransferPhase::Accepted;
            true
        } else {
            false
        }
    }

    /// Receive a chunk of `len` bytes. Rejected unless Accepted; a chunk that
    /// would exceed the declared total cancels the transfer (no overflow).
    pub fn receive(&mut self, len: u64) -> bool {
        if self.phase != TransferPhase::Accepted {
            return false;
        }
        let next = self.received.saturating_add(len);
        if next > self.total {
            self.phase = TransferPhase::Cancelled;
            return false;
        }
        self.received = next;
        true
    }

    /// Whether every declared byte has been received.
    pub fn fully_received(&self) -> bool {
        self.phase == TransferPhase::Accepted && self.received == self.total
    }

    /// Atomically finalize: commit to `Complete` only when every byte arrived
    /// AND the content hash matches; otherwise cancel (no partial or unverified
    /// file is committed). Returns whether the transfer committed.
    pub fn commit(&mut self, hash_matches: bool) -> bool {
        if self.phase == TransferPhase::Accepted && self.received == self.total && hash_matches {
            self.phase = TransferPhase::Complete;
            true
        } else if self.phase == TransferPhase::Accepted {
            self.phase = TransferPhase::Cancelled;
            false
        } else {
            false
        }
    }

    /// Cancel an in-flight (offered/accepted) transfer.
    pub fn cancel(&mut self) {
        if matches!(self.phase, TransferPhase::Offered | TransferPhase::Accepted) {
            self.phase = TransferPhase::Cancelled;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_names_pass_and_unsafe_names_are_rejected() {
        assert_eq!(safe_dest_name("movie.mkv").as_deref(), Some("movie.mkv"));
        assert_eq!(safe_dest_name("a b c.txt").as_deref(), Some("a b c.txt"));
        // Traversal and separators.
        assert_eq!(safe_dest_name(".."), None);
        assert_eq!(safe_dest_name("."), None);
        assert_eq!(safe_dest_name("../etc/passwd"), None);
        assert_eq!(safe_dest_name("dir/file.txt"), None);
        assert_eq!(safe_dest_name("dir\\file.txt"), None);
        assert_eq!(safe_dest_name("C:\\abs"), None);
        // NUL / control / empty / over-long.
        assert_eq!(safe_dest_name("a\0b"), None);
        assert_eq!(safe_dest_name(""), None);
        assert_eq!(safe_dest_name(&"x".repeat(256)), None);
        // Reserved device names (with/without extension, any case).
        assert_eq!(safe_dest_name("CON"), None);
        assert_eq!(safe_dest_name("nul.txt"), None);
        assert_eq!(safe_dest_name("Com1.log"), None);
        // Trailing space/dot tricks.
        assert_eq!(safe_dest_name("evil.exe "), None);
        assert_eq!(safe_dest_name("evil."), None);
    }

    #[test]
    fn colon_and_windows_illegal_chars_are_rejected() {
        // Drive-relative (`C:evil` has a drive prefix but no root) would replace
        // the download base dir on Path::join; the colon must be rejected.
        assert_eq!(safe_dest_name("C:evil.exe"), None);
        // NTFS Alternate Data Stream: bytes hidden on a benign-looking name.
        assert_eq!(safe_dest_name("readme.txt:hidden"), None);
        for bad in ["a<b", "a>b", "a\"b", "a|b", "a?b", "a*b"] {
            assert_eq!(safe_dest_name(bad), None, "{bad} should be rejected");
        }
    }

    #[test]
    fn bidi_and_format_controls_are_rejected() {
        // U+202E RIGHT-TO-LEFT OVERRIDE renders "photo\u{202E}gpj.exe" as
        // "photoexe.jpg" in a consent dialog while writing an .exe.
        assert_eq!(safe_dest_name("photo\u{202E}gpj.exe"), None);
        assert_eq!(safe_dest_name("a\u{200B}b.txt"), None); // zero-width space
        assert_eq!(safe_dest_name("a\u{FEFF}b.txt"), None); // BOM / ZWNBSP
        // A plain colon-free ASCII name with none of these still passes.
        assert_eq!(safe_dest_name("photo.jpg").as_deref(), Some("photo.jpg"));
    }
    #[test]
    fn over_quota_offer_is_cancelled_before_consent() {
        let t = FileTransfer::offer(2_000, 1_000);
        assert_eq!(t.phase(), TransferPhase::Cancelled);
    }

    #[test]
    fn bytes_require_consent_first() {
        let mut t = FileTransfer::offer(100, 1_000);
        assert_eq!(t.phase(), TransferPhase::Offered);
        // No consent yet: chunks are rejected.
        assert!(!t.receive(10));
        assert_eq!(t.received(), 0);
        assert!(t.accept());
        assert!(t.receive(10));
        assert_eq!(t.received(), 10);
    }

    #[test]
    fn overflow_cancels_the_transfer() {
        let mut t = FileTransfer::offer(100, 1_000);
        t.accept();
        assert!(t.receive(60));
        // 60 + 60 = 120 > 100 declared -> cancelled, no overflow.
        assert!(!t.receive(60));
        assert_eq!(t.phase(), TransferPhase::Cancelled);
        assert_eq!(t.received(), 60);
    }

    #[test]
    fn atomic_commit_requires_all_bytes_and_a_matching_hash() {
        // Incomplete commit cancels.
        let mut t = FileTransfer::offer(100, 1_000);
        t.accept();
        t.receive(50);
        assert!(!t.commit(true)); // not all bytes
        assert_eq!(t.phase(), TransferPhase::Cancelled);

        // Full but wrong hash cancels (no unverified file kept).
        let mut t = FileTransfer::offer(100, 1_000);
        t.accept();
        t.receive(100);
        assert!(t.fully_received());
        assert!(!t.commit(false));
        assert_eq!(t.phase(), TransferPhase::Cancelled);

        // Full and matching hash commits.
        let mut t = FileTransfer::offer(100, 1_000);
        t.accept();
        t.receive(100);
        assert!(t.commit(true));
        assert_eq!(t.phase(), TransferPhase::Complete);
    }

    #[test]
    fn cancel_stops_an_in_flight_transfer() {
        let mut t = FileTransfer::offer(100, 1_000);
        t.accept();
        t.receive(20);
        t.cancel();
        assert_eq!(t.phase(), TransferPhase::Cancelled);
        assert!(!t.receive(10));
    }
}
