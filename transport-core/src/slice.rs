//! Sub-frame slice reassembly (G023). A frame may be split into up to
//! [`MAX_SLICES`] slices so the host encode and the client decode/present can
//! overlap. This pure reassembler groups a frame's slices while enforcing the
//! two hard invariants: it never mixes epochs (a slice from a different epoch
//! drops the in-progress partial and re-arms recovery), and it never bypasses
//! recovery (while awaiting a key/IDR every delta slice is dropped). An
//! incomplete frame is dropped when a new frame or epoch arrives (partial-unit
//! handling); the caller falls back to whole-frame decode when slices do not
//! complete.
//!
//! Pure and I/O-free, matching the rest of transport-core. The slice count is
//! bounded by Sunshine's `multiFecFlags` 2-bit slice index (≤ 4 slices).

/// Maximum slices per frame (a 2-bit slice index on the wire).
pub const MAX_SLICES: u8 = 4;

/// The result of pushing one slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SliceOutcome {
    /// More slices are needed to complete the frame.
    Pending,
    /// Every slice of the frame has arrived.
    FrameComplete,
    /// A delta slice arrived while awaiting a key — dropped (recovery not
    /// bypassed).
    Gated,
    /// A new epoch was adopted; the reassembler is now awaiting a key.
    EpochReset,
    /// The slice index/count was invalid, or the slice count for the current
    /// frame changed — dropped.
    Malformed,
}

/// Groups a frame's slices while enforcing epoch isolation and the recovery gate.
#[derive(Debug, Clone)]
pub struct SliceReassembler {
    epoch: u32,
    awaiting_key: bool,
    frame_id: Option<u32>,
    slice_count: u8,
    got: u8,
    dropped_partial: u64,
}

impl SliceReassembler {
    /// A reassembler for `epoch`, initially awaiting a key (IDR) before any
    /// delta is admitted.
    pub fn new(epoch: u32) -> Self {
        Self {
            epoch,
            awaiting_key: true,
            frame_id: None,
            slice_count: 0,
            got: 0,
            dropped_partial: 0,
        }
    }

    /// The current epoch.
    pub fn epoch(&self) -> u32 {
        self.epoch
    }

    /// Whether a key/IDR is required before deltas are admitted.
    pub fn awaiting_key(&self) -> bool {
        self.awaiting_key
    }

    /// Count of incomplete frames dropped (partial-unit telemetry).
    pub fn dropped_partial(&self) -> u64 {
        self.dropped_partial
    }

    /// Re-arm recovery (e.g. after a detected loss): the in-progress partial is
    /// dropped and deltas are gated until the next key.
    pub fn request_recovery(&mut self) {
        self.drop_partial_if_incomplete();
        self.awaiting_key = true;
        self.frame_id = None;
        self.got = 0;
    }

    /// Push one slice of a frame.
    pub fn push(
        &mut self,
        epoch: u32,
        frame_id: u32,
        slice_index: u8,
        slice_count: u8,
        is_key: bool,
    ) -> SliceOutcome {
        if slice_count == 0 || slice_count > MAX_SLICES || slice_index >= slice_count {
            return SliceOutcome::Malformed;
        }

        // Epoch change: never mix epochs. Drop any partial, adopt the new epoch,
        // and re-arm recovery. A non-key first slice of the new epoch is gated.
        if epoch != self.epoch {
            self.drop_partial_if_incomplete();
            self.epoch = epoch;
            self.awaiting_key = true;
            self.frame_id = None;
            self.got = 0;
            self.slice_count = 0;
            if !is_key {
                return SliceOutcome::EpochReset;
            }
        }

        // Recovery gate: a delta slice while awaiting a key is dropped.
        if self.awaiting_key && !is_key {
            return SliceOutcome::Gated;
        }

        // New frame: drop any incomplete previous frame and start fresh.
        if self.frame_id != Some(frame_id) {
            self.drop_partial_if_incomplete();
            self.frame_id = Some(frame_id);
            self.slice_count = slice_count;
            self.got = 0;
        } else if self.slice_count != slice_count {
            return SliceOutcome::Malformed;
        }

        self.got |= 1u8 << slice_index;
        let full = (1u16 << slice_count) - 1;
        if u16::from(self.got) == full {
            if is_key {
                self.awaiting_key = false;
            }
            self.frame_id = None;
            self.got = 0;
            SliceOutcome::FrameComplete
        } else {
            SliceOutcome::Pending
        }
    }

