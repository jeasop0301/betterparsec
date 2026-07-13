//! Runtime host-bitrate application boundary.
//!
//! The active Moonlight C backend can queue the ecosystem's `0x5506` extension,
//! but the protocol has no acknowledgement. This boundary keeps "queued" and
//! "host encoder applied" deliberately separate.

use moonlight_common::stream::c::MoonlightStream;

use crate::abr::{ApplyGate, MIN_APPLY_INTERVAL_MS};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum BitrateApplyOutcome {
    SentUnacknowledged,
    Unsupported { reason: &'static str },
    Failed { error: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum BitrateApplyStatus {
    Idle,
    SentUnacknowledged {
        kbps: u32,
    },
    Unsupported {
        requested_kbps: u32,
        reason: &'static str,
    },
    Failed {
        requested_kbps: u32,
        error: String,
    },
}

impl BitrateApplyStatus {
    pub(crate) fn is_terminal(&self) -> bool {
        matches!(self, Self::Unsupported { .. })
    }
}

/// Small host-facing seam around the Moonlight fork's `0x5506` sender.
pub(crate) trait HostBitrateControl {
    fn apply_bitrate_kbps(&self, requested_kbps: u32) -> BitrateApplyOutcome;
}

impl HostBitrateControl for MoonlightStream {
    fn apply_bitrate_kbps(&self, requested_kbps: u32) -> BitrateApplyOutcome {
        match self.change_bitrate(requested_kbps) {
            Ok(()) => BitrateApplyOutcome::SentUnacknowledged,
            Err(moonlight_common::MoonlightError::NotSupportedOnHost) => {
                BitrateApplyOutcome::Unsupported {
                    reason: "host did not advertise the compatible 0x5506 bitrate capability",
                }
            }
            Err(error) => BitrateApplyOutcome::Failed {
                error: error.to_string(),
            },
        }
    }
}

/// Owns the apply gate and records only messages the client library queued.
pub(crate) struct BitrateApplyMachine {
    gate: ApplyGate,
    status: BitrateApplyStatus,
    last_attempt_ms: Option<u64>,
}

impl Default for BitrateApplyMachine {
    fn default() -> Self {
        Self::new(0)
    }
}

impl BitrateApplyMachine {
    pub(crate) fn new(initial_bitrate_kbps: u32) -> Self {
        let mut gate = ApplyGate::new();
        gate.seed_initial_bitrate(initial_bitrate_kbps, 0);
        Self {
            gate,
            status: BitrateApplyStatus::Idle,
            last_attempt_ms: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn status(&self) -> &BitrateApplyStatus {
        &self.status
    }

    /// Attempts one gated update and returns a status only when a control call
    /// was made. Unsupported is terminal; transient failures may retry after
    /// the normal interval and are never recorded as sent.
    pub(crate) fn poll(
        &mut self,
        target_kbps: u32,
        now_ms: u64,
        apply: impl FnOnce(u32) -> BitrateApplyOutcome,
    ) -> Option<BitrateApplyStatus> {
        if self.status.is_terminal() {
            return None;
        }
        if matches!(self.status, BitrateApplyStatus::Failed { .. })
            && self
                .last_attempt_ms
                .is_some_and(|last| now_ms.saturating_sub(last) < MIN_APPLY_INTERVAL_MS)
        {
            return None;
        }

        let candidate = self.gate.next_candidate(target_kbps, now_ms)?;
        self.last_attempt_ms = Some(now_ms);
        self.status = match apply(candidate) {
            BitrateApplyOutcome::SentUnacknowledged => {
                self.gate.record_sent_unacknowledged(candidate, now_ms);
                BitrateApplyStatus::SentUnacknowledged { kbps: candidate }
            }
            BitrateApplyOutcome::Unsupported { reason } => BitrateApplyStatus::Unsupported {
                requested_kbps: candidate,
                reason,
            },
            BitrateApplyOutcome::Failed { error } => BitrateApplyStatus::Failed {
                requested_kbps: candidate,
                error,
            },
        };
        Some(self.status.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_is_explicit_and_terminal() {
        let mut machine = BitrateApplyMachine::default();
        let mut calls = 0;
        let status = machine.poll(8_000, 0, |_| {
            calls += 1;
            BitrateApplyOutcome::Unsupported { reason: "no API" }
        });

        assert_eq!(
            status,
            Some(BitrateApplyStatus::Unsupported {
                requested_kbps: 8_000,
                reason: "no API",
            })
        );
        assert_eq!(calls, 1);
        assert_eq!(machine.poll(4_000, 10_000, |_| panic!("terminal")), None);
    }

    #[test]
    fn failed_attempt_is_not_recorded_as_sent_and_can_retry() {
        let mut machine = BitrateApplyMachine::default();
        assert!(matches!(
            machine.poll(8_000, 0, |_| BitrateApplyOutcome::Failed {
                error: "send failed".into(),
            }),
            Some(BitrateApplyStatus::Failed { .. })
        ));
        assert_eq!(machine.poll(8_000, 899, |_| panic!("rate limited")), None);
        assert_eq!(
            machine.poll(8_000, 900, |_| BitrateApplyOutcome::SentUnacknowledged),
            Some(BitrateApplyStatus::SentUnacknowledged { kbps: 8_000 })
        );
    }

    #[test]
    fn queued_message_drives_hysteresis_from_last_sent_value() {
        let mut machine = BitrateApplyMachine::default();
        assert_eq!(
            machine.poll(10_000, 0, |_| BitrateApplyOutcome::SentUnacknowledged),
            Some(BitrateApplyStatus::SentUnacknowledged { kbps: 10_000 })
        );
        assert_eq!(
            machine.poll(10_500, 2_000, |_| panic!("below threshold")),
            None
        );
        assert_eq!(
            machine.poll(11_000, 2_000, |_| BitrateApplyOutcome::SentUnacknowledged),
            Some(BitrateApplyStatus::SentUnacknowledged { kbps: 11_000 })
        );
    }

    #[test]
    fn emergency_decrease_is_not_blocked_by_last_successful_send() {
        let mut machine = BitrateApplyMachine::default();
        assert_eq!(
            machine.poll(10_000, 0, |_| BitrateApplyOutcome::SentUnacknowledged),
            Some(BitrateApplyStatus::SentUnacknowledged { kbps: 10_000 })
        );
        assert_eq!(
            machine.poll(5_000, 100, |_| BitrateApplyOutcome::SentUnacknowledged),
            Some(BitrateApplyStatus::SentUnacknowledged { kbps: 5_000 })
        );
    }

    #[test]
    fn zero_target_does_not_call_host() {
        let mut machine = BitrateApplyMachine::default();
        assert_eq!(machine.poll(0, 10_000, |_| panic!("zero target")), None);
        assert_eq!(machine.status(), &BitrateApplyStatus::Idle);
    }

    #[test]
    fn initial_session_bitrate_is_not_redundantly_sent() {
        let mut machine = BitrateApplyMachine::new(10_000);
        assert_eq!(
            machine.poll(10_000, 10_000, |_| panic!(
                "startup bitrate already configured"
            )),
            None
        );
        assert_eq!(
            machine.poll(9_000, 10_000, |_| BitrateApplyOutcome::SentUnacknowledged),
            Some(BitrateApplyStatus::SentUnacknowledged { kbps: 9_000 })
        );
    }

    #[test]
    fn active_c_backend_implements_typed_control_seam() {
        fn assert_impl<T: HostBitrateControl>() {}
        assert_impl::<MoonlightStream>();
    }
}
