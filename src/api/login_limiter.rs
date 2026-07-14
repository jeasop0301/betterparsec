//! Per-IP fixed-window login-failure rate limiter.
//!
//! # Eviction bound
//! The entry map is capped at [`MAP_LIMIT`] entries to prevent memory-DoS from
//! an attacker cycling through many source IPs. On insertion when the map is
//! full, all expired entries are evicted first; if the map is still full, the
//! single entry with the oldest `window_start` is evicted. Under a sustained
//! attack with distinct IPs this degrades gracefully: legitimate IPs already
//! over the limit remain blocked until their own window expires naturally.

use std::{
    collections::HashMap,
    net::IpAddr,
    sync::Mutex,
    time::{Duration, Instant},
};

/// Maximum failed attempts allowed per IP per [`WINDOW`] before blocking.
pub const MAX_FAILURES: u32 = 10;
/// Length of the fixed rate-limit window.
pub const WINDOW: Duration = Duration::from_secs(10 * 60);
/// Maximum number of IP entries held in memory at once.
/// Prevents memory-DoS when facing an attacker enumerating many source IPs.
pub const MAP_LIMIT: usize = 4096;

struct Entry {
    /// Start of the current fixed window for this IP.
    window_start: Instant,
    /// Failed login attempts recorded within the current window.
    failures: u32,
}

/// Shared, cross-worker rate limiter; register as `Data<LoginLimiter>`.
pub struct LoginLimiter {
    inner: Mutex<HashMap<IpAddr, Entry>>,
}

/// Result of [`LoginLimiter::check`].
pub enum LimitCheck {
    Allow,
    /// IP is blocked; respond with 429 and `Retry-After: {retry_after_secs}`.
    Deny {
        retry_after_secs: u64,
    },
}

impl LoginLimiter {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    /// Returns whether `ip` is currently rate-limited at `now`.
    ///
    /// Read-only: does not modify state.
    pub fn check(&self, ip: IpAddr, now: Instant) -> LimitCheck {
        let map = self.inner.lock().expect("login limiter mutex poisoned");
        check(&map, ip, now)
    }

    /// Record one failed login attempt for `ip` at `now`.
    pub fn record_failure(&self, ip: IpAddr, now: Instant) {
        let mut map = self.inner.lock().expect("login limiter mutex poisoned");
        record_failure(&mut map, ip, now);
    }

    /// Clear the failure counter for `ip` after a successful login.
    pub fn clear(&self, ip: IpAddr) {
        let mut map = self.inner.lock().expect("login limiter mutex poisoned");
        map.remove(&ip);
    }
}

fn check(map: &HashMap<IpAddr, Entry>, ip: IpAddr, now: Instant) -> LimitCheck {
    let Some(entry) = map.get(&ip) else {
        return LimitCheck::Allow;
    };
    if now.saturating_duration_since(entry.window_start) >= WINDOW {
        // Window expired; counter will be reset on the next failure.
        return LimitCheck::Allow;
    }
    if entry.failures >= MAX_FAILURES {
        let elapsed = now.saturating_duration_since(entry.window_start);
        let remaining_secs = WINDOW.saturating_sub(elapsed).as_secs().max(1);
        LimitCheck::Deny {
            retry_after_secs: remaining_secs,
        }
    } else {
        LimitCheck::Allow
    }
}

fn record_failure(map: &mut HashMap<IpAddr, Entry>, ip: IpAddr, now: Instant) {
    // Evict before inserting a new key to keep the map bounded.
    if !map.contains_key(&ip) && map.len() >= MAP_LIMIT {
        evict(map, now);
    }
    let entry = map.entry(ip).or_insert_with(|| Entry {
        window_start: now,
        failures: 0,
    });
    if now.saturating_duration_since(entry.window_start) >= WINDOW {
        // Start a fresh window for this IP.
        entry.window_start = now;
        entry.failures = 0;
    }
    entry.failures = entry.failures.saturating_add(1);
}