    fn drop_partial_if_incomplete(&mut self) {
        if self.frame_id.is_some() && self.got != 0 {
            self.dropped_partial += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_slice_key_completes_and_clears_recovery() {
        let mut r = SliceReassembler::new(1);
        assert!(r.awaiting_key());
        assert_eq!(r.push(1, 10, 0, 1, true), SliceOutcome::FrameComplete);
        assert!(!r.awaiting_key());
    }

    #[test]
    fn delta_before_a_key_is_gated() {
        let mut r = SliceReassembler::new(1);
        assert_eq!(r.push(1, 10, 0, 1, false), SliceOutcome::Gated);
        assert!(r.awaiting_key());
    }

    #[test]
    fn multi_slice_frame_completes_out_of_order() {
        let mut r = SliceReassembler::new(1);
        // Key frame in two slices, arriving out of order.
        assert_eq!(r.push(1, 10, 1, 2, true), SliceOutcome::Pending);
        assert_eq!(r.push(1, 10, 0, 2, true), SliceOutcome::FrameComplete);
        assert!(!r.awaiting_key());
        // A subsequent delta frame now flows.
        assert_eq!(r.push(1, 11, 0, 2, false), SliceOutcome::Pending);
        assert_eq!(r.push(1, 11, 1, 2, false), SliceOutcome::FrameComplete);
    }

    #[test]
    fn malformed_slices_are_rejected() {
        let mut r = SliceReassembler::new(1);
        assert_eq!(r.push(1, 10, 0, 0, true), SliceOutcome::Malformed); // zero count
        assert_eq!(r.push(1, 10, 0, 5, true), SliceOutcome::Malformed); // over MAX
        assert_eq!(r.push(1, 10, 2, 2, true), SliceOutcome::Malformed); // index >= count
        // Same frame id with a changed slice count is malformed.
        assert_eq!(r.push(1, 10, 0, 2, true), SliceOutcome::Pending);
        assert_eq!(r.push(1, 10, 1, 3, true), SliceOutcome::Malformed);
    }

    #[test]
    fn epoch_change_never_mixes_and_re_arms_recovery() {
        let mut r = SliceReassembler::new(1);
        r.push(1, 10, 0, 1, true); // complete a key, recovery cleared
        assert!(!r.awaiting_key());
        // New epoch, non-key first slice: adopt epoch, re-arm recovery.
        assert_eq!(r.push(2, 20, 0, 1, false), SliceOutcome::EpochReset);
        assert_eq!(r.epoch(), 2);
        assert!(r.awaiting_key());
        // Delta under the new epoch is gated until a key.
        assert_eq!(r.push(2, 20, 0, 1, false), SliceOutcome::Gated);
        // The new epoch's key completes and clears recovery.
        assert_eq!(r.push(2, 21, 0, 1, true), SliceOutcome::FrameComplete);
        assert!(!r.awaiting_key());
    }

    #[test]
    fn partial_frame_is_dropped_on_new_epoch() {
        let mut r = SliceReassembler::new(1);
        r.push(1, 10, 0, 1, true); // clear recovery
        // Start a two-slice frame, only slice 0 arrives.
        assert_eq!(r.push(1, 11, 0, 2, false), SliceOutcome::Pending);
        // A key slice under a new epoch drops the partial and starts clean.
        assert_eq!(r.push(2, 20, 0, 1, true), SliceOutcome::FrameComplete);
        assert_eq!(r.dropped_partial(), 1);
    }

    #[test]
    fn request_recovery_gates_deltas_until_the_next_key() {
        let mut r = SliceReassembler::new(1);
        r.push(1, 10, 0, 1, true); // recovery cleared
        assert!(!r.awaiting_key());
        r.request_recovery();
        assert!(r.awaiting_key());
        assert_eq!(r.push(1, 11, 0, 1, false), SliceOutcome::Gated);
        assert_eq!(r.push(1, 12, 0, 1, true), SliceOutcome::FrameComplete);
        assert!(!r.awaiting_key());
    }
}
