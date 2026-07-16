//! G004 deterministic fault matrix for `RxCore` (client-transport public
//! API only — no `.gjc`, no host/streamer changes; those cells are covered
//! by the G003 sender tests and by G007's live two-machine matrix).
//!
//! Mirrors, at the `RxCore` layer, the same seeded scenarios exercised at
//! the raw-`FecDecoder` layer by `transport-core/src/bin/fec_rig.rs`
//! (Bernoulli + Gilbert-Elliott + reorder + duplication fault models) and
//! at the TS `FecDecodePipe` layer by `tests/fec_fault_matrix.test.mjs`.
//!
//! Public-API-only note on the 16 MiB retained-bytes invariant: `RxCore`'s
//! `rx: Mutex<VideoReceiver>` and `VideoReceiver`'s own `decoder: FecDecoder`
//! are both private fields — there is no `pub` passthrough of
//! `FecDecoder::accounting()` through `RxCore`, and this suite deliberately
//! does not add one just for a test (that would be exactly the dead-seam
//! surface the project avoids). The byte-cap contract is instead proven
//! directly against `transport_core::fec::FecDecoder::accounting()` in
//! `fec_rig.rs` (same Gilbert-Elliott/reorder/duplication fault models),
//! where the lockstep test `sixteen_mib_cap_matches_the_real_receiver_cap`
//! also pins `RX_DECODER_MAX_BYTES` to the documented 16 MiB figure. At this
//! layer the bound is cross-checked only structurally: the random-loss cells
//! assert their total wire bytes stay below the cap by construction.
//!
//! Every scenario is a `#[test]` fn, deterministic (fixed seed, no
//! wall-clock — `now_ms` is a synthetic monotonic counter), and bounded
//! (asserted wall-clock runtime ceiling, generous relative to the tiny
//! deterministic workloads here).

use std::num::NonZeroU32;
use std::time::{Duration, Instant};

use client_transport::capi::RxCore;
use client_transport::frame_queue::{DEFAULT_FRAME_CAP, VideoEvent};
use transport_core::fec::{FecConfig, FecEncoder, Symbol};
use transport_core::fec_wire::{
    Epoch, V2Symbol, chunk_frame, chunk_frame_v2, encode_symbol_msg, encode_symbol_msg_v2,
};
use transport_core::video_rx::{DiscontinuityReason, RX_DECODER_MAX_BYTES};

/// Scenario runtime ceiling. Every scenario here pushes at most a few
/// hundred small messages through pure in-process state machines — this is
/// generous headroom against a real regression (accidental blocking wait,
/// quadratic reassembly, etc.), not a tight perf budget.
const SCENARIO_BUDGET: Duration = Duration::from_secs(5);

// ── Deterministic PRNG (SplitMix64 — mirrors transport-core/src/bin/fec_rig.rs) ─

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    fn next_below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next_u64() as usize) % n
        }
    }
}

// ── Gilbert-Elliott burst-loss channel (documented parameters) ─────────────

/// Two-state Markov loss channel. `steady_p` is the target long-run loss
/// probability, `mean_burst` the average consecutive-drop run length once in
/// the bad state (solved the same way as `fec_rig.rs`'s `LossModel::Burst`);
/// unlike a plain Gilbert model, both states carry their own drop
/// probability (`p_bad`/`p_good`), which is the Gilbert-Elliott
/// generalisation the assignment calls for.
#[derive(Clone, Copy)]
struct GilbertElliott {
    p_good: f64,
    p_bad: f64,
    mean_burst: f64,
    steady_p: f64,
}

struct GeChannel {
    model: GilbertElliott,
    in_bad: bool,
}

impl GeChannel {
    fn new(model: GilbertElliott) -> Self {
        Self {
            model,
            in_bad: false,
        }
    }

    fn drop(&mut self, rng: &mut Rng) -> bool {
        let GilbertElliott {
            p_good,
            p_bad,
            mean_burst,
            steady_p,
        } = self.model;
        let b2g = 1.0 / mean_burst.max(1.0);
        let g2b = (steady_p / (1.0 - steady_p).max(1e-9)) * b2g;
        if self.in_bad {
            if rng.next_f64() < b2g {
                self.in_bad = false;
            }
        } else if rng.next_f64() < g2b {
            self.in_bad = true;
        }
        let p = if self.in_bad { p_bad } else { p_good };
        rng.next_f64() < p
    }
}

