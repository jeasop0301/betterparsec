//! Multi-monitor output registry (G014). Assigns each physical monitor a stable
//! identity derived from its EDID, so a client's selected output and its
//! coordinates never go stale when the OS renumbers displays on reorder or
//! unplug/replug. Bounds the number of concurrent individual streams and offers
//! a DPI-aware coordinate transform between outputs.
//!
//! Foundation module: live enumeration (real DXGI/Win32 monitor discovery),
//! per-output capture, and the physical 2–3 monitor campaign are the
//! physically-gated part of G014. This state machine is pure and headless
//! unit-tested; `#![allow(dead_code)]` matches the repo's deferred-wire-in
//! pattern.
#![allow(dead_code)]

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// Stable EDID-derived output identity (FNV-1a over the EDID bytes). The same
/// monitor keeps its id across enumeration reorder and unplug/replug because its
/// EDID is stable; distinct monitors get distinct ids.
pub fn edid_output_id(edid: &[u8]) -> u64 {
    let mut hash = FNV_OFFSET;
    for &byte in edid {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// One enumerated output sample from the OS display list.
#[derive(Debug, Clone, Copy)]
pub struct OutputSample<'a> {
    pub edid: &'a [u8],
    pub os_index: u32,
    pub dpi: u32,
}

/// A tracked output: its stable id plus the latest OS-reported position/DPI and
/// connection/stream state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputEntry {
    pub id: u64,
    pub os_index: u32,
    pub dpi: u32,
    pub connected: bool,
    pub streaming: bool,
}

/// Reorder/hotplug-stable registry of outputs with a concurrent-stream cap.
#[derive(Debug, Clone)]
pub struct OutputRegistry {
    outputs: Vec<OutputEntry>,
    max_streams: usize,
}

impl OutputRegistry {
    /// A registry allowing at most `max_streams` concurrent individual streams.
    pub fn new(max_streams: usize) -> Self {
        Self {
            outputs: Vec::new(),
            max_streams,
        }
    }

    /// Reconcile the current OS enumeration. Each output keeps its stable
    /// EDID-derived id across reorder; an output absent from `samples` is marked
    /// disconnected (its id is retained so a replug restores it) and its stream
    /// is stopped.
    pub fn enumerate(&mut self, samples: &[OutputSample]) {
        for entry in &mut self.outputs {
            entry.connected = false;
        }
        for sample in samples {
            let id = edid_output_id(sample.edid);
            if let Some(entry) = self.outputs.iter_mut().find(|e| e.id == id) {
                entry.os_index = sample.os_index;
                entry.dpi = sample.dpi;
                entry.connected = true;
            } else {
                self.outputs.push(OutputEntry {
                    id,
                    os_index: sample.os_index,
                    dpi: sample.dpi,
                    connected: true,
                    streaming: false,
                });
            }
        }
        for entry in &mut self.outputs {
            if !entry.connected {
                entry.streaming = false;
            }
        }
    }

    /// The tracked entry for `id`, if it has ever been enumerated.
    pub fn get(&self, id: u64) -> Option<&OutputEntry> {
        self.outputs.iter().find(|e| e.id == id)
    }

    /// Number of currently connected outputs.
    pub fn connected_count(&self) -> usize {
        self.outputs.iter().filter(|e| e.connected).count()
    }

    /// Number of outputs currently streaming.
    pub fn streaming_count(&self) -> usize {
        self.outputs.iter().filter(|e| e.streaming).count()
    }

    /// Begin an individual stream for `id`. Fails when the output is unknown or
    /// disconnected, already streaming, or the concurrent-stream cap is reached.
    pub fn try_stream(&mut self, id: u64) -> bool {
        if self.streaming_count() >= self.max_streams {
            return false;
        }
        if let Some(entry) = self.outputs.iter_mut().find(|e| e.id == id)
            && entry.connected
            && !entry.streaming
        {
            entry.streaming = true;
            return true;
        }
        false
    }

    /// Stop an individual stream for `id`. Returns `true` if one was streaming.
    pub fn stop_stream(&mut self, id: u64) -> bool {
        if let Some(entry) = self.outputs.iter_mut().find(|e| e.id == id)
            && entry.streaming
        {
            entry.streaming = false;
            return true;
        }
        false
    }

