//! FEC quantification rig — U2 P2 (docs/design/fec-framing.md §8, ROADMAP
//! U2 "Gate B/D 정량화 rig"). Drives the pure Tetrys decoder
//! ([`transport_core::fec`]) through a deterministic lossy channel and
//! reports recovery-rate-vs-redundancy curves that feed the adaptive-FEC
//! ratio decision (U2 P3). Headless and reproducible: a fixed seed per
//! cell makes every run byte-identical (same discipline as the
//! `nvenc-slice-probe` bin).
//!
//! What it measures: within one protection window, of the *source*
//! symbols the channel dropped, what fraction does the decoder heal at a
//! given redundancy ratio and loss model. Two channels:
//!   - independent (Bernoulli): each symbol dropped i.i.d. with prob p.
//!   - burst (Gilbert-Elliott two-state): a bad state with its own drop
//!     probability (the classic all-or-nothing Gilbert model is the
//!     `p_bad == 1.0` special case) at a tunable mean burst length and
//!     steady-state loss `steady_p` — the case FEC actually has to survive
//!     for video. G004 additionally sweeps deterministic reorder and
//!     duplication faults (see `fault_matrix` below), which are orthogonal
//!     to loss and exercised directly against `FecDecoder::accounting()`.
//!
//! Caveat (read before quoting absolute numbers): each trial is one
//! self-contained window, so the last few sources see fewer *following*
//! repairs than they would in a continuous stream (a repair only covers
//! sources already in its elastic window). The reported recovery% is
//! therefore a **lower bound** on steady-state — the *shape* (recovery vs
//! ratio, independent vs burst) is what drives the adaptive-ratio call,
//! not the last decimal.
//!
//! Run: `cargo run --release -p transport-core --bin fec-rig`

use transport_core::fec::{FecConfig, FecDecoder, FecEncoder, Symbol};

// ── Deterministic PRNG (SplitMix64) ─────────────────────────────────────────

/// Tiny reproducible PRNG — no external deps (transport-core is dep-free).
struct SplitMix64(u64);

impl SplitMix64 {
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