// ── Wire-stream builders ────────────────────────────────────────────────────

fn frame_payload(frame_idx: u32, len: usize) -> Vec<u8> {
    (0..len as u32)
        .map(|i| (i.wrapping_mul(31).wrapping_add(frame_idx * 7) & 0xFF) as u8)
        .collect()
}

/// Builds a v1 wire stream (source + repair messages, emission order) for
/// `frame_count` frames (frame 0 is the only keyframe), each chunked to
/// force multiple chunks per frame.
fn build_v1_stream(frame_count: u32, ratio: (u8, u8)) -> Vec<Vec<u8>> {
    let mut enc = FecEncoder::new(FecConfig {
        redundancy_numerator: ratio.0,
        redundancy_denominator: ratio.1,
        window_max_symbols: 64,
        window_max_bytes: 1 << 20,
    });
    let mut seq = 0u32;
    let mut wire = Vec::new();
    for f in 0..frame_count {
        let data = frame_payload(f, 3000);
        for chunk in chunk_frame(f, f == 0, (f + 1) * 16_667, &data) {
            let out = enc.push_source(seq, &chunk);
            wire.push(encode_symbol_msg(&out.source));
            for rep in &out.repairs {
                wire.push(encode_symbol_msg(rep));
            }
            seq += 1;
        }
    }
    wire
}

fn to_v2_msg(epoch: Epoch, sym: Symbol) -> Vec<u8> {
    let v2 = match sym {
        Symbol::Source { seq, payload } => V2Symbol::Source {
            epoch,
            seq,
            payload,
        },
        Symbol::Repair {
            repair_seq,
            window_base,
            window_end,
            payload,
        } => V2Symbol::Repair {
            epoch,
            repair_seq,
            window_base,
            window_end,
            payload,
        },
    };
    encode_symbol_msg_v2(&v2).expect("encoder-produced symbol is always a valid v2 wire message")
}

/// Builds a v2 wire stream for `frame_count` frames under `epoch`.
fn build_v2_stream(epoch: Epoch, frame_count: u32, ratio: (u8, u8)) -> Vec<Vec<u8>> {
    let mut enc = FecEncoder::new(FecConfig {
        redundancy_numerator: ratio.0,
        redundancy_denominator: ratio.1,
        window_max_symbols: 64,
        window_max_bytes: 1 << 20,
    });
    let mut seq = 0u32;
    let mut wire = Vec::new();
    for f in 0..frame_count {
        let data = frame_payload(f, 1600);
        for chunk in chunk_frame_v2(f, f == 0, (f + 1) * 16_667, &data)
            .expect("payload within ENCODED_FRAME_MAX")
        {
            let out = enc.push_source(seq, &chunk);
            wire.push(to_v2_msg(epoch, out.source));
            for rep in out.repairs {
                wire.push(to_v2_msg(epoch, rep));
            }
            seq += 1;
        }
    }
    wire
}

/// Applies loss (Bernoulli xor Gilbert-Elliott), then reorder (deterministic
/// sliding-window shuffle), then duplication faults, in that order, seeded
/// from a single `seed` so the whole scenario is reproducible byte-for-byte.
fn apply_faults(
    wire: Vec<Vec<u8>>,
    seed: u64,
    bernoulli_p: f64,
    ge: Option<GilbertElliott>,
    reorder_window: usize,
    dup_p: f64,
) -> Vec<Vec<u8>> {
    let mut rng = Rng::new(seed);
    let mut ge_ch = ge.map(GeChannel::new);

    let mut survivors = Vec::with_capacity(wire.len());
    for msg in wire {
        let dropped = if let Some(ch) = ge_ch.as_mut() {
            ch.drop(&mut rng)
        } else {
            rng.next_f64() < bernoulli_p
        };
        if !dropped {
            survivors.push(msg);
        }
    }

    if reorder_window > 1 {
        let mut i = 0;
        while i + reorder_window <= survivors.len() {
            let j = i + rng.next_below(reorder_window);
            survivors.swap(i, j);
            i += 1;
        }
    }

    let mut out = Vec::with_capacity(survivors.len());
    for msg in survivors {
        out.push(msg.clone());
        if rng.next_f64() < dup_p {
            out.push(msg);
        }
    }
    out
}

