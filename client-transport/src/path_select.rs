//! Transport path selection and roaming (G024). One bounded ladder decides how
//! the session connects — direct UDP first, then TURN over UDP, then TURN over
//! TLS 443 (proxy/firewall traversal), then WebSocket/TCP as the last resort.
//! Exactly one transport is ever active; a fallback advances down the ladder and
//! records why. An interface/IP change (roaming) bumps a generation and restarts
//! the ladder from the top, and any result stamped with a stale generation is
//! ignored — so a late failure from a superseded attempt can never advance the
//! new one (generation isolation).
//!
//! Pure and I/O-free: the caller performs the actual ICE/TURN/WebSocket
//! establishment and feeds results back here.

/// A transport path, best-to-worst in the ladder order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportPath {
    /// Direct peer-to-peer UDP (host candidate / server-reflexive).
    DirectUdp,
    /// Relayed via TURN over UDP.
    TurnUdp,
    /// Relayed via TURN over TLS on port 443 (proxy/firewall traversal).
    TurnTls443,
    /// WebSocket over TCP — the always-reachable last resort.
    WebSocketTcp,
}

/// Why the current path was abandoned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FallbackReason {
    /// The path did not connect within its budget.
    Timeout,
    /// ICE/DTLS negotiation failed.
    IceFailed,
    /// The path was blocked (firewall / policy).
    Blocked,
    /// The network interface or IP changed (roaming).
    NetworkChange,
}

/// The fixed ladder, best to worst.
const LADDER: [TransportPath; 4] = [
    TransportPath::DirectUdp,
    TransportPath::TurnUdp,
    TransportPath::TurnTls443,
    TransportPath::WebSocketTcp,
];

/// Bounded transport-path selector with single-active-path, generation
/// isolation, roaming, and reason telemetry.
#[derive(Debug, Clone)]
pub struct PathSelector {
    index: usize,
    generation: u32,
    exhausted: bool,
    stable: bool,
    fallbacks: u64,
    last: Option<(TransportPath, FallbackReason)>,
}

impl Default for PathSelector {
    fn default() -> Self {
        Self::new()
    }
}

impl PathSelector {
    /// A selector attempting the top of the ladder under generation 0.
    pub fn new() -> Self {
        Self {
            index: 0,
            generation: 0,
            exhausted: false,
            stable: false,
            fallbacks: 0,
            last: None,
        }
    }

    /// The single transport currently being attempted (or established), or
    /// `None` once the ladder is exhausted.
    pub fn current(&self) -> Option<TransportPath> {
        if self.exhausted {
            None
        } else {
            Some(LADDER[self.index])
        }
    }

    /// The current roaming generation. Results stamped with a different
    /// generation are stale and must be ignored.
    pub fn generation(&self) -> u32 {
        self.generation
    }

    /// Whether the ladder is exhausted (the caller must reconnect / give up).
    pub fn is_exhausted(&self) -> bool {
        self.exhausted
    }

    /// Total fallbacks recorded (telemetry).
    pub fn fallbacks(&self) -> u64 {
        self.fallbacks
    }

    /// The most recent (path, reason) fallback (telemetry).
    pub fn last_fallback(&self) -> Option<(TransportPath, FallbackReason)> {
        self.last
    }

    /// Mark the current path established. Only honored for the current
    /// generation; a stale success is ignored.
    pub fn succeed(&mut self, generation: u32) -> bool {
        if generation != self.generation || self.exhausted {
            return false;
        }
        self.stable = true;
        true
    }

    /// Whether the current-generation path is established.
    pub fn is_stable(&self) -> bool {
        self.stable
    }

    /// Abandon the current path for `reason` and advance to the next ladder rung.
    /// A result stamped with a stale generation is ignored (returns the current
    /// path unchanged). Returns the new current path, or `None` when the ladder
    /// is exhausted.
    pub fn fallback(&mut self, generation: u32, reason: FallbackReason) -> Option<TransportPath> {
        if generation != self.generation || self.exhausted {
            return self.current();
        }
        self.last = Some((LADDER[self.index], reason));
        self.fallbacks += 1;
        self.stable = false;
        if self.index + 1 >= LADDER.len() {
            self.exhausted = true;
            None
        } else {
            self.index += 1;
            Some(LADDER[self.index])
        }
    }

    /// An interface/IP change: bump the generation and restart the ladder from
    /// the top. Any in-flight result under the old generation is now stale.
    pub fn on_network_change(&mut self) {
        self.last = Some((LADDER[self.index], FallbackReason::NetworkChange));
        self.generation = self.generation.wrapping_add(1);
        self.index = 0;
        self.exhausted = false;
        self.stable = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_at_direct_udp_generation_zero() {
        let sel = PathSelector::new();
        assert_eq!(sel.current(), Some(TransportPath::DirectUdp));
        assert_eq!(sel.generation(), 0);
        assert!(!sel.is_exhausted());
    }

    #[test]
    fn fallback_walks_the_bounded_ladder_then_exhausts() {
        let mut sel = PathSelector::new();
        assert_eq!(sel.fallback(0, FallbackReason::Timeout), Some(TransportPath::TurnUdp));
        assert_eq!(sel.fallback(0, FallbackReason::IceFailed), Some(TransportPath::TurnTls443));
        assert_eq!(sel.fallback(0, FallbackReason::Blocked), Some(TransportPath::WebSocketTcp));
        // Last rung fails -> exhausted, no concurrent path remains.
        assert_eq!(sel.fallback(0, FallbackReason::Timeout), None);
        assert!(sel.is_exhausted());
        assert_eq!(sel.current(), None);
        assert_eq!(sel.fallbacks(), 4);
        assert_eq!(sel.last_fallback(), Some((TransportPath::WebSocketTcp, FallbackReason::Timeout)));
    }

    #[test]
    fn exactly_one_path_is_active_at_a_time() {
        let mut sel = PathSelector::new();
        let a = sel.current();
        sel.fallback(0, FallbackReason::Timeout);
        let b = sel.current();
        assert_ne!(a, b);
        assert!(a.is_some() && b.is_some());
    }

    #[test]
    fn stale_generation_results_are_ignored() {
        let mut sel = PathSelector::new();
        sel.fallback(0, FallbackReason::Timeout); // now on TurnUdp, gen 0
        sel.on_network_change(); // gen 1, back to DirectUdp
        assert_eq!(sel.generation(), 1);
        assert_eq!(sel.current(), Some(TransportPath::DirectUdp));
        // A late failure stamped with the old generation must not advance us.
        assert_eq!(sel.fallback(0, FallbackReason::IceFailed), Some(TransportPath::DirectUdp));
        assert_eq!(sel.current(), Some(TransportPath::DirectUdp));
        // A stale success is also ignored.
        assert!(!sel.succeed(0));
        // The current generation still works.
        assert!(sel.succeed(1));
        assert!(sel.is_stable());
    }

    #[test]
    fn roaming_restarts_the_ladder_under_a_new_generation() {
        let mut sel = PathSelector::new();
        sel.fallback(0, FallbackReason::Timeout);
        sel.fallback(0, FallbackReason::IceFailed); // on TurnTls443
        sel.succeed(0);
        assert!(sel.is_stable());
        // Network switch: restart from the top under a fresh generation.
        sel.on_network_change();
        assert_eq!(sel.current(), Some(TransportPath::DirectUdp));
        assert!(!sel.is_stable());
        assert_eq!(sel.generation(), 1);
        assert_eq!(sel.last_fallback(), Some((TransportPath::TurnTls443, FallbackReason::NetworkChange)));
    }
}