    /// Transform a point from output `from`'s DPI space into output `to`'s.
    /// `None` when either output is unknown or `from` has zero DPI.
    pub fn transform_point(&self, from: u64, to: u64, x: i32, y: i32) -> Option<(i32, i32)> {
        let src = self.get(from)?;
        let dst = self.get(to)?;
        if src.dpi == 0 {
            return None;
        }
        let scale = |v: i32| (i64::from(v) * i64::from(dst.dpi) / i64::from(src.dpi)) as i32;
        Some((scale(x), scale(y)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EDID_A: &[u8] = b"MONITOR-A-serial-0001";
    const EDID_B: &[u8] = b"MONITOR-B-serial-0002";
    const EDID_C: &[u8] = b"MONITOR-C-serial-0003";

    fn sample(edid: &[u8], os_index: u32, dpi: u32) -> OutputSample<'_> {
        OutputSample {
            edid,
            os_index,
            dpi,
        }
    }

    #[test]
    fn edid_id_is_stable_and_distinct() {
        assert_eq!(edid_output_id(EDID_A), edid_output_id(EDID_A));
        assert_ne!(edid_output_id(EDID_A), edid_output_id(EDID_B));
    }

    #[test]
    fn ids_survive_enumeration_reorder() {
        let mut reg = OutputRegistry::new(4);
        reg.enumerate(&[sample(EDID_A, 0, 96), sample(EDID_B, 1, 96)]);
        let id_a = edid_output_id(EDID_A);
        let id_b = edid_output_id(EDID_B);
        assert_eq!(reg.get(id_a).map(|e| e.os_index), Some(0));
        assert_eq!(reg.get(id_b).map(|e| e.os_index), Some(1));
        // OS renumbers the displays (reorder): ids stay, os_index updates.
        reg.enumerate(&[sample(EDID_B, 0, 96), sample(EDID_A, 1, 96)]);
        assert_eq!(reg.get(id_a).map(|e| e.os_index), Some(1));
        assert_eq!(reg.get(id_b).map(|e| e.os_index), Some(0));
        assert_eq!(reg.connected_count(), 2);
    }

    #[test]
    fn hotplug_retains_id_and_stops_stream_then_restores() {
        let mut reg = OutputRegistry::new(4);
        reg.enumerate(&[sample(EDID_A, 0, 96), sample(EDID_B, 1, 96)]);
        let id_b = edid_output_id(EDID_B);
        assert!(reg.try_stream(id_b));
        assert!(reg.get(id_b).expect("id_b present after stream").streaming);
        // Unplug B: it becomes disconnected, its stream stops, id retained.
        reg.enumerate(&[sample(EDID_A, 0, 96)]);
        let b = reg.get(id_b).expect("id retained");
        assert!(!b.connected);
        assert!(!b.streaming);
        // Replug B: same id, reconnected.
        reg.enumerate(&[sample(EDID_A, 0, 96), sample(EDID_B, 2, 120)]);
        let b = reg.get(id_b).expect("id_b present after replug");
        assert!(b.connected);
        assert_eq!(b.os_index, 2);
        assert_eq!(b.dpi, 120);
    }

    #[test]
    fn concurrent_stream_cap_and_gating() {
        let mut reg = OutputRegistry::new(2);
        reg.enumerate(&[
            sample(EDID_A, 0, 96),
            sample(EDID_B, 1, 96),
            sample(EDID_C, 2, 96),
        ]);
        let (a, b, c) = (
            edid_output_id(EDID_A),
            edid_output_id(EDID_B),
            edid_output_id(EDID_C),
        );
        assert!(reg.try_stream(a));
        assert!(reg.try_stream(b));
        // Cap reached: the third individual stream is denied.
        assert!(!reg.try_stream(c));
        assert_eq!(reg.streaming_count(), 2);
        // Already streaming is not double-counted.
        assert!(!reg.try_stream(a));
        // Stopping one frees a slot.
        assert!(reg.stop_stream(a));
        assert!(reg.try_stream(c));
        // Unknown / disconnected outputs cannot stream.
        assert!(!reg.try_stream(0xdead_beef));
    }

    #[test]
    fn dpi_transform_scales_between_outputs() {
        let mut reg = OutputRegistry::new(4);
        reg.enumerate(&[sample(EDID_A, 0, 96), sample(EDID_B, 1, 192)]);
        let (a, b) = (edid_output_id(EDID_A), edid_output_id(EDID_B));
        // 96 -> 192 DPI doubles the coordinates.
        assert_eq!(reg.transform_point(a, b, 100, 50), Some((200, 100)));
        // 192 -> 96 halves them.
        assert_eq!(reg.transform_point(b, a, 200, 100), Some((100, 50)));
        // Unknown output yields None.
        assert_eq!(reg.transform_point(a, 0xdead, 1, 1), None);
    }
}