// ── Harness: drives RxCore and enforces the cross-scenario invariants ─────

/// Drives one `RxCore` through a wire-message sequence, popping every queued
/// [`VideoEvent`] after each push (in wire order) and enforcing, on every
/// scenario:
///   - a [`VideoEvent::Discontinuity`] always reports the *same* recovery
///     `(generation, epoch)` that `RxCore::recovery()` reports immediately
///     after it is popped (structural "discontinuity precedes/opens
///     recovery atomically" check, at the queue-consumer's vantage point);
///   - zero delta frames are ever observed while a recovery is open (the
///     decode-side handshake contract) — the harness simulates the decode
///     side by acknowledging the first key frame it sees after an open
///     recovery, exactly like `app-native`'s `DecodeState::on_meta`.
struct Harness {
    core: RxCore,
    now_ms: u64,
    open_recovery: Option<(u64, u32)>,
}

impl Harness {
    fn new() -> Self {
        Self {
            core: RxCore::new(0),
            now_ms: 0,
            open_recovery: None,
        }
    }

    /// Feeds every message through `on_message`, draining and
    /// invariant-checking the queue after each push. Returns every event
    /// observed, in order.
    fn feed(&mut self, msgs: &[Vec<u8>]) -> Vec<VideoEvent> {
        let mut all = Vec::new();
        for m in msgs {
            self.now_ms += 2;
            self.core.on_message(m, self.now_ms);
            while let Some(ev) = self.core.try_event() {
                self.check_invariants(&ev);
                all.push(ev);
            }
        }
        all
    }

    fn check_invariants(&mut self, ev: &VideoEvent) {
        match ev {
            VideoEvent::Discontinuity {
                generation, epoch, ..
            } => {
                assert_eq!(
                    self.core.recovery(),
                    Some((*generation, *epoch)),
                    "a popped discontinuity must report the recovery RxCore has open"
                );
                self.open_recovery = Some((*generation, *epoch));
            }
            VideoEvent::Frame(unit) => {
                if let Some((generation, epoch)) = self.open_recovery {
                    assert!(
                        unit.is_key,
                        "delta frame {} delivered while recovery (gen {generation}, epoch {epoch}) was open",
                        unit.frame_id
                    );
                    assert!(
                        self.core
                            .acknowledge_decoded_key(generation, epoch, unit.frame_id),
                        "the qualifying key frame must close the recovery it satisfies"
                    );
                    assert_eq!(self.core.recovery(), None, "ack must close recovery");
                    self.open_recovery = None;
                }
            }
        }
    }
}

fn frames_of(events: &[VideoEvent]) -> Vec<&transport_core::video_rx::DecodeUnit> {
    events.iter().filter_map(VideoEvent::as_frame).collect()
}

fn discontinuities_of(events: &[VideoEvent]) -> Vec<(u64, u32, DiscontinuityReason)> {
    events
        .iter()
        .filter_map(|e| match e {
            VideoEvent::Discontinuity {
                generation,
                epoch,
                reason,
            } => Some((*generation, *epoch, *reason)),
            VideoEvent::Frame(_) => None,
        })
        .collect()
}

fn run_bounded(f: impl FnOnce()) {
    let start = Instant::now();
    f();
    assert!(
        start.elapsed() < SCENARIO_BUDGET,
        "scenario exceeded its {SCENARIO_BUDGET:?} runtime budget"
    );
}

// ── Cell: clean stream ──────────────────────────────────────────────────────

#[test]
fn clean_stream_all_frames_deliver_in_order_zero_discontinuities() {
    run_bounded(|| {
        let wire = build_v1_stream(12, (1, 4));
        let mut h = Harness::new();
        let events = h.feed(&wire);

        assert!(
            discontinuities_of(&events).is_empty(),
            "a lossless stream must never discontinue"
        );
        let frames = frames_of(&events);
        assert_eq!(frames.len(), 12);
        for (i, unit) in frames.iter().enumerate() {
            assert_eq!(unit.frame_id, i as u32);
            assert_eq!(unit.data, frame_payload(i as u32, 3000));
        }
    });
}

// ── Cell: reorder ────────────────────────────────────────────────────────────

