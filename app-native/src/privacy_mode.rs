//! Transactional privacy mode (G015). Engaging privacy applies every requested
//! protection — blank the host physical display and/or block the host local
//! input — atomically: either all requested protections are in force (Applied)
//! or the transaction rolls back the ones that did apply and none remain in
//! force (fail open to local control). A crash, lease expiry, or reboot
//! deactivates privacy and restores local control, reporting the loss to the
//! remote.
//!
//! Foundation module: the actual platform blanking / input blocking and the
//! physical crash/unplug/reboot campaign are the physically-gated part of G015.
//! This state machine is pure — the per-protection platform action is injected
//! as a closure — and headless unit-tested. `#![allow(dead_code)]` matches the
//! repo's deferred-wire-in pattern. Wire types (PrivacyState, protection bits,
//! TransitionStatus, DowngradeReason) are the G009 desktop-control contract.
#![allow(dead_code)]

use common::desktop_control::privacy::{
    PROTECT_ALL, PROTECT_BLANK_DISPLAY, PROTECT_BLOCK_LOCAL_INPUT, PrivacyState,
};
use common::desktop_control::{DowngradeReason, TransitionStatus};

/// The two protection bits, in a fixed apply order.
const PROTECTION_BITS: [u8; 2] = [PROTECT_BLANK_DISPLAY, PROTECT_BLOCK_LOCAL_INPUT];

/// Transactional privacy state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PrivacyMode {
    active: bool,
    protections: u8,
}

impl PrivacyMode {
    /// An inactive privacy mode (host has local control).
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether privacy is currently active.
    pub fn is_active(&self) -> bool {
        self.active
    }

    /// The protections currently in force (0 when inactive).
    pub fn protections(&self) -> u8 {
        self.protections
    }

    /// Whether the host currently controls its display/input locally (the
    /// inverse of active privacy).
    pub fn local_control(&self) -> bool {
        !self.active
    }

    /// The current state as a wire [`PrivacyState`] for the remote.
    pub fn state(&self) -> PrivacyState {
        PrivacyState {
            active: self.active,
            protections_effective: self.protections,
            status: TransitionStatus::Applied,
            reason: None,
        }
    }

    /// Transactionally engage privacy for `requested` protections. `apply` is
    /// invoked per requested protection bit and returns whether that platform
    /// action succeeded; on ANY failure every already-applied protection is
    /// rolled back via `rollback` and privacy stays off (fail open). Returns the
    /// resulting [`PrivacyState`] for the remote.
    pub fn engage<A, R>(&mut self, requested: u8, mut apply: A, mut rollback: R) -> PrivacyState
    where
        A: FnMut(u8) -> bool,
        R: FnMut(u8),
    {
        // Reject unknown protection bits or an empty request.
        if requested == 0 || requested & !PROTECT_ALL != 0 {
            return PrivacyState {
                active: self.active,
                protections_effective: self.protections,
                status: TransitionStatus::Rejected,
                reason: Some(DowngradeReason::Unsupported),
            };
        }
        // Re-engage while already active: roll back the currently in-force
        // protections first, so the new (possibly smaller) request is applied
        // all-or-nothing against a clean base. Without this, a smaller re-engage
        // would overwrite `protections` and orphan an in-force protection that
        // neither disengage nor fail_open ever release — a fail-CLOSED lockout
        // that contradicts the fail-open contract.
        if self.active {
            for &bit in &PROTECTION_BITS {
                if self.protections & bit != 0 {
                    rollback(bit);
                }
            }
            self.active = false;
            self.protections = 0;
        }
        let mut applied: u8 = 0;
        for &bit in &PROTECTION_BITS {
            if requested & bit == 0 {
                continue;
            }
            if apply(bit) {
                applied |= bit;
            } else {
                // Roll back every protection that did apply — all or nothing.
                for &done in &PROTECTION_BITS {
                    if applied & done != 0 {
                        rollback(done);
                    }
                }
                self.active = false;
                self.protections = 0;
                return PrivacyState {
                    active: false,
                    protections_effective: 0,
                    status: TransitionStatus::Rejected,
                    reason: Some(DowngradeReason::PolicyDenied),
                };
            }
        }
        self.active = true;
        self.protections = requested;
        PrivacyState {
            active: true,
            protections_effective: requested,
            status: TransitionStatus::Applied,
            reason: None,
        }
    }

    /// Explicitly leave privacy, rolling back every in-force protection.
    pub fn disengage<R>(&mut self, mut rollback: R)
    where
        R: FnMut(u8),
    {
        for &bit in &PROTECTION_BITS {
            if self.protections & bit != 0 {
                rollback(bit);
            }
        }
        self.active = false;
        self.protections = 0;
    }

