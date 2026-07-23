//! Latency measurement schema (G021). A versioned cross-stage timestamp record
//! for one frame's journey (input -> capture -> encode -> send on the host;
//! receive -> decode -> present on the client) plus percentile statistics.
//!
//! Host and client keep independent monotonic clocks, so an end-to-end
//! input-to-photon latency is NOT directly computable from these timestamps —
//! that requires external measurement (the photodiode / high-speed-camera Gate C
//! of G021, which is physically gated). What IS computable and validated here:
//! each machine's stage sequence must be monotonic non-decreasing (a decrease
//! signals a clock step / skew and the sample is rejected), and per-machine
//! pipeline durations feed the percentile summary (median / p95 / p99), with an
//! empty run rejected rather than reported as zero.

/// One frame's per-stage timestamps in microseconds. Host-side stages use the
/// host monotonic clock; client-side stages use the client monotonic clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StageTimestamps {
    pub input_us: u64,
    pub capture_us: u64,
    pub encode_us: u64,
    pub send_us: u64,
    pub receive_us: u64,
    pub decode_us: u64,
    pub present_us: u64,
}

impl StageTimestamps {
    /// Host-side stages (input -> capture -> encode -> send) must be monotonic.
    pub fn host_valid(&self) -> bool {
        self.input_us <= self.capture_us
            && self.capture_us <= self.encode_us
            && self.encode_us <= self.send_us
    }

    /// Client-side stages (receive -> decode -> present) must be monotonic.
    pub fn client_valid(&self) -> bool {
        self.receive_us <= self.decode_us && self.decode_us <= self.present_us
    }

    /// A sample is valid when both machines' clocks are internally monotonic.
    pub fn is_valid(&self) -> bool {
        self.host_valid() && self.client_valid()
    }

    /// Host pipeline duration (input -> send), µs. Meaningful only when
    /// [`Self::host_valid`].
    pub fn host_pipeline_us(&self) -> u64 {
        self.send_us.saturating_sub(self.input_us)
    }

    /// Client pipeline duration (receive -> present), µs. Meaningful only when
    /// [`Self::client_valid`].
    pub fn client_pipeline_us(&self) -> u64 {
        self.present_us.saturating_sub(self.receive_us)
    }
}

/// Percentile summary over a set of latency samples (µs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LatencyStats {
    pub count: usize,
    pub min: u64,
    pub p50: u64,
    pub p95: u64,
    pub p99: u64,
    pub max: u64,
}

/// Nearest-rank percentile index for a sorted slice of length `len` (> 0).
fn percentile_index(len: usize, fraction: f64) -> usize {
    let rank = (fraction * len as f64).ceil() as usize;
    rank.saturating_sub(1).min(len - 1)
}

impl LatencyStats {
    /// Compute the summary from `samples`. Returns `None` for an empty set — an
    /// empty/invalid run is rejected rather than reported as all-zero.
    pub fn from_samples(samples: &[u64]) -> Option<LatencyStats> {
        if samples.is_empty() {
            return None;
        }
        let mut sorted = samples.to_vec();
        sorted.sort_unstable();
        let len = sorted.len();
        Some(LatencyStats {
            count: len,
            min: sorted[0],
            p50: sorted[percentile_index(len, 0.50)],
            p95: sorted[percentile_index(len, 0.95)],
            p99: sorted[percentile_index(len, 0.99)],
            max: sorted[len - 1],
        })
    }
}

/// Reduce a run of raw samples to valid stage timestamps and a per-machine
/// percentile summary. Returns `None` when fewer than `min_samples` samples are
/// internally-monotonic-valid (an untrustworthy run is rejected, not reported).
pub fn summarize_run(
    samples: &[StageTimestamps],
    min_samples: usize,
) -> Option<(LatencyStats, LatencyStats)> {
    let valid: Vec<&StageTimestamps> = samples.iter().filter(|s| s.is_valid()).collect();
    if valid.len() < min_samples.max(1) {
        return None;
    }
    let host: Vec<u64> = valid.iter().map(|s| s.host_pipeline_us()).collect();
    let client: Vec<u64> = valid.iter().map(|s| s.client_pipeline_us()).collect();
    Some((
        LatencyStats::from_samples(&host)?,
        LatencyStats::from_samples(&client)?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(
        input: u64,
        capture: u64,
        encode: u64,
        send: u64,
        rx: u64,
        dec: u64,
        pres: u64,
    ) -> StageTimestamps {
        StageTimestamps {
            input_us: input,
            capture_us: capture,
            encode_us: encode,
            send_us: send,
            receive_us: rx,
            decode_us: dec,
            present_us: pres,
        }
    }

    #[test]
    fn monotonic_sample_is_valid_and_gives_pipeline_durations() {
        let s = ts(0, 100, 300, 500, 1_000, 1_400, 1_900);
        assert!(s.is_valid());
        assert_eq!(s.host_pipeline_us(), 500);
        assert_eq!(s.client_pipeline_us(), 900);
    }

    #[test]
    fn clock_step_rejects_the_sample() {
        // Host encode goes backwards (clock step) -> host invalid -> rejected.
        let host_step = ts(0, 100, 90, 500, 1_000, 1_400, 1_900);
        assert!(!host_step.host_valid());
        assert!(!host_step.is_valid());
        // Client decode goes backwards -> client invalid.
        let client_step = ts(0, 100, 300, 500, 1_000, 900, 1_900);
        assert!(!client_step.client_valid());
        assert!(!client_step.is_valid());
    }

    #[test]
    fn percentiles_are_nearest_rank_and_empty_run_is_rejected() {
        assert_eq!(LatencyStats::from_samples(&[]), None);
        let one = LatencyStats::from_samples(&[42]).expect("non-empty");
        assert_eq!(
            (one.min, one.p50, one.p95, one.p99, one.max),
            (42, 42, 42, 42, 42)
        );
        // 1..=100: p50 = 50, p95 = 95, p99 = 99 (nearest-rank).
        let hundred: Vec<u64> = (1..=100).collect();
        let stats = LatencyStats::from_samples(&hundred).expect("non-empty");
        assert_eq!(stats.count, 100);
        assert_eq!(stats.min, 1);
        assert_eq!(stats.max, 100);
        assert_eq!(stats.p50, 50);
        assert_eq!(stats.p95, 95);
        assert_eq!(stats.p99, 99);
    }

    #[test]
    fn summarize_run_drops_invalid_and_rejects_thin_runs() {
        let good = ts(0, 100, 300, 500, 1_000, 1_400, 1_900);
        let bad = ts(0, 100, 90, 500, 1_000, 1_400, 1_900); // host clock step
        // One valid + one invalid, min_samples = 2 -> rejected (too few valid).
        assert_eq!(summarize_run(&[good, bad], 2), None);
        // Enough valid samples -> summarized per machine.
        let run = [good, good, good, bad];
        let (host, client) = summarize_run(&run, 3).expect("enough valid samples");
        assert_eq!(host.count, 3);
        assert_eq!(host.p50, 500);
        assert_eq!(client.p50, 900);
    }
}