#[test]
fn reorder_within_window_gates_then_still_makes_forward_progress() {
    run_bounded(|| {
        // NOTE (finding, not a test-authoring bug): the underlying
        // `FecDecoder::advance_contiguous` cannot distinguish "arrived out of
        // order" from "lost" beyond one heuristic — an open gap that gets
        // *bounded* by a later-arriving seq is declared permanently
        // unrecoverable (see fec.rs's `advance_contiguous` doc comment: "the
        // whole run was never healed — it is abandoned/skipped"). Sustained
        // reordering therefore surfaces as a real `FecEviction`
        // discontinuity at the receiver, not a transparent recovery — this
        // cell proves that contract (gate-then-progress, same shape as the
        // Gilbert-Elliott burst cell), not "every frame survives unscathed".
        let wire = build_v1_stream(10, (1, 2));
        let faulted = apply_faults(wire.clone(), 0xF00D_0001, 0.0, None, 3, 0.0);
        assert_ne!(
            faulted, wire,
            "the reorder fault must actually permute the stream (seeded no-op guard)"
        );
        let mut h = Harness::new();
        let events = h.feed(&faulted);

        let frames = frames_of(&events);
        assert!(
            !frames.is_empty(),
            "reordering must not stall the stream permanently"
        );
        let mut last = None;
        for unit in &frames {
            if let Some(prev) = last {
                assert!(unit.frame_id > prev, "frame ids must strictly increase");
            }
            assert_eq!(
                unit.data,
                frame_payload(unit.frame_id, 3000),
                "a delivered frame's bytes must still be byte-identical"
            );
            last = Some(unit.frame_id);
        }
    });
}

// ── Cell: random (Bernoulli) loss 0.1% / 1% / 2% / 5% ───────────────────────

fn random_loss_scenario(loss_p: f64, seed: u64) {
    run_bounded(|| {
        let wire = build_v1_stream(40, (1, 2));
        let total_wire_bytes: usize = wire.iter().map(Vec::len).sum();
        assert!(
            total_wire_bytes < RX_DECODER_MAX_BYTES as usize,
            "scenario stays well under the 16 MiB decoder byte cap by construction"
        );
        let faulted = apply_faults(wire.clone(), seed, loss_p, None, 1, 0.0);
        // Seeded no-op guard: at >=1% the fixed seeds provably drop
        // something; at 0.1% this seed may legitimately drop nothing over a
        // stream this short, so only the non-growth invariant applies there.
        if loss_p >= 0.01 {
            assert!(
                faulted.len() < wire.len(),
                "the loss fault must actually drop messages (seeded no-op guard)"
            );
        }
        assert!(faulted.len() <= wire.len());
        let mut h = Harness::new();
        let events = h.feed(&faulted);

        let frames = frames_of(&events);
        // 1/2 redundancy comfortably heals up to 5% independent loss; every
        // frame must still complete (harness already proves zero deltas
        // leaked while any recovery it did open stayed open).
        assert_eq!(
            frames.len(),
            40,
            "1/2 FEC ratio must recover every frame at {}% independent loss",
            loss_p * 100.0
        );
    });
}

#[test]
fn random_loss_0_1_percent_recovers_via_fec() {
    random_loss_scenario(0.001, 0xF00D_1001);
}

#[test]
fn random_loss_1_percent_recovers_via_fec() {
    random_loss_scenario(0.01, 0xF00D_1002);
}

#[test]
fn random_loss_2_percent_recovers_via_fec() {
    random_loss_scenario(0.02, 0xF00D_1003);
}

#[test]
fn random_loss_5_percent_recovers_via_fec() {
    random_loss_scenario(0.05, 0xF00D_1004);
}

// ── Cell: Gilbert-Elliott burst loss 20% ────────────────────────────────────

