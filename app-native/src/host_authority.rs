//! Host-authority lease foundation (G013). A single remote owner may hold host
//! authority — display / privacy / optional virtual-device ownership — for a
//! bounded, renewable lease. On expiry, explicit release, fault, reboot, or
//! uninstall the host **fails open** to local control. The lease is journaled
//! for reboot/uninstall recovery, but recovery always fails open: a pre-reboot
//! lease is never restored as live (the monotonic clock resets across a reboot).
//!
//! Pure and clock-injected (the watchdog pattern): the caller supplies a
//! monotonic millisecond timestamp; there are no timers here. Downstream stories
//! (G014 multi-monitor, G015 privacy, G016 virtual devices) build ownership on
//! top of this foundation. The generation/reason types are shared with the G009
//! desktop-control contract so one owner, stale generations, and typed reasons
//! stay consistent across the whole host surface.

// Foundation module: the lease is consumed by the downstream host-authority
// stories (G014 multi-monitor, G015 privacy, G016 virtual devices), which are
// gated on physical/host validation, so the API is intentionally not yet wired
// into a live caller. `#![allow(dead_code)]` matches the repo's deferred
// wire-in pattern; the state machine itself is fully fake-clock unit-tested.
#![allow(dead_code)]
use common::desktop_control::{ControlGeneration, DowngradeReason};

/// The host-authority state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostAuthority {
    /// Host has local control (default and after every fail-open path).
    Local,
    /// A remote `owner` holds authority under `generation` until `deadline_ms`.
    Held {
        owner: u64,
        generation: ControlGeneration,
        deadline_ms: u64,
    },
    /// A fault left the host in local control but operator-visible Degraded.
    Degraded { reason: DowngradeReason },
}

/// Outcome of an [`HostAuthorityLease::acquire`] attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Acquire {
    /// A new owner took the lease.
    Granted,
    /// The current owner refreshed its lease.
    Renewed,
    /// Denied: another owner holds a live lease, an older generation was
    /// presented, or the host is Degraded.
    Denied,
}

/// A durable journal record of who (if anyone) held authority, for audit and
/// operator visibility across a restart. Absolute deadlines are intentionally
/// NOT journaled — they are meaningless after a reboot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HostAuthorityJournal {
    pub held_owner: Option<u64>,
    pub generation: Option<u32>,
}

/// Fail-open host-authority lease with bounded renewal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostAuthorityLease {
    state: HostAuthority,
    lease_ms: u64,
}

impl HostAuthorityLease {
    /// A released lease (host local control). Each grant/renew is bounded to
    /// `lease_ms`.
    pub fn new(lease_ms: u64) -> Self {
        Self {
            state: HostAuthority::Local,
            lease_ms,
        }
    }

    /// The current state.
    pub fn state(&self) -> &HostAuthority {
        &self.state
    }

    /// Whether the host currently controls its resources locally. `Local` and
    /// `Degraded` are local control; only a live `Held` lease is not.
    pub fn local_control(&self) -> bool {
        !matches!(self.state, HostAuthority::Held { .. })
    }

    /// The Degraded reason, if any (operator-visible).
    pub fn degraded_reason(&self) -> Option<DowngradeReason> {
        match self.state {
            HostAuthority::Degraded { reason } => Some(reason),
            _ => None,
        }
    }

    /// Expire the lease if `now_ms` is at or past its deadline (watchdog).
    /// Returns `true` if it just expired (fail-open to `Local`).
    pub fn poll(&mut self, now_ms: u64) -> bool {
        if let HostAuthority::Held { deadline_ms, .. } = self.state
            && now_ms >= deadline_ms
        {
            self.state = HostAuthority::Local;
            return true;
        }
        false
    }

    /// Attempt to acquire or refresh host authority for `owner`. Expires a
    /// stale lease first, then applies the single-owner / newest-generation
    /// rules.
    pub fn acquire(&mut self, owner: u64, generation: ControlGeneration, now_ms: u64) -> Acquire {
        self.poll(now_ms);
        match self.state {
            HostAuthority::Degraded { .. } => Acquire::Denied,
            HostAuthority::Local => {
                self.state = HostAuthority::Held {
                    owner,
                    generation,
                    deadline_ms: now_ms.saturating_add(self.lease_ms),
                };
                Acquire::Granted
            }
            HostAuthority::Held {
                owner: cur_owner,
                generation: cur_gen,
                ..
            } => {
                if owner != cur_owner {
                    // A different owner holds a live lease.
                    Acquire::Denied
                } else if generation == cur_gen || generation.supersedes(cur_gen) {
                    self.state = HostAuthority::Held {
                        owner,
                        generation,
                        deadline_ms: now_ms.saturating_add(self.lease_ms),
                    };
                    Acquire::Renewed
                } else {
                    // Same owner but a stale generation.
                    Acquire::Denied
                }
            }
        }
    }

    /// Explicitly release if `owner` holds the lease (fail open to `Local`).
    /// Returns `true` if a lease was released.
    pub fn release(&mut self, owner: u64) -> bool {
        if let HostAuthority::Held { owner: cur, .. } = self.state
            && cur == owner
        {
            self.state = HostAuthority::Local;
            return true;
        }
        false
    }

    /// Record a fault: the host retains local control but is operator-visible
    /// `Degraded` until [`Self::clear_fault`].
    pub fn fault(&mut self, reason: DowngradeReason) {
        self.state = HostAuthority::Degraded { reason };
    }

    /// Clear a `Degraded` state back to `Local`.
    pub fn clear_fault(&mut self) {
        if matches!(self.state, HostAuthority::Degraded { .. }) {
            self.state = HostAuthority::Local;
        }
    }