    /// Uniform in [0, 1).
    fn next_f64(&mut self) -> f64 {
        // Top 53 bits → [0,1) with full mantissa precision.
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

// ── Loss channel ────────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
enum LossModel {
    /// Each symbol dropped i.i.d. with probability `p` (0..1).
    Independent { p: f64 },
    /// Gilbert-Elliott two-state burst channel (generalizes the classic
    /// Gilbert model, which is the special case `p_good == 0.0`,
    /// `p_bad == 1.0`): `steady_p` is the target long-run loss probability,
    /// `mean_burst` the average consecutive-drop run length once in the bad
    /// state, `p_good`/`p_bad` the per-symbol drop probability while in
    /// each state (so the bad state need not be "always drops" — the
    /// Gilbert-Elliott generalization FEC actually has to survive when a
    /// "bad" radio period is merely worse, not total blackout).
    Burst {
        p_good: f64,
        p_bad: f64,
        steady_p: f64,
        mean_burst: f64,
    },
}

/// Per-trial mutable channel state (Gilbert good/bad); Independent is
/// stateless but shares the type for a uniform `drop` call.
struct Channel {
    model: LossModel,
    in_bad: bool,
}

impl Channel {
    fn new(model: LossModel) -> Self {
        Self {
            model,
            in_bad: false,
        }
    }

    /// Returns true if this symbol is dropped.
    fn drop(&mut self, rng: &mut SplitMix64) -> bool {
        match self.model {
            LossModel::Independent { p } => rng.next_f64() < p,
            LossModel::Burst {
                p_good,
                p_bad,
                steady_p,
                mean_burst,
            } => {
                // Steady-state π_bad = steady_p, mean bad run = mean_burst.
                //   bad→good = 1/mean_burst
                //   good→bad  = steady_p/((1-steady_p)·mean_burst)   (solves π_bad = steady_p)
                let b2g = 1.0 / mean_burst.max(1.0);
                let g2b = (steady_p / ((1.0 - steady_p).max(1e-9))) * b2g;
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
    }
}

// ── Trial ───────────────────────────────────────────────────────────────────

#[derive(Default, Clone, Copy)]
struct TrialStats {
    n_sources: u64,
    dropped_sources: u64,
    delivered_direct: u64,
    recovered: u64,
    repairs_emitted: u64,
    loss_spans: u64,
    loss_spans_recovered: u64,
}

/// One protection-window trial: encode `n_sources` symbols at the given
/// ratio, run every emitted symbol (source + repair) through the channel
/// in emission order, feed survivors to a fresh decoder, and read the
/// decoder's own counters back.
fn run_trial(
    ratio_num: u8,
    ratio_den: u8,
    n_sources: u32,
    symbol_size: usize,
    model: LossModel,
    seed: u64,
) -> TrialStats {
    let cfg = FecConfig {
        redundancy_numerator: ratio_num,
        redundancy_denominator: ratio_den,
        window_max_symbols: 64,
        window_max_bytes: 1 << 20,
    };
    let mut enc = FecEncoder::new(cfg);
    // Decoder window must span the whole trial so eviction never masks a
    // recovery: n_sources is kept below the 64-symbol cap by the caller.
    let mut dec = FecDecoder::new(64, 1 << 20);

    let mut rng = SplitMix64::new(seed);
    let mut ch = Channel::new(model);
    let mut st = TrialStats {
        n_sources: u64::from(n_sources),
        ..Default::default()
    };

    for seq in 0..n_sources {
        // Distinct per-seq payload so a mis-recovery can't accidentally
        // match: first 4 bytes carry the seq, rest is PRNG-free filler.
        let mut payload = vec![0u8; symbol_size];
        payload[..4].copy_from_slice(&seq.to_le_bytes());
        let out = enc.push_source(seq, &payload);

        // Source through the channel.
        if ch.drop(&mut rng) {
            st.dropped_sources += 1;
        } else {
            dec.push_symbol(out.source);
        }
        // Repairs emitted alongside this source.
        for repair in out.repairs {
            st.repairs_emitted += 1;
            if !ch.drop(&mut rng) {
                dec.push_symbol(repair);
            }
        }
    }

    let stats = dec.stats();
    st.delivered_direct = stats.source_symbols_received;
    st.recovered = stats.symbols_recovered;
    st.loss_spans = stats.loss_spans;
    st.loss_spans_recovered = stats.loss_spans_recovered;
    st
}

#[derive(Default)]
struct Aggregate {
    n_sources: u64,
    dropped: u64,
    delivered: u64,
    recovered: u64,
    repairs: u64,
    loss_spans: u64,
    loss_spans_recovered: u64,
}

impl Aggregate {
    fn add(&mut self, t: TrialStats) {
        self.n_sources += t.n_sources;
        self.dropped += t.dropped_sources;
        self.delivered += t.delivered_direct;
        self.recovered += t.recovered;
        self.repairs += t.repairs_emitted;
        self.loss_spans += t.loss_spans;
        self.loss_spans_recovered += t.loss_spans_recovered;
    }

    /// Fraction of dropped source symbols the decoder healed (0..1).
    fn recovery_rate(&self) -> f64 {
        if self.dropped == 0 {
            return 1.0;
        }
        self.recovered as f64 / self.dropped as f64
    }

    /// Fraction of all source symbols that never became known (0..1).
    fn residual_rate(&self) -> f64 {
        if self.n_sources == 0 {
            return 0.0;
        }
        let residual = self.dropped.saturating_sub(self.recovered);
        residual as f64 / self.n_sources as f64
    }

    /// Repair symbols emitted per source symbol (redundancy overhead).
    fn overhead(&self) -> f64 {
        if self.n_sources == 0 {
            return 0.0;
        }
        self.repairs as f64 / self.n_sources as f64
    }
}

// ── Driver ──────────────────────────────────────────────────────────────────

/// Redundancy ratios swept, as (label, numerator, denominator).
const RATIOS: &[(&str, u8, u8)] = &[
    ("0%", 0, 1),
    ("10%", 1, 10),
    ("20%", 1, 5),
    ("33%", 1, 3),
    ("50%", 1, 2),
];

/// Loss rates swept (as fractions).
const LOSSES: &[f64] = &[0.05, 0.10, 0.20, 0.30];

const N_SOURCES: u32 = 48;
const SYMBOL_SIZE: usize = 1024;
const TRIALS: u32 = 400;
const MEAN_BURST: f64 = 4.0;

fn sweep(model_name: &str, make_model: impl Fn(f64) -> LossModel) {
    println!("\n### {model_name} channel");
    println!("| loss | ratio | recovery% | residual loss% | overhead% | spans healed |");
    println!("|---|---|---|---|---|---|");
    for &loss in LOSSES {
        for &(label, num, den) in RATIOS {
            let mut agg = Aggregate::default();
            for trial in 0..TRIALS {
                // Reproducible per-cell seed.
                let seed = (loss.to_bits())
                    ^ ((num as u64) << 40)
                    ^ ((den as u64) << 32)
                    ^ u64::from(trial).wrapping_mul(0x1000_0001)
                    ^ model_name.len() as u64;
                agg.add(run_trial(
                    num,
                    den,
                    N_SOURCES,
                    SYMBOL_SIZE,
                    make_model(loss),
                    seed,
                ));
            }
            let spans = if agg.loss_spans == 0 {
                "n/a".to_string()
            } else {
                format!("{}/{}", agg.loss_spans_recovered, agg.loss_spans)
            };
            println!(
                "| {:>3.0}% | {:>4} | {:>7.1} | {:>12.2} | {:>7.1} | {:>10} |",
                loss * 100.0,
                label,
                agg.recovery_rate() * 100.0,
                agg.residual_rate() * 100.0,
                agg.overhead() * 100.0,
                spans,
            );
        }
    }
}

// ── G004 fault matrix: reorder, duplication, accounting bound ──────────────

/// Builds the full emission-ordered symbol list for `n_sources` source
/// symbols at the given ratio (source then its repairs, in wire order),
/// with distinct payload per seq — same discipline as [`run_trial`].
fn emit_symbols(ratio_num: u8, ratio_den: u8, n_sources: u32, symbol_size: usize) -> Vec<Symbol> {
    let cfg = FecConfig {
        redundancy_numerator: ratio_num,
        redundancy_denominator: ratio_den,
        window_max_symbols: 64,
        window_max_bytes: 1 << 20,
    };
    let mut enc = FecEncoder::new(cfg);
    let mut out = Vec::new();
    for seq in 0..n_sources {
        let mut payload = vec![0u8; symbol_size];
        payload[..4].copy_from_slice(&seq.to_le_bytes());
        let emitted = enc.push_source(seq, &payload);
        out.push(emitted.source);
        out.extend(emitted.repairs);
    }
    out
}

/// Applies loss (via `model`), then a deterministic sliding-window reorder,
/// then duplication, in that order — same discipline (and independently
/// seeded/reproduced, not shared code: this bin only sees `transport-core`'s
/// public API, same as `client-transport/tests/video_fault_matrix.rs` and
/// `tests/fec_fault_matrix.test.mjs`, which mirror this fault pipeline at
/// their own layers) as those two files' `apply_faults` helpers.
///
/// `reorder_window <= 1` disables reordering; `dup_p == 0.0` disables
/// duplication.
fn apply_faults(
    symbols: Vec<Symbol>,
    model: LossModel,
    reorder_window: usize,
    dup_p: f64,
    seed: u64,
) -> Vec<Symbol> {
    let mut rng = SplitMix64::new(seed);
    let mut ch = Channel::new(model);
    let mut survivors: Vec<Symbol> = symbols.into_iter().filter(|_| !ch.drop(&mut rng)).collect();

    if reorder_window > 1 {
        let mut i = 0;
        while i + reorder_window <= survivors.len() {
            let j = i + (rng.next_u64() as usize % reorder_window);
            survivors.swap(i, j);
            i += 1;
        }
    }

    let mut out = Vec::with_capacity(survivors.len());
    for sym in survivors {
        out.push(sym.clone());
        if rng.next_f64() < dup_p {
            out.push(sym);
        }
    }
    out
}

/// Result of one fault-matrix cell: recovery outcome plus the peak
/// `FecDecoder::accounting().retained_bytes` observed across the whole
/// trial, which [`run_fault_trial`] asserts (not just records) never
/// exceeds `max_bytes` — the G004 "FEC retained bytes never exceed 16MiB"
/// invariant, proven directly against the pure decoder's own public
/// accounting rather than through any receiver-layer passthrough.
struct FaultTrialStats {
    n_sources: u64,
    delivered_direct: u64,
    recovered: u64,
    peak_retained_bytes: usize,
}

/// Runs one fault-matrix trial: builds the emission-ordered symbol stream,
/// applies the fault pipeline, feeds survivors to a fresh decoder capped at
/// `max_bytes`, and asserts (panics — this rig is meant to fail loudly, not
/// silently pass a broken cell) that `accounting().retained_bytes` never
/// exceeds `max_bytes` after any push.
fn run_fault_trial(
    ratio: (u8, u8),
    n_sources: u32,
    symbol_size: usize,
    model: LossModel,
    reorder_window: usize,
    dup_p: f64,
    seed: u64,
    max_symbols: u16,
    max_bytes: u32,
) -> FaultTrialStats {
    let symbols = emit_symbols(ratio.0, ratio.1, n_sources, symbol_size);
    let faulted = apply_faults(symbols, model, reorder_window, dup_p, seed);

    let mut dec = FecDecoder::new(max_symbols, max_bytes);
    let mut peak_retained_bytes = 0usize;
    for sym in faulted {
        dec.push_symbol(sym);
        let accounting = dec.accounting();
        assert!(
            accounting.retained_bytes <= max_bytes as usize,
            "FEC decoder retained_bytes {} exceeded its {max_bytes}-byte cap \
             (ratio {ratio:?}, reorder_window {reorder_window}, dup_p {dup_p}, seed {seed})",
            accounting.retained_bytes
        );
        peak_retained_bytes = peak_retained_bytes.max(accounting.retained_bytes);
    }

    let stats = dec.stats();
    FaultTrialStats {
        n_sources: u64::from(n_sources),
        delivered_direct: stats.source_symbols_received,
        recovered: stats.symbols_recovered,
        peak_retained_bytes,
    }
}

/// The receiver's real decoder byte cap (mirrors
/// `transport_core::video_rx::RX_DECODER_MAX_BYTES`, duplicated here since
/// this bin does not depend on that module — kept in lockstep by the
/// `sixteen_mib_cap_matches_the_real_receiver_cap` test below).
const FAULT_MATRIX_MAX_BYTES: u32 = 16 * 1024 * 1024;

/// G004 fault matrix: Gilbert-Elliott burst loss, reorder, and duplication,
/// each swept independently and in combination, at a fixed 20% ratio.
/// Every cell asserts the 16 MiB accounting bound via [`run_fault_trial`];
/// this function additionally reports recovery/duplication-tolerance shape.
fn fault_matrix() {
    println!("\n# G004 fault matrix (reorder / duplication / Gilbert-Elliott)");
    println!(
        "\nWindow: {N_SOURCES} source symbols × {SYMBOL_SIZE} B, ratio 20% (1/5), \
         decoder cap {FAULT_MATRIX_MAX_BYTES} bytes. Every cell below asserts \
         accounting().retained_bytes never exceeded the cap."
    );
    println!("\n| fault | param | recovered/dropped | peak retained bytes |");
    println!("|---|---|---|---|");

    struct Cell<'a> {
        label: &'a str,
        param: &'a str,
        model: LossModel,
        reorder_window: usize,
        dup_p: f64,
        seed: u64,
    }
    let cells = [
        Cell {
            label: "reorder-only",
            param: "window=8",
            model: LossModel::Independent { p: 0.0 },
            reorder_window: 8,
            dup_p: 0.0,
            seed: 0xFA07_7001,
        },
        Cell {
            label: "duplication-only",
            param: "p=0.10",
            model: LossModel::Independent { p: 0.0 },
            reorder_window: 1,
            dup_p: 0.10,
            seed: 0xFA07_7002,
        },
        Cell {
            label: "gilbert-elliott burst",
            param: "steady_p=0.20, p_good=0.02",
            model: LossModel::Burst {
                p_good: 0.02,
                p_bad: 0.9,
                steady_p: 0.20,
                mean_burst: MEAN_BURST,
            },
            reorder_window: 1,
            dup_p: 0.0,
            seed: 0xFA07_7003,
        },
        Cell {
            label: "combined",
            param: "GE 20% + reorder(6) + dup(0.05)",
            model: LossModel::Burst {
                p_good: 0.02,
                p_bad: 0.9,
                steady_p: 0.20,
                mean_burst: MEAN_BURST,
            },
            reorder_window: 6,
            dup_p: 0.05,
            seed: 0xFA07_7004,
        },
    ];

    for cell in cells {
        let stats = run_fault_trial(
            (1, 5),
            N_SOURCES,
            SYMBOL_SIZE,
            cell.model,
            cell.reorder_window,
            cell.dup_p,
            cell.seed,
            64,
            FAULT_MATRIX_MAX_BYTES,
        );
        println!(
            "| {} | {} | {}/{} | {} |",
            cell.label,
            cell.param,
            stats.recovered,
            stats.n_sources.saturating_sub(stats.delivered_direct),
            stats.peak_retained_bytes,
        );
    }
}

fn main() {
    println!("# FEC quantification rig (U2 P2)");
    println!(
        "\nWindow: {N_SOURCES} source symbols × {SYMBOL_SIZE} B, {TRIALS} trials/cell, \
         burst mean run {MEAN_BURST}. recovery% = healed / dropped sources; \
         residual = sources never known."
    );

    sweep("independent", |p| LossModel::Independent { p });
    sweep("burst", |p| LossModel::Burst {
        p_good: 0.0,
        p_bad: 1.0,
        steady_p: p,
        mean_burst: MEAN_BURST,
    });

    // One-line takeaway: the smallest swept ratio that clears 99% recovery
    // at each independent loss rate (the adaptive-ratio target).
    println!("\n### 99% recovery target (independent channel)");
    for &loss in LOSSES {
        let mut chosen = None;
        for &(label, num, den) in RATIOS {
            let mut agg = Aggregate::default();
            for trial in 0..TRIALS {
                let seed = (loss.to_bits()) ^ ((num as u64) << 40) ^ u64::from(trial);
                agg.add(run_trial(
                    num,
                    den,
                    N_SOURCES,
                    SYMBOL_SIZE,
                    LossModel::Independent { p: loss },
                    seed,
                ));
            }
            if agg.recovery_rate() >= 0.99 {
                chosen = Some(label);
                break;
            }
        }
        match chosen {
            Some(label) => println!("- {:>3.0}% loss → {label} ratio clears 99%", loss * 100.0),
            None => println!(
                "- {:>3.0}% loss → no swept ratio (≤50%) clears 99%",
                loss * 100.0
            ),
        }
    }

    fault_matrix();
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splitmix_is_deterministic() {
        let mut a = SplitMix64::new(42);
        let mut b = SplitMix64::new(42);
        for _ in 0..100 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
        // Different seed diverges.
        let mut c = SplitMix64::new(43);
        assert_ne!(SplitMix64::new(42).next_u64(), c.next_u64());
    }

    #[test]
    fn splitmix_f64_in_unit_interval() {
        let mut r = SplitMix64::new(7);
        for _ in 0..10_000 {
            let x = r.next_f64();
            assert!((0.0..1.0).contains(&x), "{x} out of [0,1)");
        }
    }

    #[test]
    fn bernoulli_drop_rate_converges() {
        let mut ch = Channel::new(LossModel::Independent { p: 0.30 });
        let mut rng = SplitMix64::new(123);
        let n = 100_000;
        let dropped = (0..n).filter(|_| ch.drop(&mut rng)).count();
        let rate = dropped as f64 / n as f64;
        assert!((rate - 0.30).abs() < 0.01, "rate {rate} not ~0.30");
    }

    #[test]
    fn burst_steady_state_matches_target_but_clusters() {
        let mut ch = Channel::new(LossModel::Burst {
            p_good: 0.0,
            p_bad: 1.0,
            steady_p: 0.20,
            mean_burst: 5.0,
        });
        let mut rng = SplitMix64::new(9);
        let n = 200_000;
        let mut dropped = 0u64;
        let mut runs = 0u64; // count of bad-run starts
        let mut prev = false;
        for _ in 0..n {
            let d = ch.drop(&mut rng);
            if d {
                dropped += 1;
                if !prev {
                    runs += 1;
                }
            }
            prev = d;
        }
        let rate = dropped as f64 / n as f64;
        assert!((rate - 0.20).abs() < 0.02, "steady-state {rate} not ~0.20");
        // Clustering: mean run length should be well above 1 (near 5).
        let mean_run = dropped as f64 / runs as f64;
        assert!(mean_run > 3.0, "mean run {mean_run} not bursty");
    }

    #[test]
    fn zero_loss_delivers_everything_recovers_nothing() {
        let t = run_trial(1, 5, 48, 256, LossModel::Independent { p: 0.0 }, 1);
        assert_eq!(t.dropped_sources, 0);
        assert_eq!(t.delivered_direct, 48);
        assert_eq!(t.recovered, 0);
    }

    #[test]
    fn no_fec_recovers_nothing_under_loss() {
        // ratio 0/1 = no repairs → any dropped source is unrecoverable.
        let t = run_trial(0, 1, 48, 256, LossModel::Independent { p: 0.25 }, 5);
        assert!(t.dropped_sources > 0, "expected some loss at p=0.25");
        assert_eq!(t.repairs_emitted, 0);
        assert_eq!(t.recovered, 0);
    }

    #[test]
    fn heavy_redundancy_beats_light_at_equal_loss() {
        // Aggregate recovery must be monotone-ish: 50% ratio should heal
        // strictly more of a 20% loss than 10% ratio does.
        let agg = |num: u8, den: u8| {
            let mut a = Aggregate::default();
            for trial in 0..200u32 {
                a.add(run_trial(
                    num,
                    den,
                    48,
                    256,
                    LossModel::Independent { p: 0.20 },
                    u64::from(trial) ^ ((num as u64) << 32),
                ));
            }
            a.recovery_rate()
        };
        let light = agg(1, 10);
        let heavy = agg(1, 2);
        assert!(heavy > light, "heavy {heavy} should beat light {light}");
    }

    #[test]
    fn overhead_tracks_the_ratio() {
        // 20% ratio (1/5) → ~0.2 repairs per source over a full window.
        let mut a = Aggregate::default();
        for trial in 0..200u32 {
            a.add(run_trial(
                1,
                5,
                48,
                256,
                LossModel::Independent { p: 0.0 },
                u64::from(trial),
            ));
        }
        let ovh = a.overhead();
        assert!((ovh - 0.20).abs() < 0.03, "overhead {ovh} not ~0.20");
    }
    // ── G004: reorder / duplication / Gilbert-Elliott partial-loss ────────

    #[test]
    fn sixteen_mib_cap_matches_the_real_receiver_cap() {
        assert_eq!(
            FAULT_MATRIX_MAX_BYTES,
            transport_core::video_rx::RX_DECODER_MAX_BYTES,
            "fec_rig's fault-matrix byte cap must track the real receiver's decoder cap"
        );
    }

    #[test]
    fn reorder_permutes_but_never_drops_or_duplicates() {
        let symbols = emit_symbols(0, 1, 32, 64); // no repairs: ratio 0/1
        let before: Vec<u32> = symbols
            .iter()
            .map(|s| match s {
                Symbol::Source { seq, .. } => *seq,
                Symbol::Repair { .. } => unreachable!("ratio 0/1 emits no repairs"),
            })
            .collect();
        let after = apply_faults(symbols, LossModel::Independent { p: 0.0 }, 8, 0.0, 77);
        let after_seqs: Vec<u32> = after
            .iter()
            .map(|s| match s {
                Symbol::Source { seq, .. } => *seq,
                Symbol::Repair { .. } => unreachable!(),
            })
            .collect();

        assert_eq!(
            after_seqs.len(),
            before.len(),
            "reorder must not change the count"
        );
        let mut sorted_before = before.clone();
        let mut sorted_after = after_seqs.clone();
        sorted_before.sort_unstable();
        sorted_after.sort_unstable();
        assert_eq!(sorted_before, sorted_after, "reorder must be a permutation");
        assert_ne!(
            after_seqs, before,
            "an 8-wide window over 32 symbols must actually reorder"
        );
    }

    #[test]
    fn duplication_rate_converges_and_reorder_is_deterministic_per_seed() {
        let symbols = emit_symbols(0, 1, 2000, 16);
        let n = symbols.len();
        let faulted = apply_faults(
            symbols.clone(),
            LossModel::Independent { p: 0.0 },
            1,
            0.15,
            555,
        );
        let extra = faulted.len() - n;
        let rate = extra as f64 / n as f64;
        assert!(
            (rate - 0.15).abs() < 0.02,
            "duplication rate {rate} not ~0.15"
        );

        // Same seed -> byte-identical fault application (determinism, no
        // wall-clock/thread-rng dependence).
        let faulted_again = apply_faults(symbols, LossModel::Independent { p: 0.0 }, 1, 0.15, 555);
        assert_eq!(
            faulted, faulted_again,
            "identical seed must reproduce byte-identical output"
        );
    }

    #[test]
    fn gilbert_elliott_partial_good_state_loss_still_drops_in_good_state() {
        // p_good > 0: even outside a burst, symbols occasionally drop — the
        // Gilbert-Elliott generalization over the classic Gilbert model
        // (`p_good == 0`) that G004 calls for.
        let mut ch = Channel::new(LossModel::Burst {
            p_good: 0.05,
            p_bad: 0.9,
            steady_p: 0.20,
            mean_burst: 4.0,
        });
        let mut rng = SplitMix64::new(2024);
        let mut good_state_drops = 0u64;
        let mut good_state_total = 0u64;
        for _ in 0..200_000 {
            let dropped = ch.drop(&mut rng);
            // `drop` transitions state *then* selects the probability from
            // the post-transition state — measure the same state it just
            // used, not the pre-call one, or a good→bad transition step
            // gets mislabeled as "good state".
            if !ch.in_bad {
                good_state_total += 1;
                if dropped {
                    good_state_drops += 1;
                }
            }
        }
        assert!(
            good_state_total > 0,
            "channel must spend time in the good state"
        );
        let good_rate = good_state_drops as f64 / good_state_total as f64;
        assert!(
            (good_rate - 0.05).abs() < 0.02,
            "good-state drop rate {good_rate} not ~0.05 (p_good must actually apply)"
        );
    }

    #[test]
    fn fault_trial_never_exceeds_an_impossibly_small_byte_cap() {
        // A byte cap smaller than even one symbol's accounted size must
        // never make `run_fault_trial`'s own accounting assertion trip —
        // `FecDecoder` self-enforces the cap by refusing to admit a symbol
        // that would exceed it (accounting-bytes never rise past the cap in
        // the first place), which is the actual G004 contract:
        // `accounting().retained_bytes` never exceeds `max_bytes`, proven
        // here at the most adversarial cap (below one symbol's size) where
        // any bug in that self-enforcement would surely blow the assertion.
        let stats = run_fault_trial(
            (0, 1),
            4,
            1024,
            LossModel::Independent { p: 0.0 },
            1,
            0.0,
            1,
            64,
            16, // 16 bytes: far below even one 1024-byte source symbol's accounting.
        );
        assert!(
            stats.peak_retained_bytes <= 16,
            "the decoder must never admit a symbol that would exceed its own byte cap"
        );
        // Nothing could be admitted under such a tiny cap: no recoveries.
        assert_eq!(stats.recovered, 0);
    }

    #[test]
    fn fault_matrix_cells_stay_within_the_16mib_cap_and_recover_something() {
        for (model, reorder_window, dup_p, seed) in [
            (
                LossModel::Burst {
                    p_good: 0.02,
                    p_bad: 0.9,
                    steady_p: 0.20,
                    mean_burst: MEAN_BURST,
                },
                1,
                0.0,
                0xFA07_9001,
            ),
            (
                LossModel::Burst {
                    p_good: 0.02,
                    p_bad: 0.9,
                    steady_p: 0.20,
                    mean_burst: MEAN_BURST,
                },
                6,
                0.05,
                0xFA07_9002,
            ),
        ] {
            let stats = run_fault_trial(
                (1, 5),
                N_SOURCES,
                SYMBOL_SIZE,
                model,
                reorder_window,
                dup_p,
                seed,
                64,
                FAULT_MATRIX_MAX_BYTES,
            );
            assert!(
                stats.peak_retained_bytes <= FAULT_MATRIX_MAX_BYTES as usize,
                "peak retained bytes must never exceed the 16 MiB cap"
            );
            assert!(
                stats.recovered > 0,
                "1/5 ratio must recover at least some loss at 20% GE"
            );
        }
    }
}