    /// Crash / lease-expiry / reboot: fail open. Privacy is deactivated and
    /// local control restored; returns `true` when privacy was active and is
    /// therefore lost (the caller reports the loss to the remote and performs
    /// the platform-level restore). No rollback closure is run — a fault means
    /// the platform is presumed to have already dropped the protections.
    pub fn fail_open(&mut self) -> bool {
        let was_active = self.active;
        self.active = false;
        self.protections = 0;
        was_active
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engage_all_or_nothing_applied() {
        let mut privacy = PrivacyMode::new();
        let mut applied = Vec::new();
        let state = privacy.engage(
            PROTECT_ALL,
            |bit| {
                applied.push(bit);
                true
            },
            |_| panic!("no rollback on success"),
        );
        assert_eq!(
            applied,
            vec![PROTECT_BLANK_DISPLAY, PROTECT_BLOCK_LOCAL_INPUT]
        );
        assert!(privacy.is_active());
        assert_eq!(privacy.protections(), PROTECT_ALL);
        assert!(!privacy.local_control());
        assert_eq!(state.status, TransitionStatus::Applied);
        assert!(state.active);
        assert_eq!(state.reason, None);
    }

    #[test]
    fn engage_rolls_back_when_a_later_protection_fails() {
        let mut privacy = PrivacyMode::new();
        let mut rolled_back = Vec::new();
        // Blank display applies; block-local-input fails -> roll back the blank.
        let state = privacy.engage(
            PROTECT_ALL,
            |bit| bit == PROTECT_BLANK_DISPLAY,
            |bit| rolled_back.push(bit),
        );
        assert_eq!(rolled_back, vec![PROTECT_BLANK_DISPLAY]);
        assert!(!privacy.is_active());
        assert_eq!(privacy.protections(), 0);
        assert!(privacy.local_control());
        assert_eq!(state.status, TransitionStatus::Rejected);
        assert!(!state.active);
        assert_eq!(state.reason, Some(DowngradeReason::PolicyDenied));
    }

    #[test]
    fn engage_rejects_empty_or_unknown_bits() {
        let mut privacy = PrivacyMode::new();
        let empty = privacy.engage(0, |_| true, |_| {});
        assert_eq!(empty.status, TransitionStatus::Rejected);
        assert_eq!(empty.reason, Some(DowngradeReason::Unsupported));
        let unknown = privacy.engage(0x80, |_| true, |_| {});
        assert_eq!(unknown.status, TransitionStatus::Rejected);
        assert!(!privacy.is_active());
    }

    #[test]
    fn engage_single_protection() {
        let mut privacy = PrivacyMode::new();
        let mut applied = Vec::new();
        privacy.engage(
            PROTECT_BLOCK_LOCAL_INPUT,
            |bit| {
                applied.push(bit);
                true
            },
            |_| {},
        );
        assert_eq!(applied, vec![PROTECT_BLOCK_LOCAL_INPUT]);
        assert_eq!(privacy.protections(), PROTECT_BLOCK_LOCAL_INPUT);
    }

    #[test]
    fn disengage_rolls_back_in_force_protections() {
        let mut privacy = PrivacyMode::new();
        privacy.engage(PROTECT_ALL, |_| true, |_| {});
        let mut rolled_back = Vec::new();
        privacy.disengage(|bit| rolled_back.push(bit));
        assert_eq!(
            rolled_back,
            vec![PROTECT_BLANK_DISPLAY, PROTECT_BLOCK_LOCAL_INPUT]
        );
        assert!(!privacy.is_active());
        assert!(privacy.local_control());
    }

    #[test]
    fn fail_open_reports_loss_only_when_active() {
        let mut privacy = PrivacyMode::new();
        assert!(!privacy.fail_open()); // was inactive: nothing lost
        privacy.engage(PROTECT_ALL, |_| true, |_| {});
        assert!(privacy.fail_open()); // was active: privacy lost
        assert!(!privacy.is_active());
        assert!(privacy.local_control());
        assert!(!privacy.state().active);
    }
    #[test]
    fn re_engage_with_smaller_set_releases_the_dropped_protection() {
        let mut privacy = PrivacyMode::new();
        privacy.engage(PROTECT_ALL, |_| true, |_| {});
        assert_eq!(privacy.protections(), PROTECT_ALL);
        // Re-engage with only blank-display: block-local-input must be rolled
        // back on the platform, not orphaned (which would lock the host out).
        let mut rolled_back = Vec::new();
        let state = privacy.engage(PROTECT_BLANK_DISPLAY, |_| true, |bit| rolled_back.push(bit));
        assert_eq!(state.status, TransitionStatus::Applied);
        assert_eq!(state.protections_effective, PROTECT_BLANK_DISPLAY);
        assert_eq!(privacy.protections(), PROTECT_BLANK_DISPLAY);
        assert!(rolled_back.contains(&PROTECT_BLOCK_LOCAL_INPUT));
    }

    #[test]
    fn re_engage_failure_fails_open_from_clean_base() {
        let mut privacy = PrivacyMode::new();
        privacy.engage(PROTECT_ALL, |_| true, |_| {});
        // A re-engage whose new apply fails must leave privacy fully off, not
        // stranded on the prior in-force set.
        let state = privacy.engage(PROTECT_ALL, |bit| bit == PROTECT_BLANK_DISPLAY, |_| {});
        assert_eq!(state.status, TransitionStatus::Rejected);
        assert!(!privacy.is_active());
        assert_eq!(privacy.protections(), 0);
        assert!(privacy.local_control());
    }
}
