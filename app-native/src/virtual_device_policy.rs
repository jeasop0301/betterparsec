//! Evidence-gated virtual-device policy (G016). A host virtual display or host
//! Virtual HID may be enabled ONLY after a compatibility cell was recorded as
//! failing without it, and never while the real display/input producer is
//! active — exactly one producer per device kind, so a virtual device can never
//! double the real one. Virtual devices are off by default.
//!
//! Foundation module: actually building, WHCP-signing, installing, and
//! reboot-recovering the separate driver packages, and running the physical
//! elevated/UWP/game/headless-output compatibility matrix that would justify a
//! virtual device, are the physically-gated part of G016 (explicitly NOT a G007
//! prerequisite). This policy gate is pure and headless unit-tested;
//! `#![allow(dead_code)]` matches the repo's deferred-wire-in pattern.
#![allow(dead_code)]

/// Which kind of host virtual device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VirtualDeviceKind {
    /// A virtual display adapter.
    Display,
    /// A virtual (injected) HID.
    Hid,
}

/// The active producer for a device kind — the real device or a virtual one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Producer {
    Real,
    Virtual,
}

/// Per-kind evidence + single-producer policy for host virtual devices.
#[derive(Debug, Clone, Default)]
pub struct VirtualDevicePolicy {
    evidence: [Option<String>; 2],
    producer: [Option<Producer>; 2],
}

fn idx(kind: VirtualDeviceKind) -> usize {
    match kind {
        VirtualDeviceKind::Display => 0,
        VirtualDeviceKind::Hid => 1,
    }
}

impl VirtualDevicePolicy {
    /// A policy with no recorded evidence and no active producers (virtual
    /// devices off by default).
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that a compatibility cell failed without this virtual device,
    /// justifying its later use. `cell` identifies the failing scenario.
    pub fn record_failure(&mut self, kind: VirtualDeviceKind, cell: impl Into<String>) {
        self.evidence[idx(kind)] = Some(cell.into());
    }

    /// Whether justifying evidence has been recorded for this kind.
    pub fn has_evidence(&self, kind: VirtualDeviceKind) -> bool {
        self.evidence[idx(kind)].is_some()
    }

    /// The recorded failing-cell identifier, if any.
    pub fn evidence(&self, kind: VirtualDeviceKind) -> Option<&str> {
        self.evidence[idx(kind)].as_deref()
    }

    /// The current producer for this kind (`None` = neither active).
    pub fn producer(&self, kind: VirtualDeviceKind) -> Option<Producer> {
        self.producer[idx(kind)]
    }

    /// Whether the virtual device is currently the active producer.
    pub fn virtual_active(&self, kind: VirtualDeviceKind) -> bool {
        self.producer[idx(kind)] == Some(Producer::Virtual)
    }

    /// Claim the real device as the producer. Succeeds when nothing or the real
    /// device is already active; fails when the virtual device is active
    /// (releasing it first is required — duplicate-producer prevention).
    pub fn claim_real(&mut self, kind: VirtualDeviceKind) -> bool {
        match self.producer[idx(kind)] {
            None | Some(Producer::Real) => {
                self.producer[idx(kind)] = Some(Producer::Real);
                true
            }
            Some(Producer::Virtual) => false,
        }
    }

    /// Enable the virtual device as the producer. Requires recorded evidence AND
    /// that no real producer is active (exactly one producer). Off by default,
    /// so this must be called explicitly.
    pub fn enable_virtual(&mut self, kind: VirtualDeviceKind) -> bool {
        if !self.has_evidence(kind) {
            return false;
        }
        if self.producer[idx(kind)].is_some() {
            // A producer (real or virtual) already holds this kind.
            return self.producer[idx(kind)] == Some(Producer::Virtual);
        }
        self.producer[idx(kind)] = Some(Producer::Virtual);
        true
    }

    /// Release whatever producer holds this kind (back to `None`).
    pub fn release(&mut self, kind: VirtualDeviceKind) {
        self.producer[idx(kind)] = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use VirtualDeviceKind::{Display, Hid};

    #[test]
    fn virtual_devices_are_off_by_default_and_need_evidence() {
        let mut policy = VirtualDevicePolicy::new();
        assert_eq!(policy.producer(Display), None);
        assert!(!policy.has_evidence(Display));
        // No evidence -> cannot enable.
        assert!(!policy.enable_virtual(Display));
        assert!(!policy.virtual_active(Display));
    }

    #[test]
    fn evidence_gates_virtual_enablement() {
        let mut policy = VirtualDevicePolicy::new();
        policy.record_failure(Display, "elevated-app-headless-output");
        assert!(policy.has_evidence(Display));
        assert_eq!(
            policy.evidence(Display),
            Some("elevated-app-headless-output")
        );
        assert!(policy.enable_virtual(Display));
        assert!(policy.virtual_active(Display));
        assert_eq!(policy.producer(Display), Some(Producer::Virtual));
        // A different kind without evidence is still gated.
        assert!(!policy.enable_virtual(Hid));
    }

    #[test]
    fn duplicate_producer_is_prevented() {
        let mut policy = VirtualDevicePolicy::new();
        policy.record_failure(Hid, "uwp-input-drop");
        // Real HID active first: the virtual HID cannot also become the producer.
        assert!(policy.claim_real(Hid));
        assert!(!policy.enable_virtual(Hid));
        assert_eq!(policy.producer(Hid), Some(Producer::Real));
        // Release, then the virtual can take over.
        policy.release(Hid);
        assert!(policy.enable_virtual(Hid));
        assert!(policy.virtual_active(Hid));
        // With the virtual active, the real device cannot double it.
        assert!(!policy.claim_real(Hid));
        assert_eq!(policy.producer(Hid), Some(Producer::Virtual));
    }

    #[test]
    fn enable_virtual_is_idempotent_when_already_virtual() {
        let mut policy = VirtualDevicePolicy::new();
        policy.record_failure(Display, "game-fullscreen-no-monitor");
        assert!(policy.enable_virtual(Display));
        assert!(policy.enable_virtual(Display)); // idempotent
        assert!(policy.virtual_active(Display));
    }
}