#[test]
fn gilbert_elliott_burst_loss_20_percent_recovers_or_gates_cleanly() {
    run_bounded(|| {
        let wire = build_v1_stream(60, (1, 2));
        let ge = GilbertElliott {
            p_good: 0.0,
            p_bad: 1.0,
            mean_burst: 4.0,
            steady_p: 0.20,
        };
        let faulted = apply_faults(wire, 0xF00D_2001, 0.0, Some(ge), 1, 0.0);
        let mut h = Harness::new();
        let events = h.feed(&faulted);

        // Bursty 20% loss can exceed what a 1/2 ratio heals in the worst
        // burst; the contract under test is not "always fully recovers" but
        // "never leaks a delta while recovery is open" (enforced by the
        // harness on every popped event) and "always makes forward
        // progress" (at least some frames deliver).
        let frames = frames_of(&events);
        assert!(
            !frames.is_empty(),
            "burst loss must not stall the stream permanently"
        );
        // Frame ids observed must be strictly increasing (no reorder/replay
        // leaked through as a duplicate/aliased frame id).
        let mut last = None;
        for unit in &frames {
            if let Some(prev) = last {
                assert!(unit.frame_id > prev, "frame ids must strictly increase");
            }
            last = Some(unit.frame_id);
        }
    });
}

// ── Cell: malformed / truncated messages ────────────────────────────────────

#[test]
fn malformed_and_truncated_messages_are_silently_ignored() {
    run_bounded(|| {
        let mut h = Harness::new();
        // Empty, unknown-kind, and truncated-source messages must never
        // panic and must never produce an event.
        let garbage: Vec<Vec<u8>> = vec![
            vec![],
            vec![0xFF, 1, 2, 3],
            vec![0x00, 1, 2], // truncated v1 source (needs 5-byte header)
            vec![0x00, 0, 0, 0, 0, 1, 2], // valid header, short chunk payload
        ];
        let events = h.feed(&garbage);
        assert!(
            events.is_empty(),
            "malformed input must produce zero events"
        );

        // The receiver must still be usable afterwards: a legitimate single-
        // chunk key frame assembles normally. Uses a fresh seq (100, not 0):
        // the last garbage message above is a *valid* FEC source symbol
        // (seq 0) whose chunk payload is merely too short to be a chunk
        // header — the decoder legitimately consumes seq 0 (matching
        // video_rx.rs's own `malformed_messages_are_dropped` contract), so
        // re-using seq 0 here would be silently rejected as a duplicate.
        let chunk = chunk_frame(0, true, 1000, &[7, 8, 9]).remove(0);
        let msg = encode_symbol_msg(&Symbol::Source {
            seq: 100,
            payload: chunk,
        });
        let events = h.feed(&[msg]);
        let frames = frames_of(&events);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, [7, 8, 9]);
    });
}

// ── Cell: CRC-corrupt v2 frame ──────────────────────────────────────────────

#[test]
fn crc_corrupt_v2_frame_triggers_typed_discontinuity() {
    run_bounded(|| {
        let epoch = NonZeroU32::new(9).expect("nonzero");
        let mut h = Harness::new();
        assert!(h.core.configure_fec(2, Some(epoch)));

        let e = Epoch::new(9).expect("nonzero");
        let mut chunk = chunk_frame_v2(0, true, 0, &[1, 2, 3])
            .expect("valid v2 frame")
            .remove(0);
        *chunk.last_mut().expect("nonempty chunk") ^= 1; // flip one payload byte -> CRC mismatch
        let msg = to_v2_msg(
            e,
            Symbol::Source {
                seq: 0,
                payload: chunk,
            },
        );

        let events = h.feed(&[msg]);
        let discontinuities = discontinuities_of(&events);
        assert!(
            discontinuities
                .iter()
                .any(|(_, _, reason)| *reason == DiscontinuityReason::FrameCrc),
            "corrupted encoded-frame CRC must surface as FrameCrc, not silently drop"
        );
        assert!(
            frames_of(&events).is_empty(),
            "the corrupt frame itself must never deliver"
        );
    });
}

// ── Cell: queue overload (overflow reset ordering) ──────────────────────────

