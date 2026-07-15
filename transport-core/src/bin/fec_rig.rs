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
//!   - burst (Gilbert two-state): a bad state that always drops with a
//!     tunable mean burst length and steady-state loss p — the case FEC
//!     actually has to survive for video.
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

use transport_core::fec::{FecConfig, FecDecoder, FecEncoder};

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
    /// Gilbert two-state burst channel. `p` is the steady-state loss
    /// probability; `mean_burst` the average consecutive-drop run length
    /// once in the bad state.
    Burst { p: f64, mean_burst: f64 },
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
            LossModel::Burst { p, mean_burst } => {
                // Steady-state π_bad = p, mean bad run = mean_burst.
                //   bad→good = 1/mean_burst
                //   good→bad  = p/((1-p)·mean_burst)   (solves π_bad = p)
                let b2g = 1.0 / mean_burst.max(1.0);
                let g2b = (p / ((1.0 - p).max(1e-9))) * b2g;
                if self.in_bad {
                    if rng.next_f64() < b2g {
                        self.in_bad = false;
                    }
                } else if rng.next_f64() < g2b {
                    self.in_bad = true;
                }
                self.in_bad
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

fn main() {
    println!("# FEC quantification rig (U2 P2)");
    println!(
        "\nWindow: {N_SOURCES} source symbols × {SYMBOL_SIZE} B, {TRIALS} trials/cell, \
         burst mean run {MEAN_BURST}. recovery% = healed / dropped sources; \
         residual = sources never known."
    );

    sweep("independent", |p| LossModel::Independent { p });
    sweep("burst", |p| LossModel::Burst {
        p,
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
            p: 0.20,
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
}
