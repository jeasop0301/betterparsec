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

/// `0x5509` ACK status (u32 LE on the wire). See docs/design/f1-ack.md §2.
/// A Tier A host emits only `Dispatched` or `ValidationFailed`.
// Constructed only by the (not yet wired) 0x5509 receive path and tests —
// production stays on the legacy path until f1-ack.md R-1/R-2 are verified.
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AckStatus {
    /// Validation passed, encoder event queued (Tier A success).
    Dispatched,
    /// Handler validation failed (out-of-range kbps, etc.).
    ValidationFailed,
    /// NVENC reconfiguration confirmed (Tier B only).
    EncoderApplied,
    /// NVENC reconfiguration failed (Tier B only).
    EncoderFailed,
    /// parameter_type not handled by this host.
    UnsupportedParam,
}

impl AckStatus {
    /// Decode the wire value. Unknown values (5..=u32::MAX) are rejected —
    /// a forward-compatible host emitting a new status must not be
    /// misinterpreted as one of the known outcomes.
    // Caller will be the 0x5509 receive path (not yet wired; f1-ack.md R-2).
    #[allow(dead_code)]
    pub(crate) fn from_wire(raw: u32) -> Option<Self> {
        match raw {
            0 => Some(Self::Dispatched),
            1 => Some(Self::ValidationFailed),
            2 => Some(Self::EncoderApplied),
            3 => Some(Self::EncoderFailed),
            4 => Some(Self::UnsupportedParam),
            _ => None,
        }
    }
}

/// How strong the host's "applied" claim is (docs/design/f1-ack.md §1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AckTier {
    /// Tier A: the request passed validation and was queued to the encoder.
    /// NOT equivalent to NVENC apply success — labels must not say "applied".
    Dispatched,
    /// Tier B: NVENC reconfiguration completion was confirmed.
    EncoderConfirmed,
}