    /// A durable journal record for restart audit. Absolute deadlines are not
    /// journaled.
    pub fn journal(&self) -> HostAuthorityJournal {
        match self.state {
            HostAuthority::Held {
                owner, generation, ..
            } => HostAuthorityJournal {
                held_owner: Some(owner),
                generation: Some(generation.0),
            },
            _ => HostAuthorityJournal::default(),
        }
    }

    /// Recover after a reboot/reinstall from a journal: ALWAYS fail open to
    /// local control. Returns the fresh released lease plus the owner (if any)
    /// whose pre-reboot lease was dropped, so an operator/log can report it. The
    /// owner must re-acquire; a journaled lease is never restored as live.
    /// (Uninstall = an empty/absent journal = `Local` with no dropped owner.)
    pub fn recover(journal: &HostAuthorityJournal, lease_ms: u64) -> (Self, Option<u64>) {
        (Self::new(lease_ms), journal.held_owner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LEASE: u64 = 500;

    fn cg(g: u32) -> ControlGeneration {
        ControlGeneration(g)
    }

    #[test]
    fn new_lease_is_local_control() {
        let lease = HostAuthorityLease::new(LEASE);
        assert_eq!(lease.state(), &HostAuthority::Local);
        assert!(lease.local_control());
        assert_eq!(lease.degraded_reason(), None);
    }

    #[test]
    fn acquire_grants_then_bounded_renews_and_rejects_stale_generation() {
        let mut lease = HostAuthorityLease::new(LEASE);
        assert_eq!(lease.acquire(7, cg(1), 1_000), Acquire::Granted);
        assert!(!lease.local_control());
        assert_eq!(
            lease.state(),
            &HostAuthority::Held {
                owner: 7,
                generation: cg(1),
                deadline_ms: 1_500,
            }
        );
        // Same owner, same generation: a bounded renew from the new `now`.
        assert_eq!(lease.acquire(7, cg(1), 1_200), Acquire::Renewed);
        assert_eq!(
            lease.state(),
            &HostAuthority::Held {
                owner: 7,
                generation: cg(1),
                deadline_ms: 1_700,
            }
        );
        // Newer generation renews; older generation is denied.
        assert_eq!(lease.acquire(7, cg(2), 1_300), Acquire::Renewed);
        assert_eq!(lease.acquire(7, cg(1), 1_400), Acquire::Denied);
    }

    #[test]
    fn a_different_owner_is_denied_while_the_lease_is_live() {
        let mut lease = HostAuthorityLease::new(LEASE);
        lease.acquire(7, cg(1), 1_000);
        assert_eq!(lease.acquire(9, cg(5), 1_100), Acquire::Denied);
        assert!(!lease.local_control());
    }

    #[test]
    fn watchdog_expiry_fails_open_and_lets_a_new_owner_in() {
        let mut lease = HostAuthorityLease::new(LEASE);
        lease.acquire(7, cg(1), 1_000); // deadline 1_500
        assert!(!lease.poll(1_499));
        assert!(!lease.local_control());
        // At the deadline it expires and fails open to local control.
        assert!(lease.poll(1_500));
        assert!(lease.local_control());
        assert_eq!(lease.state(), &HostAuthority::Local);
        // A different owner can now take it.
        assert_eq!(lease.acquire(9, cg(1), 1_600), Acquire::Granted);
    }

    #[test]
    fn release_only_by_the_holder() {
        let mut lease = HostAuthorityLease::new(LEASE);
        lease.acquire(7, cg(1), 1_000);
        assert!(!lease.release(9)); // wrong owner
        assert!(!lease.local_control());
        assert!(lease.release(7));
        assert!(lease.local_control());
        assert!(!lease.release(7)); // already released
    }

    #[test]
    fn fault_is_degraded_local_control_and_blocks_acquire_until_cleared() {
        let mut lease = HostAuthorityLease::new(LEASE);
        lease.acquire(7, cg(1), 1_000);
        lease.fault(DowngradeReason::TransientError);
        assert!(lease.local_control());
        assert_eq!(
            lease.degraded_reason(),
            Some(DowngradeReason::TransientError)
        );
        // A Degraded host refuses new leases until the fault is cleared.
        assert_eq!(lease.acquire(9, cg(1), 1_100), Acquire::Denied);
        lease.clear_fault();
        assert_eq!(lease.state(), &HostAuthority::Local);
        assert_eq!(lease.acquire(9, cg(1), 1_100), Acquire::Granted);
    }

    #[test]
    fn journal_records_the_holder_and_recovery_always_fails_open() {
        let mut lease = HostAuthorityLease::new(LEASE);
        lease.acquire(7, cg(3), 1_000);
        let journal = lease.journal();
        assert_eq!(
            journal,
            HostAuthorityJournal {
                held_owner: Some(7),
                generation: Some(3),
            }
        );
        // Simulated reboot: recover from the journal fails open, reporting the
        // dropped owner for operator visibility.
        let (recovered, dropped) = HostAuthorityLease::recover(&journal, LEASE);
        assert!(recovered.local_control());
        assert_eq!(recovered.state(), &HostAuthority::Local);
        assert_eq!(dropped, Some(7));

        // A released lease journals nothing; uninstall (empty journal) recovers
        // to Local with no dropped owner.
        let empty = HostAuthorityLease::new(LEASE).journal();
        assert_eq!(empty, HostAuthorityJournal::default());
        let (fresh, none) = HostAuthorityLease::recover(&empty, LEASE);
        assert!(fresh.local_control());
        assert_eq!(none, None);
    }
}