/// Evict entries to make room for a new IP.
/// Step 1: remove all expired entries (window elapsed).
/// Step 2: if still at capacity, remove the single entry with the oldest window_start.
fn evict(map: &mut HashMap<IpAddr, Entry>, now: Instant) {
    map.retain(|_, e| now.saturating_duration_since(e.window_start) < WINDOW);
    if map.len() >= MAP_LIMIT {
        if let Some(oldest_ip) = map
            .iter()
            .min_by_key(|(_, e)| e.window_start)
            .map(|(ip, _)| *ip)
        {
            map.remove(&oldest_ip);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    fn ip4(a: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(192, 0, 2, a))
    }

    fn ip6(seg: u16) -> IpAddr {
        IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, seg))
    }

    /// Build a `HashMap` with `count` entries, each with `window_start = t` and 1 failure.
    /// Uses the `10.0.x.y` address space; does not overlap with `192.0.2.x` or `200.x.x.x`.
    fn make_map(count: usize, t: Instant) -> HashMap<IpAddr, Entry> {
        let mut map = HashMap::with_capacity(count);
        for i in 0..count {
            let ip = IpAddr::V4(Ipv4Addr::new(
                10,
                ((i >> 16) & 0xFF) as u8,
                ((i >> 8) & 0xFF) as u8,
                (i & 0xFF) as u8,
            ));
            map.insert(
                ip,
                Entry {
                    window_start: t,
                    failures: 1,
                },
            );
        }
        map
    }

    // --- Window boundary ---

    #[test]
    fn ninth_failure_still_allows() {
        // 9 failures < MAX_FAILURES → still allowed
        let l = LoginLimiter::new();
        let ip = ip4(1);
        let t0 = Instant::now();
        for _ in 0..9 {
            l.record_failure(ip, t0);
        }
        assert!(matches!(l.check(ip, t0), LimitCheck::Allow));
    }

    #[test]
    fn tenth_failure_triggers_deny() {
        // 10 failures = MAX_FAILURES → blocked
        let l = LoginLimiter::new();
        let ip = ip4(2);
        let t0 = Instant::now();
        for _ in 0..10 {
            l.record_failure(ip, t0);
        }
        assert!(matches!(l.check(ip, t0), LimitCheck::Deny { .. }));
    }

    #[test]
    fn eleventh_failure_still_denied_no_overflow() {
        // saturating_add keeps failures ≥ MAX_FAILURES; must not overflow u32
        let l = LoginLimiter::new();
        let ip = ip4(3);
        let t0 = Instant::now();
        for _ in 0..11 {
            l.record_failure(ip, t0);
        }
        assert!(matches!(l.check(ip, t0), LimitCheck::Deny { .. }));
    }

    // --- Window expiry ---

    #[test]
    fn window_expiry_allows_again() {
        let l = LoginLimiter::new();
        let ip = ip4(4);
        let t0 = Instant::now();
        for _ in 0..10 {
            l.record_failure(ip, t0);
        }
        // Advance past the window boundary.
        let t1 = t0 + WINDOW + Duration::from_secs(1);
        assert!(matches!(l.check(ip, t1), LimitCheck::Allow));
    }

    // --- Success clears counter ---

    #[test]
    fn clear_after_failures_allows_immediately() {
        let l = LoginLimiter::new();
        let ip = ip4(5);
        let t0 = Instant::now();
        for _ in 0..10 {
            l.record_failure(ip, t0);
        }
        l.clear(ip);
        assert!(matches!(l.check(ip, t0), LimitCheck::Allow));
    }

    // --- IP isolation ---

    #[test]
    fn two_ips_do_not_interfere() {
        let l = LoginLimiter::new();
        let ip_a = ip4(6);
        let ip_b = ip4(7);
        let t0 = Instant::now();
        for _ in 0..10 {
            l.record_failure(ip_a, t0);
        }
        // ip_b has no failures → allowed; ip_a is blocked.
        assert!(matches!(l.check(ip_b, t0), LimitCheck::Allow));
        assert!(matches!(l.check(ip_a, t0), LimitCheck::Deny { .. }));
    }

    // --- IPv6 ---

    #[test]
    fn ipv6_key_tracked_and_blocked() {
        let l = LoginLimiter::new();
        let ip = ip6(1);
        let t0 = Instant::now();
        for _ in 0..10 {
            l.record_failure(ip, t0);
        }
        assert!(matches!(l.check(ip, t0), LimitCheck::Deny { .. }));
    }

    // --- Eviction: expired entries removed first ---

    #[test]
    fn eviction_removes_expired_entries_first() {
        // MAP_LIMIT entries at t0; at t1 = t0 + WINDOW + 1s they are all expired.
        let t0 = Instant::now();
        let t1 = t0 + WINDOW + Duration::from_secs(1);
        let mut map = make_map(MAP_LIMIT, t0);
        assert_eq!(map.len(), MAP_LIMIT);

        let new_ip = IpAddr::V4(Ipv4Addr::new(200, 200, 200, 200));
        // record_failure at t1: evicts all expired entries, then inserts new_ip.
        record_failure(&mut map, new_ip, t1);

        assert!(map.contains_key(&new_ip));
        assert_eq!(
            map.len(),
            1,
            "expired eviction must clear old entries before inserting new one"
        );
    }

    // --- Eviction: oldest active entry when none are expired ---

    #[test]
    fn eviction_removes_oldest_window_when_none_expired() {
        let t0 = Instant::now();
        // t1 is only 1 s later — all entries are well within WINDOW.
        let t1 = t0 + Duration::from_secs(1);

        // One entry at t0 (oldest), the rest at t1 (newer).
        let oldest_ip = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 0));
        let mut map = HashMap::new();
        map.insert(
            oldest_ip,
            Entry {
                window_start: t0,
                failures: 1,
            },
        );
        for i in 1..MAP_LIMIT {
            let ip = IpAddr::V4(Ipv4Addr::new(
                10,
                ((i >> 16) & 0xFF) as u8,
                ((i >> 8) & 0xFF) as u8,
                (i & 0xFF) as u8,
            ));
            map.insert(
                ip,
                Entry {
                    window_start: t1,
                    failures: 1,
                },
            );
        }
        assert_eq!(map.len(), MAP_LIMIT);

        let new_ip = IpAddr::V4(Ipv4Addr::new(200, 200, 200, 200));
        record_failure(&mut map, new_ip, t1);

        assert!(map.contains_key(&new_ip), "new IP must be inserted");
        assert!(
            !map.contains_key(&oldest_ip),
            "oldest-window entry must be evicted"
        );
        assert_eq!(
            map.len(),
            MAP_LIMIT,
            "net change: evict one oldest, insert one new"
        );
    }
}