#[test]
fn queue_overflow_reset_precedes_its_retained_frame_and_opens_recovery() {
    run_bounded(|| {
        let mut h = Harness::new();
        let mut seq = 0u32;

        // Fill the queue to exactly its cap (16): frame 0 key, rest delta —
        // none of these trigger recovery on their own (no loss, no reorder).
        for f in 0..DEFAULT_FRAME_CAP as u32 {
            let chunk = chunk_frame(f, f == 0, f * 1000, &[f as u8]).remove(0);
            let msg = encode_symbol_msg(&Symbol::Source {
                seq,
                payload: chunk,
            });
            seq += 1;
            h.core.on_message(&msg, h.now_ms);
            h.now_ms += 1;
        }
        assert_eq!(
            h.core.recovery(),
            None,
            "filling the queue exactly to capacity must not itself open recovery"
        );

        // The (cap+1)th push overflows: clears the backlog, opens recovery,
        // and enqueues QueueOverflow immediately before this new (key) frame.
        let overflow_id = DEFAULT_FRAME_CAP as u32;
        let chunk = chunk_frame(overflow_id, true, overflow_id * 1000, &[0xEE]).remove(0);
        let msg = encode_symbol_msg(&Symbol::Source {
            seq,
            payload: chunk,
        });
        h.core.on_message(&msg, h.now_ms);

        let mut popped = Vec::new();
        while let Some(ev) = h.core.try_event() {
            h.check_invariants(&ev);
            popped.push(ev);
        }

        assert_eq!(
            popped.len(),
            2,
            "overflow must clear the stale backlog: exactly [reset, retained frame] survive"
        );
        assert!(matches!(
            popped[0],
            VideoEvent::Discontinuity {
                reason: DiscontinuityReason::QueueOverflow,
                ..
            }
        ));
        match &popped[1] {
            VideoEvent::Frame(unit) => assert_eq!(unit.frame_id, overflow_id),
            other => panic!("expected the retained frame, got {other:?}"),
        }
        // The harness's own check_invariants already acked this recovery
        // (the retained frame is a key), so it must now read closed.
        assert_eq!(h.core.recovery(), None);
    });
}

// ── Cell: same-epoch / new-epoch renegotiation ──────────────────────────────

#[test]
fn renegotiation_same_epoch_starts_a_clean_generation() {
    run_bounded(|| {
        let mut h = Harness::new();
        let epoch = NonZeroU32::new(7).expect("nonzero");
        assert!(h.core.configure_fec(2, Some(epoch)));

        let wire = build_v2_stream(Epoch::new(7).expect("nonzero"), 3, (0, 1));
        let first = h.feed(&wire);
        assert_eq!(frames_of(&first).len(), 3);

        // Renegotiate: fresh generation, same epoch value.
        h.core.begin_fec_negotiation();
        assert_eq!(
            h.core.recovery(),
            None,
            "a fresh generation carries no recovery"
        );
        assert!(
            h.core.try_event().is_none(),
            "no orphaned events from the prior generation"
        );
        assert_eq!(
            h.core.video_stats(),
            transport_core::video_rx::VideoReceiverStats::default(),
            "stats must reset for the new generation"
        );
        assert!(h.core.configure_fec(2, Some(epoch)));

        let wire2 = build_v2_stream(Epoch::new(7).expect("nonzero"), 3, (0, 1));
        let second = h.feed(&wire2);
        let frames = frames_of(&second);
        assert_eq!(frames.len(), 3);
        assert_eq!(
            frames[0].frame_id, 0,
            "the new generation restarts frame ids from 0"
        );
    });
}

#[test]
fn renegotiation_new_epoch_starts_a_clean_generation() {
    run_bounded(|| {
        let mut h = Harness::new();
        let epoch_a = NonZeroU32::new(7).expect("nonzero");
        assert!(h.core.configure_fec(2, Some(epoch_a)));
        let wire = build_v2_stream(Epoch::new(7).expect("nonzero"), 3, (0, 1));
        assert_eq!(frames_of(&h.feed(&wire)).len(), 3);

        h.core.begin_fec_negotiation();
        let epoch_b = NonZeroU32::new(8).expect("nonzero");
        assert!(h.core.configure_fec(2, Some(epoch_b)));

        // Epoch 7 traffic must now be rejected outright (wrong wire epoch).
        let stale = build_v2_stream(Epoch::new(7).expect("nonzero"), 1, (0, 1));
        assert!(
            frames_of(&h.feed(&stale)).is_empty(),
            "stale-epoch traffic must be rejected"
        );

        let wire_b = build_v2_stream(Epoch::new(8).expect("nonzero"), 3, (0, 1));
        let events = h.feed(&wire_b);
        let frames = frames_of(&events);
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[0].frame_id, 0);
    });
}

// ── Cell: failed-key (never acked) ──────────────────────────────────────────