/// PendingAck → AckTimeout threshold (docs/design/f1-ack.md §4: ENet LAN RTT
/// + encoder event queue latency + 2× control-loop polling jitter ≈ 3 s).
pub(crate) const ACK_TIMEOUT_MS: u64 = 3_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum BitrateApplyStatus {
    Idle,
    /// Legacy path: host does not advertise the 0x5509 ACK capability.
    SentUnacknowledged {
        kbps: u32,
    },
    /// ACK-capable host: request queued, awaiting 0x5509.
    PendingAck {
        kbps: u32,
        sent_at_ms: u64,
    },
    /// 0x5509 confirmed the request (strength per [`AckTier`]).
    Applied {
        requested_kbps: u32,
        applied_kbps: u32,
        tier: AckTier,
    },
    /// 0x5509 reported a host-side failure.
    ApplyFailed {
        requested_kbps: u32,
        status: AckStatus,
    },
    /// ACK-capable host did not respond within [`ACK_TIMEOUT_MS`].
    /// Not terminal: the next poll cycle may retry.
    AckTimeout {
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
    /// True only after the host advertises the 0x5509 ACK capability
    /// (LI_FF_DYNAMIC_BITRATE_ACK, 0x80). MUST stay false until Foundation
    /// R-1/R-2 are verified (docs/design/f1-ack.md): with 0x40-only hosts,
    /// entering PendingAck would wait forever and retry-loop every 3 s.
    /// No production caller sets this yet — shipped behavior is unchanged.
    ack_supported: bool,
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
            ack_supported: false,
        }
    }

    /// Arm 0x5509 ACK tracking. Call only after capability negotiation
    /// confirms the host sends ACK replies (0x80 bit) — see field docs.
    // Inactive until the Foundation host side is verified (f1-ack.md R-1/R-2);
    // exercised by unit tests only.
    #[allow(dead_code)]
    pub(crate) fn enable_ack_tracking(&mut self) {
        self.ack_supported = true;
    }

    #[cfg(test)]
    pub(crate) fn status(&self) -> &BitrateApplyStatus {
        &self.status
    }

    /// Deliver a parsed 0x5509 ACK. Transitions `PendingAck` to `Applied` /
    /// `ApplyFailed` and returns the new status; an ACK arriving in any other
    /// state (late ACK after timeout, duplicate, unexpected delivery) is
    /// discarded and returns `None` with the state unchanged.
    // Inactive until the 0x5509 receive path exists (f1-ack.md R-2);
    // exercised by unit tests only.
    #[allow(dead_code)]
    pub(crate) fn handle_ack(
        &mut self,
        applied_kbps: u32,
        status: AckStatus,
    ) -> Option<BitrateApplyStatus> {
        let BitrateApplyStatus::PendingAck { kbps, .. } = self.status else {
            return None;
        };
        self.status = match status {
            AckStatus::Dispatched => BitrateApplyStatus::Applied {
                requested_kbps: kbps,
                applied_kbps,
                tier: AckTier::Dispatched,
            },
            AckStatus::EncoderApplied => BitrateApplyStatus::Applied {
                requested_kbps: kbps,
                applied_kbps,
                tier: AckTier::EncoderConfirmed,
            },
            AckStatus::ValidationFailed
            | AckStatus::EncoderFailed
            | AckStatus::UnsupportedParam => BitrateApplyStatus::ApplyFailed {
                requested_kbps: kbps,
                status,
            },
        };
        Some(self.status.clone())
    }

    /// Attempts one gated update and returns a status only when a control call
    /// was made. Unsupported is terminal; transient failures may retry after
    /// the normal interval and are never recorded as sent. While a request is
    /// awaiting its ACK, no new request is issued (one in-flight maximum);
    /// an ACK overdue by [`ACK_TIMEOUT_MS`] transitions to `AckTimeout`.
    pub(crate) fn poll(
        &mut self,
        target_kbps: u32,
        now_ms: u64,
        apply: impl FnOnce(u32) -> BitrateApplyOutcome,
    ) -> Option<BitrateApplyStatus> {
        if self.status.is_terminal() {
            return None;
        }
        if let BitrateApplyStatus::PendingAck { kbps, sent_at_ms } = self.status {
            if now_ms.saturating_sub(sent_at_ms) >= ACK_TIMEOUT_MS {
                self.status = BitrateApplyStatus::AckTimeout { kbps };
                return Some(self.status.clone());
            }
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
                if self.ack_supported {
                    BitrateApplyStatus::PendingAck {
                        kbps: candidate,
                        sent_at_ms: now_ms,
                    }
                } else {
                    BitrateApplyStatus::SentUnacknowledged { kbps: candidate }
                }
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

    // ── f1-ack 0x5509 state machine (R-1/R-2 source-confirmed 2026-07-14;
    //    stays inactive until the moonlight-common 0x5509 receive path lands) ──

    // A01 — shipped default: without enable_ack_tracking a queued send stays
    // SentUnacknowledged and never enters PendingAck.
    #[test]
    fn ack_disabled_default_keeps_legacy_path() {
        let mut machine = BitrateApplyMachine::default();
        assert_eq!(
            machine.poll(8_000, 0, |_| BitrateApplyOutcome::SentUnacknowledged),
            Some(BitrateApplyStatus::SentUnacknowledged { kbps: 8_000 })
        );
        assert_eq!(machine.handle_ack(8_000, AckStatus::Dispatched), None);
        assert_eq!(
            machine.status(),
            &BitrateApplyStatus::SentUnacknowledged { kbps: 8_000 },
            "an unexpected ACK must not disturb the legacy state"
        );
    }

    // A02 — armed machine enters PendingAck and holds one in-flight request:
    // no new control call while awaiting the ACK.
    #[test]
    fn ack_enabled_send_enters_pending_and_blocks_new_sends() {
        let mut machine = BitrateApplyMachine::default();
        machine.enable_ack_tracking();
        assert_eq!(
            machine.poll(8_000, 1_000, |_| BitrateApplyOutcome::SentUnacknowledged),
            Some(BitrateApplyStatus::PendingAck {
                kbps: 8_000,
                sent_at_ms: 1_000,
            })
        );
        assert_eq!(
            machine.poll(4_000, 2_500, |_| panic!("one in-flight request max")),
            None
        );
    }

    // A03 — Tier A dispatch ACK resolves PendingAck to Applied{Dispatched}.
    #[test]
    fn dispatch_ack_resolves_to_applied() {
        let mut machine = BitrateApplyMachine::default();
        machine.enable_ack_tracking();
        machine.poll(8_000, 0, |_| BitrateApplyOutcome::SentUnacknowledged);
        assert_eq!(
            machine.handle_ack(8_000, AckStatus::Dispatched),
            Some(BitrateApplyStatus::Applied {
                requested_kbps: 8_000,
                applied_kbps: 8_000,
                tier: AckTier::Dispatched,
            })
        );
    }

    // A04 — failure ACKs resolve to ApplyFailed carrying the wire status.
    #[test]
    fn failure_ack_resolves_to_apply_failed() {
        let mut machine = BitrateApplyMachine::default();
        machine.enable_ack_tracking();
        machine.poll(8_000, 0, |_| BitrateApplyOutcome::SentUnacknowledged);
        assert_eq!(
            machine.handle_ack(0, AckStatus::ValidationFailed),
            Some(BitrateApplyStatus::ApplyFailed {
                requested_kbps: 8_000,
                status: AckStatus::ValidationFailed,
            })
        );
    }

    // A05 — Tier B encoder ACK maps to the EncoderConfirmed tier.
    #[test]
    fn encoder_ack_resolves_to_encoder_confirmed_tier() {
        let mut machine = BitrateApplyMachine::default();
        machine.enable_ack_tracking();
        machine.poll(8_000, 0, |_| BitrateApplyOutcome::SentUnacknowledged);
        assert_eq!(
            machine.handle_ack(7_500, AckStatus::EncoderApplied),
            Some(BitrateApplyStatus::Applied {
                requested_kbps: 8_000,
                applied_kbps: 7_500,
                tier: AckTier::EncoderConfirmed,
            })
        );
    }

    // A06 — timeout boundary: 2999 ms still pending, exactly 3000 ms times
    // out; AckTimeout is not terminal and the next gated poll retries.
    #[test]
    fn ack_timeout_boundary_and_retry() {
        let mut machine = BitrateApplyMachine::default();
        machine.enable_ack_tracking();
        machine.poll(8_000, 1_000, |_| BitrateApplyOutcome::SentUnacknowledged);
        assert_eq!(machine.poll(8_000, 1_000 + 2_999, |_| panic!("pending")), None);
        assert_eq!(
            machine.poll(8_000, 1_000 + 3_000, |_| panic!("timeout transition only")),
            Some(BitrateApplyStatus::AckTimeout { kbps: 8_000 })
        );
        // Late ACK after the timeout is discarded.
        assert_eq!(machine.handle_ack(8_000, AckStatus::Dispatched), None);
        assert_eq!(machine.status(), &BitrateApplyStatus::AckTimeout { kbps: 8_000 });
        // Retry proceeds under normal gate rules (new target, interval ok).
        assert_eq!(
            machine.poll(6_000, 10_000, |_| BitrateApplyOutcome::SentUnacknowledged),
            Some(BitrateApplyStatus::PendingAck {
                kbps: 6_000,
                sent_at_ms: 10_000,
            })
        );
    }

    // A07 — ACKs in non-pending states are discarded (Idle here; late-ACK
    // after timeout is pinned in A06, legacy state in A01).
    #[test]
    fn ack_in_idle_is_discarded() {
        let mut machine = BitrateApplyMachine::default();
        machine.enable_ack_tracking();
        assert_eq!(machine.handle_ack(8_000, AckStatus::Dispatched), None);
        assert_eq!(machine.status(), &BitrateApplyStatus::Idle);
    }

    // A08 — wire decode domain: 0..=4 map to the enum, everything else is
    // rejected (a future status value must not alias a known outcome).
    #[test]
    fn ack_status_wire_domain() {
        assert_eq!(AckStatus::from_wire(0), Some(AckStatus::Dispatched));
        assert_eq!(AckStatus::from_wire(1), Some(AckStatus::ValidationFailed));
        assert_eq!(AckStatus::from_wire(2), Some(AckStatus::EncoderApplied));
        assert_eq!(AckStatus::from_wire(3), Some(AckStatus::EncoderFailed));
        assert_eq!(AckStatus::from_wire(4), Some(AckStatus::UnsupportedParam));
        assert_eq!(AckStatus::from_wire(5), None);
        assert_eq!(AckStatus::from_wire(u32::MAX), None);
    }
}