#[test]
fn failed_key_never_acked_gates_deltas_forever() {
    run_bounded(|| {
        let core = RxCore::new(0);
        let mut seq = 0u32;

        // Force a discontinuity without ever completing a keyframe:
        // MAX_PENDING_FRAMES (8): the 9th distinct never-completing pending
        // frame trips the reassembly-eviction reset, matching
        // video_rx.rs's own pending-cap unit test.
        for i in 0..9u32 {
            let chunk = chunk_frame(i, false, i * 1000, &vec![i as u8; 4000]);
            // 4000 bytes / CHUNK_FRAGMENT_MAX(1182) -> multiple chunks; send
            // only the first so the frame never completes.
            let msg = encode_symbol_msg(&Symbol::Source {
                seq,
                payload: chunk[0].clone(),
            });
            seq += 1;
            core.on_message(&msg, u64::from(i));
        }
        let mut saw_discontinuity = false;
        while let Some(ev) = core.try_event() {
            if matches!(
                ev,
                VideoEvent::Discontinuity {
                    reason: DiscontinuityReason::ReassemblyEviction,
                    ..
                }
            ) {
                saw_discontinuity = true;
            }
        }
        assert!(
            saw_discontinuity,
            "9th pending frame must trip the reassembly cap"
        );
        assert!(core.poll_needs_idr(), "the reset must request an IDR");
        let (generation, epoch) = core
            .recovery()
            .expect("recovery must be open after the reset");

        // Now feed only delta frames, forever (bounded to a handful for the
        // test): none may ever be delivered, and recovery must stay open —
        // there is no key frame to close it.
        for i in 100..120u32 {
            let chunk = chunk_frame(i, false, i * 1000, &[i as u8]).remove(0);
            let msg = encode_symbol_msg(&Symbol::Source {
                seq,
                payload: chunk,
            });
            seq += 1;
            core.on_message(&msg, u64::from(i));
            assert!(
                core.try_event().is_none(),
                "a delta frame must never surface while recovery ({generation}, {epoch}) is open"
            );
        }
        assert_eq!(
            core.recovery(),
            Some((generation, epoch)),
            "recovery must remain open: no key frame ever arrived to ack it"
        );
        assert!(
            core.video_stats().frames_dropped_awaiting_idr >= 20,
            "every gated delta must count against frames_dropped_awaiting_idr"
        );
    });
}

// ── Cell: 100 sequential lease generations ──────────────────────────────────

#[test]
fn hundred_sequential_lease_generations_carry_no_orphan_state() {
    run_bounded(|| {
        // `RxCore::acquire_session_lease`/`release_session_lease` are
        // crate-private (by design — see capi.rs's doc comment on
        // `LeaseState`), so an external integration test cannot drive the
        // real one-shot lease directly. What we *can* prove through the
        // public API is the contract that makes the lease one-shot
        // meaningful: a fresh receiver per reconnect starts with zero state
        // inherited from any predecessor, 100 times over with a distinct
        // deterministic seed each generation.
        for generation in 0..100u64 {
            let mut h = Harness {
                core: RxCore::new(generation),
                now_ms: 0,
                open_recovery: None,
            };
            assert_eq!(
                h.core.recovery(),
                None,
                "gen {generation}: fresh receiver has no recovery"
            );
            assert_eq!(
                h.core.frames_delivered(),
                0,
                "gen {generation}: fresh receiver delivered nothing yet"
            );
            assert!(
                !h.core.poll_needs_idr(),
                "gen {generation}: fresh receiver has no pending IDR"
            );
            assert!(
                h.core.try_event().is_none(),
                "gen {generation}: fresh receiver's queue is empty"
            );
            assert_eq!(
                h.core.video_stats(),
                transport_core::video_rx::VideoReceiverStats::default(),
                "gen {generation}: fresh receiver's stats are zeroed"
            );

            let wire = build_v1_stream(4, (1, 2));
            let faulted = apply_faults(wire, 0xF00D_3000 + generation, 0.02, None, 3, 0.05);
            let events = h.feed(&faulted);
            assert!(
                frames_of(&events).len() <= 4,
                "gen {generation}: must never observe more frames than were encoded"
            );
            assert_eq!(
                h.core.frames_delivered() as usize,
                frames_of(&events).len(),
                "gen {generation}: frames_delivered must match what the queue actually yielded"
            );
        }
    });
}
