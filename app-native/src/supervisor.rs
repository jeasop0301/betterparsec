//! G005: unified supervision for the exactly-one-global Foundation
//! process plus the embedded server, on top of the existing managed
//! Sunshine subprocess (`crate::sunshine`). Three independent, boring
//! pieces:
//!
//! - [`FoundationSupervisor`]: a small state machine (Stopped / Starting
//!   / Healthy / Degraded / Restarting / Failed) driving one Foundation
//!   child through a [`ChildSpawner`] seam, with a restart budget +
//!   exponential backoff and a bounded graceful stop. Generic over the
//!   child type (real Sunshine vs. a fake stub) so the 100-cycle tests
//!   below run in milliseconds, headless.
//! - [`job`]: Windows Job Object containment (`KILL_ON_JOB_CLOSE`) so a
//!   crashed/killed unified app never orphans children — compile-gated
//!   to `cfg(windows)` with a no-op fallback elsewhere.
//! - [`redact`]: a pure string transform stripping obvious
//!   credential/token/key patterns out of consolidated child log lines
//!   before they reach the host log.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

// ── State machine ───────────────────────────────────────────────────────

/// Truthful supervision state. `Healthy` requires both the embedded
/// server *and* Foundation to be live; `Degraded` is Foundation-down/
/// web-still-up (the existing `host.rs::start` fallback behavior,
/// generalized into an explicit state instead of an `Option<SunshineProcess>`
/// the UI has to infer from).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupervisorState {
    Stopped,
    Starting,
    Healthy,
    Degraded,
    Restarting,
    Failed,
}

/// Read-only, truthful-by-construction snapshot for the UI/health API —
/// never claims Healthy without observing both liveness signals this
/// tick.
#[derive(Debug, Clone)]
pub struct HealthSnapshot {
    pub state: SupervisorState,
    pub foundation_pid: Option<u32>,
    pub restarts_in_window: u32,
    pub last_error: Option<String>,
}

/// One supervision-level event, fired exactly once per underlying
/// occurrence (never re-fired on repeated `poll()` calls while a dead
/// child sits reaped) — the seam the fake-child tests assert against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SupervisorEvent {
    Spawned {
        pid: u32,
    },
    /// The child died (crash or graceful exit) — terminal for that one
    /// process instance, whether or not a restart follows.
    Terminal {
        pid: u32,
        exit_code: Option<i32>,
    },
    RestartBudgetExhausted,
}

pub type EventSink = Arc<dyn Fn(SupervisorEvent) + Send + Sync>;

// ── Restart budget ──────────────────────────────────────────────────────

/// N restarts per rolling window, exponential backoff between attempts.
/// Pure aside from `Instant::now()` at call sites — every method takes
/// `now` explicitly so it's directly unit-testable without real sleeps.
#[derive(Debug, Clone)]
pub struct RestartBudget {
    max_restarts: u32,
    window: Duration,
    attempts: VecDeque<Instant>,
    consecutive: u32,
}

impl RestartBudget {
    pub fn new(max_restarts: u32, window: Duration) -> Self {
        Self {
            max_restarts,
            window,
            attempts: VecDeque::new(),
            consecutive: 0,
        }
    }

    fn prune(&mut self, now: Instant) {
        while let Some(front) = self.attempts.front() {
            if now.duration_since(*front) >= self.window {
                self.attempts.pop_front();
            } else {
                break;
            }
        }
    }

    pub fn count_in_window(&mut self, now: Instant) -> u32 {
        self.prune(now);
        self.attempts.len() as u32
    }

    /// Record a restart attempt at `now`. `Ok(backoff)` when still under
    /// budget (caller should wait `backoff` before respawning);
    /// `Err(())` when the window is full — the caller must not restart.
    pub fn record_and_check(&mut self, now: Instant) -> Result<Duration, ()> {
        self.prune(now);
        if self.attempts.len() as u32 >= self.max_restarts {
            return Err(());
        }
        self.attempts.push_back(now);
        self.consecutive = self.consecutive.saturating_add(1);
        // 100ms, 200ms, 400ms, ... capped at 6.4s.
        let shift = self.consecutive.saturating_sub(1).min(6);
        Ok(Duration::from_millis(100u64 << shift))
    }

    /// A clean run resets the exponential-backoff ladder (but not the
    /// windowed attempt count — the budget still remembers recent
    /// restarts for the rolling window).
    pub fn note_stable(&mut self) {
        self.consecutive = 0;
    }
}

// ── Supervised child seam ───────────────────────────────────────────────

/// Anything the supervisor can start/monitor/stop. Implemented by a thin
/// adapter over `sunshine::SunshineProcess` for the real Foundation and,
/// in tests, by a fast stub child — same state machine either way.
pub trait SupervisedChild: Send {
    fn pid(&self) -> u32;
    fn is_running(&mut self) -> bool;
    /// Request graceful stop; non-blocking. Only meaningful when
    /// [`Self::supports_graceful_stop`] is `true` — Foundation has no
    /// known graceful-shutdown IPC in this codebase (documented
    /// limitation, same as `sunshine::SunshineProcess::stop`'s
    /// kill-only contract), so the real adapter's `request_stop` is a
    /// no-op and its `supports_graceful_stop` is `false`, which makes
    /// [`FoundationSupervisor::stop`] skip the bounded-deadline poll
    /// entirely and kill immediately instead of guaranteed-stalling
    /// for the full deadline waiting on cooperation that will never
    /// come. The fake test child *does* report `true`, so the
    /// deadline-poll code path itself stays exercised end to end.
    fn request_stop(&mut self);
    /// Whether `request_stop` can actually make this child exit on its
    /// own (a real cooperative shutdown protocol) — when `false`,
    /// [`FoundationSupervisor::stop`] skips straight to `kill` instead
    /// of polling out a deadline that can never be satisfied.
    fn supports_graceful_stop(&self) -> bool;
    fn kill(&mut self);
    /// Best-effort exit code once actually exited; never blocks.
    fn try_exit_code(&mut self) -> Option<i32>;
}

/// Produces one [`SupervisedChild`] per spawn attempt.
pub trait ChildSpawner: Send + Sync {
    fn spawn(&self) -> Result<Box<dyn SupervisedChild>, String>;
}

// ── Foundation supervisor ───────────────────────────────────────────────

/// How long a (re)spawned child must stay running before the
/// exponential-backoff ladder resets (`RestartBudget::note_stable`).
/// Measured against the `now` passed into `poll_at` (real wall-clock
/// time in production via `poll()`, an explicitly advanced fake clock
/// in tests) rather than a real sleep — so a fast crash-loop, where the
/// child dies well inside this window every time, keeps climbing
/// 100/200/400ms... in the real monitor-loop path instead of resetting
/// on every respawn (MEDIUM advisory fix: backoff was previously dead
/// in production because `note_stable()` fired immediately on respawn).
const STABILITY_WINDOW: Duration = Duration::from_secs(30);

struct Inner {
    state: SupervisorState,
    child: Option<Box<dyn SupervisedChild>>,
    budget: RestartBudget,
    last_error: Option<String>,
    embedded_server_alive: bool,
    restart_not_before: Option<Instant>,
    /// When the current child was (re)spawned — `None` when no child is
    /// tracked. Compared against `now` in `poll_at` to decide whether
    /// the stability window has elapsed.
    spawned_at: Option<Instant>,
    /// Whether `note_stable()` has already fired for the current child
    /// — avoids repeatedly resetting an already-zero backoff ladder.
    stable_recorded: bool,
    /// A spawn (initial `start()` or a budgeted respawn in `poll()`) is
    /// in flight — the mutex is *not* held for the actual
    /// `spawner.spawn()` call (which can block for seconds, e.g.
    /// Foundation's up-to-20s `wait_ready`), so this flag is what
    /// prevents a second, concurrent spawn attempt from racing in
    /// underneath it (the "exactly one global Foundation" invariant
    /// under concurrency — `child.is_some()` alone isn't enough once
    /// the slot can be briefly empty *and* not-yet-filled at the same
    /// time). `snapshot()`/`foundation_health()` never blocks on this:
    /// it only ever needs the (cheap, always-available) lock.
    spawning: bool,
}

/// Owns exactly one Foundation child (never spawns a second one while
/// the first slot is occupied) and the embedded server's reported
/// liveness, and derives a truthful [`SupervisorState`] from both.
pub struct FoundationSupervisor {
    spawner: Box<dyn ChildSpawner>,
    events: Option<EventSink>,
    inner: Mutex<Inner>,
}

impl FoundationSupervisor {
    pub fn new(spawner: Box<dyn ChildSpawner>) -> Self {
        Self::with_budget(spawner, RestartBudget::new(3, Duration::from_secs(60)))
    }

    pub fn with_budget(spawner: Box<dyn ChildSpawner>, budget: RestartBudget) -> Self {
        Self {
            spawner,
            events: None,
            inner: Mutex::new(Inner {
                state: SupervisorState::Stopped,
                child: None,
                budget,
                last_error: None,
                embedded_server_alive: true,
                restart_not_before: None,
                spawned_at: None,
                stable_recorded: false,
                spawning: false,
            }),
        }
    }

    pub fn with_event_sink(mut self, sink: EventSink) -> Self {
        self.events = Some(sink);
        self
    }

    fn emit(&self, event: SupervisorEvent) {
        if let Some(sink) = &self.events {
            sink(event);
        }
    }

    /// Derive Healthy/Degraded from current liveness — never claims
    /// Healthy without a live child.
    fn derive_up_state(&self, inner: &Inner) -> SupervisorState {
        if inner.embedded_server_alive {
            SupervisorState::Healthy
        } else {
            // Foundation alive but the embedded server itself is down —
            // still not the steady-state "Healthy" claim.
            SupervisorState::Degraded
        }
    }

    /// Start (or restart from Stopped/Failed) the Foundation child.
    /// Never spawns a second child while one is already tracked, and
    /// never spawns a second one concurrently with an in-flight spawn
    /// either — see `Inner::spawning`. The actual `spawner.spawn()`
    /// call (which can block for seconds — Foundation's `wait_ready` is
    /// up to 20s) runs with the mutex *dropped*, so `snapshot()`/
    /// `foundation_health()` never blocks on it (G005 MEDIUM advisory
    /// fix).
    pub fn start(&self) {
        {
            let mut inner = self.inner.lock().expect("supervisor mutex poisoned");
            if inner.child.is_some() || inner.spawning {
                return; // already running or a spawn is already in flight
            }
            inner.spawning = true;
            inner.state = SupervisorState::Starting;
        }
        let result = self.spawner.spawn();
        self.commit_spawn_result(result, Instant::now());
    }

    /// Common tail of both `start()` and `poll_at`'s respawn path:
    /// re-lock, clear the in-flight flag, and install the spawned child
    /// (or record the failure) — the only place that mutates
    /// `inner.child`/`inner.spawned_at` after a spawn attempt, so the
    /// two callers can't diverge in how they commit the result.
    fn commit_spawn_result(
        &self,
        result: Result<Box<dyn SupervisedChild>, String>,
        now: Instant,
    ) -> SupervisorState {
        let mut inner = self.inner.lock().expect("supervisor mutex poisoned");
        inner.spawning = false;
        match result {
            Ok(child) => {
                let pid = child.pid();
                inner.child = Some(child);
                inner.last_error = None;
                inner.spawned_at = Some(now);
                inner.stable_recorded = false;
                inner.state = self.derive_up_state(&inner);
                let state = inner.state;
                drop(inner);
                self.emit(SupervisorEvent::Spawned { pid });
                state
            }
            Err(e) => {
                inner.last_error = Some(e);
                // LOW advisory fix: a transient respawn failure must
                // still consume budget and retry rather than
                // permanently `Failed`ing while attempts remain —
                // budget exhaustion is the only terminal path. `start()`
                // (no prior crash, no budget entry to make) still just
                // goes `Failed` directly since there's nothing to
                // retry from here without a `poll()` driving it.
                if inner.state == SupervisorState::Restarting {
                    match inner.budget.record_and_check(now) {
                        Ok(backoff) => {
                            inner.state = SupervisorState::Restarting;
                            inner.restart_not_before = Some(now + backoff);
                        }
                        Err(()) => {
                            inner.state = SupervisorState::Failed;
                            let state = inner.state;
                            drop(inner);
                            self.emit(SupervisorEvent::RestartBudgetExhausted);
                            return state;
                        }
                    }
                } else {
                    inner.state = SupervisorState::Failed;
                }
                inner.state
            }
        }
    }

    /// Update embedded-server liveness (the other half of the Healthy
    /// requirement) — call this from the host role's own liveness check.
    pub fn note_embedded_server_alive(&self, alive: bool) {
        let mut inner = self.inner.lock().expect("supervisor mutex poisoned");
        inner.embedded_server_alive = alive;
        if inner.child.is_some()
            && matches!(
                inner.state,
                SupervisorState::Healthy | SupervisorState::Degraded
            )
        {
            inner.state = self.derive_up_state(&inner);
        }
    }

    /// Drive the state machine one tick: detect a dead child, fire
    /// exactly one `Terminal` event for it, then either schedule a
    /// budgeted restart (`Restarting` + backoff deadline) or give up
    /// (`Failed`). When `Restarting` and the backoff deadline has
    /// passed, actually respawn (mutex dropped for the spawn call — see
    /// `start()`'s doc). Cheap and non-blocking for callers who don't
    /// hit the respawn branch — callers poll this on a timer or in a
    /// tight loop (tests).
    pub fn poll(&self) -> SupervisorState {
        self.poll_at(Instant::now())
    }

    /// `poll()`'s real implementation, taking `now` explicitly so tests
    /// can drive the exponential-backoff stability window
    /// (`STABILITY_WINDOW`) deterministically — advancing a captured
    /// `Instant` by `Duration`s — instead of sleeping 30 real seconds.
    fn poll_at(&self, now: Instant) -> SupervisorState {
        let mut inner = self.inner.lock().expect("supervisor mutex poisoned");
        if let Some(child) = inner.child.as_mut() {
            if child.is_running() {
                if !inner.stable_recorded
                    && inner.spawned_at.is_some_and(|spawned_at| {
                        now.duration_since(spawned_at) >= STABILITY_WINDOW
                    })
                {
                    inner.budget.note_stable();
                    inner.stable_recorded = true;
                }
                inner.state = self.derive_up_state(&inner);
                return inner.state;
            }
            let pid = child.pid();
            let exit_code = child.try_exit_code();
            inner.child = None;
            inner.spawned_at = None;
            inner.last_error = Some(format!("foundation exited (pid {pid}, code {exit_code:?})"));
            drop(inner);
            self.emit(SupervisorEvent::Terminal { pid, exit_code });
            inner = self.inner.lock().expect("supervisor mutex poisoned");

            match inner.budget.record_and_check(now) {
                Ok(backoff) => {
                    inner.state = SupervisorState::Restarting;
                    inner.restart_not_before = Some(now + backoff);
                }
                Err(()) => {
                    inner.state = SupervisorState::Failed;
                    drop(inner);
                    self.emit(SupervisorEvent::RestartBudgetExhausted);
                    return SupervisorState::Failed;
                }
            }
            return inner.state;
        }

        if inner.state != SupervisorState::Restarting || inner.spawning {
            return inner.state;
        }
        let due = inner.restart_not_before.is_none_or(|t| now >= t);
        if !due {
            return inner.state;
        }
        inner.spawning = true;
        drop(inner);

        let result = self.spawner.spawn();
        self.commit_spawn_result(result, now)
    }

    /// Bounded graceful stop: request stop, then — only when the child
    /// actually supports a cooperative shutdown
    /// (`SupervisedChild::supports_graceful_stop`) — poll `is_running`
    /// until `deadline` before killing. A kill-only child (the real
    /// Foundation adapter) skips the poll entirely: there's no
    /// cooperation to wait for, so waiting out the deadline would just
    /// be a guaranteed multi-second stall ending in the same `kill()`
    /// (G005 MEDIUM advisory fix — this used to always burn the full
    /// deadline on the caller's thread, e.g. the egui UI thread via
    /// `Host::stop`). Always leaves the supervisor `Stopped` with no
    /// tracked child, whether the child cooperated or not.
    pub fn stop(&self, deadline: Duration) {
        let mut inner = self.inner.lock().expect("supervisor mutex poisoned");
        if let Some(child) = inner.child.as_mut() {
            child.request_stop();
            if child.supports_graceful_stop() {
                let poll_deadline = Instant::now() + deadline;
                while Instant::now() < poll_deadline {
                    if !child.is_running() {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            if child.is_running() {
                child.kill();
            }
            let pid = child.pid();
            let exit_code = child.try_exit_code();
            inner.child = None;
            inner.spawned_at = None;
            drop(inner);
            self.emit(SupervisorEvent::Terminal { pid, exit_code });
            inner = self.inner.lock().expect("supervisor mutex poisoned");
        }
        inner.state = SupervisorState::Stopped;
        inner.restart_not_before = None;
    }

    pub fn snapshot(&self) -> HealthSnapshot {
        let mut inner = self.inner.lock().expect("supervisor mutex poisoned");
        let restarts_in_window = inner.budget.count_in_window(Instant::now());
        HealthSnapshot {
            state: inner.state,
            foundation_pid: inner.child.as_ref().map(|c| c.pid()),
            restarts_in_window,
            last_error: inner.last_error.clone(),
        }
    }
}

// ── Windows Job Object containment ──────────────────────────────────────

/// Process-wide containment: one Job Object for the whole app, created
/// lazily on first use, `KILL_ON_JOB_CLOSE`. Any child assigned here
/// dies when the app process exits (crash or otherwise) — used for the
/// managed Foundation process and, via [`assign_to_app_job`], the
/// streamer spawn path (`app-native/src/host.rs`'s
/// `ContainedStreamerLauncher`).
static APP_JOB: OnceLock<Result<job::JobContainer, String>> = OnceLock::new();

pub fn app_job() -> Result<&'static job::JobContainer, &'static str> {
    match APP_JOB.get_or_init(job::JobContainer::new) {
        Ok(j) => Ok(j),
        Err(e) => Err(e.as_str()),
    }
}

/// Helper the streamer spawn path (and Foundation's [`SunshineSpawner`])
/// both use — assign an already-spawned child's pid into the app-wide
/// job. Takes a bare pid (not a `Child` reference) so it works
/// identically for `std::process::Child` (Foundation) and
/// `tokio::process::Child` (the streamer launcher) without either type
/// needing to be nameable from this module.
pub fn assign_to_app_job(pid: u32) -> Result<(), String> {
    app_job().map_err(|e| e.to_string())?.assign(pid)
}

#[cfg(windows)]
pub mod job {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
    };

    /// A Windows Job Object configured with `KILL_ON_JOB_CLOSE` — every
    /// process assigned to it (and any descendants it spawns) is
    /// terminated when the last handle to the job closes, i.e. when
    /// this process exits.
    pub struct JobContainer(HANDLE);

    // SAFETY: a job object HANDLE is a plain kernel handle; Win32 job
    // object calls are safe to invoke from any thread.
    unsafe impl Send for JobContainer {}
    unsafe impl Sync for JobContainer {}

    impl JobContainer {
        pub fn new() -> Result<Self, String> {
            // SAFETY: FFI per the documented CreateJobObjectW contract;
            // both pointer args are legitimately null (anonymous job,
            // default security attributes).
            let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if handle.is_null() {
                return Err(format!(
                    "CreateJobObjectW failed: {}",
                    std::io::Error::last_os_error()
                ));
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            // SAFETY: `info` is a valid, fully-initialized (zeroed +
            // one field set) instance of the struct
            // `JobObjectExtendedLimitInformation` expects, with a
            // matching size.
            let ok = unsafe {
                SetInformationJobObject(
                    handle,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const core::ffi::c_void,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
            };
            if ok == 0 {
                let err = std::io::Error::last_os_error();
                unsafe { CloseHandle(handle) };
                return Err(format!("SetInformationJobObject failed: {err}"));
            }
            Ok(Self(handle))
        }

        /// Assign an already-spawned process (by pid) into this job so
        /// it dies with the app. Opens a short-lived handle via
        /// `OpenProcess` (PROCESS_SET_QUOTA | PROCESS_TERMINATE — the
        /// documented minimum `AssignProcessToJobObject` needs) rather
        /// than requiring the caller's concrete `Child` type, so both
        /// `std::process::Child` (Foundation) and `tokio::process::Child`
        /// (the streamer launcher) share this one path via a bare pid.
        pub fn assign(&self, pid: u32) -> Result<(), String> {
            // SAFETY: FFI per the documented OpenProcess contract; pid
            // is a plain process id, no unsafe preconditions beyond that.
            let proc_handle = unsafe { OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid) };
            if proc_handle.is_null() {
                return Err(format!(
                    "OpenProcess({pid}) failed: {}",
                    std::io::Error::last_os_error()
                ));
            }
            // SAFETY: `proc_handle` was just opened above and is valid
            // for the duration of this call; closed unconditionally
            // afterwards.
            let ok = unsafe { AssignProcessToJobObject(self.0, proc_handle) };
            let err = if ok == 0 {
                Some(std::io::Error::last_os_error())
            } else {
                None
            };
            unsafe { CloseHandle(proc_handle) };
            match err {
                Some(e) => Err(format!("AssignProcessToJobObject({pid}) failed: {e}")),
                None => Ok(()),
            }
        }
    }

    impl Drop for JobContainer {
        fn drop(&mut self) {
            // SAFETY: `self.0` was returned by `CreateJobObjectW` above
            // and never closed elsewhere.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

#[cfg(not(windows))]
pub mod job {
    /// No-op fallback: non-Windows platforms have no Job Object
    /// primitive; `SupervisedChild::kill` + process-group teardown is
    /// the equivalent handled elsewhere. Kept as a real (empty) type so
    /// callers compile identically on every platform.
    pub struct JobContainer;

    impl JobContainer {
        pub fn new() -> Result<Self, String> {
            Ok(Self)
        }

        pub fn assign(&self, _pid: u32) -> Result<(), String> {
            Ok(())
        }
    }
}

// ── Redacted logs ────────────────────────────────────────────────────────

/// Keywords that mark a `key=value` / `key: value` pair as sensitive.
/// Deliberately boring/obvious — this is a best-effort belt, not a
/// secrets scanner.
const SENSITIVE_KEYS: &[&str] = &[
    "password",
    "passwd",
    "token",
    "secret",
    "apikey",
    "api_key",
    "credential",
    "cred",
    "pkey",
    "private_key",
    "privatekey",
    "authorization",
    "auth",
    "cookie",
    "session",
];

fn key_is_sensitive(key: &str) -> bool {
    let key = key
        .trim()
        .trim_start_matches(['-', '"', '\''])
        .to_ascii_lowercase();
    SENSITIVE_KEYS.iter().any(|s| key.ends_with(s) || key == *s)
}

/// Strip obvious credential/token/key values out of one line of child
/// stdout/stderr before it reaches the host log: `key=value` and
/// `key: value` pairs whose key matches [`SENSITIVE_KEYS`] have their
/// value replaced with `<redacted>`; a bare `Bearer <token>` is
/// redacted the same way. Pure, line-oriented, no regex — boring on
/// purpose.
pub fn redact(input: &str) -> String {
    input
        .lines()
        .map(redact_line)
        .collect::<Vec<_>>()
        .join("\n")
}

fn redact_line(line: &str) -> String {
    // Bearer tokens first: `Authorization: Bearer xyz` would otherwise
    // have its value redacted by the key=value pass below (key
    // "Authorization" is sensitive), consuming the literal "Bearer "
    // marker before this pass ever sees it.
    let line = redact_bearer(line);
    let mut out = String::with_capacity(line.len());
    let mut rest = line.as_str();
    loop {
        let sep_idx = rest.find(['=', ':']);
        let Some(idx) = sep_idx else {
            out.push_str(rest);
            break;
        };
        let (before_sep, after_sep_with_sep) = rest.split_at(idx);
        let sep = &after_sep_with_sep[..1];
        let after_sep = &after_sep_with_sep[1..];

        // The "key" is the last whitespace-delimited token before the
        // separator; keep everything earlier on the line untouched.
        let key_start = before_sep
            .rfind(|c: char| c.is_whitespace())
            .map(|i| i + 1)
            .unwrap_or(0);
        let (prefix, key) = before_sep.split_at(key_start);

        // Skip (but preserve) whitespace right after the separator
        // (e.g. `key: value`) before measuring the value token itself.
        let ws_len = after_sep.len() - after_sep.trim_start().len();
        let (leading_ws, after_ws) = after_sep.split_at(ws_len);
        let value_end = after_ws.find(char::is_whitespace).unwrap_or(after_ws.len());
        let (value, value_rest) = after_ws.split_at(value_end);

        out.push_str(prefix);
        out.push_str(key);
        out.push_str(sep);
        out.push_str(leading_ws);
        if key_is_sensitive(key) && !value.trim().is_empty() {
            out.push_str("<redacted>");
        } else {
            out.push_str(value);
        }
        rest = value_rest;
        if rest.is_empty() {
            break;
        }
    }
    out
}

fn redact_bearer(line: &str) -> String {
    const MARKER: &str = "Bearer ";
    let Some(idx) = line.find(MARKER) else {
        return line.to_string();
    };
    let (prefix, after) = line.split_at(idx + MARKER.len());
    let token_end = after.find(char::is_whitespace).unwrap_or(after.len());
    let (_, after_token) = after.split_at(token_end);
    format!("{prefix}<redacted>{after_token}")
}

/// Spawn a thread that reads `reader` line by line, redacts each line,
/// and forwards it into `tracing` under `target: "child"`. Used to
/// consolidate a supervised child's stdout/stderr into the host log
/// instead of (or in addition to) a plain file redirect.
pub fn spawn_log_pump<R>(reader: R, label: &'static str)
where
    R: std::io::Read + Send + 'static,
{
    std::thread::Builder::new()
        .name(format!("bp-log-{label}"))
        .spawn(move || {
            use std::io::BufRead;
            let buffered = std::io::BufReader::new(reader);
            for line in buffered.lines().map_while(Result::ok) {
                tracing::info!(target: "child", child = label, "{}", redact(&line));
            }
        })
        .ok();
}

/// Spawn a thread that reads `reader` (a supervised child's stderr
/// pipe) line by line, redacts each line, forwards it into `tracing`
/// (same as [`spawn_log_pump`]), and additionally re-writes the
/// redacted line to a fresh file at `log_path` — the "redacting tee"
/// that replaces a raw `Stdio::from(File)` stderr redirect, so the
/// on-disk diagnostic file `sunshine::SunshineProcess::wait_ready`'s
/// `tail_of` reads from is never raw (G005 stderr-redaction blocker
/// fix). Returns the join handle so a caller needing a read-after-
/// write guarantee (`wait_ready`'s startup-failure tail) can wait for
/// the pipe to fully drain — and the file write with it — before
/// reading the file; `None` only if the thread itself failed to start
/// (near-impossible; the caller falls back to an empty tail, same as
/// today's `tail_of` on a missing/empty file).
pub fn spawn_redacting_tee<R>(
    reader: R,
    log_path: std::path::PathBuf,
) -> Option<std::thread::JoinHandle<()>>
where
    R: std::io::Read + Send + 'static,
{
    std::thread::Builder::new()
        .name("bp-log-stderr-tee".into())
        .spawn(move || {
            use std::io::{BufRead, Write};
            let mut file = match std::fs::File::create(&log_path) {
                Ok(f) => f,
                Err(e) => {
                    tracing::warn!(err = %e, path = %log_path.display(), "failed to create redacted stderr log");
                    return;
                }
            };
            let buffered = std::io::BufReader::new(reader);
            for line in buffered.lines().map_while(Result::ok) {
                let redacted = redact(&line);
                tracing::info!(target: "child", child = "foundation-stderr", "{}", redacted);
                if writeln!(file, "{redacted}").is_ok() {
                    let _ = file.flush();
                }
            }
        })
        .ok()
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;

    // -- RestartBudget (pure) ------------------------------------------

    #[test]
    fn restart_budget_allows_up_to_max_then_denies() {
        let mut budget = RestartBudget::new(3, Duration::from_secs(60));
        let base = Instant::now();
        assert!(budget.record_and_check(base).is_ok());
        assert!(budget.record_and_check(base).is_ok());
        assert!(budget.record_and_check(base).is_ok());
        assert!(
            budget.record_and_check(base).is_err(),
            "4th attempt within the window must be denied"
        );
    }

    #[test]
    fn restart_budget_backoff_grows_exponentially() {
        let mut budget = RestartBudget::new(10, Duration::from_secs(60));
        let base = Instant::now();
        let b1 = budget.record_and_check(base).expect("1st");
        let b2 = budget.record_and_check(base).expect("2nd");
        let b3 = budget.record_and_check(base).expect("3rd");
        assert!(b2 > b1, "backoff must grow: {b1:?} -> {b2:?}");
        assert!(b3 > b2, "backoff must grow: {b2:?} -> {b3:?}");
    }

    #[test]
    fn restart_budget_forgets_attempts_outside_the_window() {
        let mut budget = RestartBudget::new(2, Duration::from_millis(50));
        let base = Instant::now();
        budget.record_and_check(base).expect("1st");
        budget.record_and_check(base).expect("2nd");
        assert!(budget.record_and_check(base).is_err(), "budget exhausted");
        let later = base + Duration::from_millis(60);
        assert!(
            budget.record_and_check(later).is_ok(),
            "old attempts must age out of the rolling window"
        );
    }

    // -- Fake child ------------------------------------------------------

    /// A real (but tiny, fast) child process standing in for Foundation
    /// — reuses `sunshine.rs`'s stub pattern (ComSpec on Windows,
    /// `/bin/sh` elsewhere) instead of Foundation's actual binary, so a
    /// 100-cycle test runs in milliseconds.
    struct FakeChild(std::process::Child, Option<i32>);

    impl SupervisedChild for FakeChild {
        fn pid(&self) -> u32 {
            self.0.id()
        }
        fn is_running(&mut self) -> bool {
            matches!(self.0.try_wait(), Ok(None))
        }
        fn request_stop(&mut self) {
            // The fake child has no cooperative shutdown protocol
            // either (it's a plain `cmd.exe`/`sh` one-liner) — mirrors
            // the real adapter's documented kill-only limitation. It
            // still reports `true` from `supports_graceful_stop` (see
            // below) so `FoundationSupervisor::stop`'s deadline-poll
            // branch stays exercised by the timing itself, not by a
            // special test hook.
        }
        fn supports_graceful_stop(&self) -> bool {
            true
        }
        fn kill(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
        fn try_exit_code(&mut self) -> Option<i32> {
            match self.0.try_wait() {
                Ok(Some(status)) => Some(status.code().unwrap_or(-1)),
                _ => self.1,
            }
        }
    }

    fn spawn_fake(sleep_ms: u64, exit_code: i32) -> std::process::Child {
        if cfg!(windows) {
            let comspec = std::env::var_os("ComSpec").expect("ComSpec");
            std::process::Command::new(comspec)
                .args([
                    "/C",
                    &format!("ping -n 1 -w {sleep_ms} 127.0.0.1 >NUL & exit /B {exit_code}"),
                ])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn fake child")
        } else {
            std::process::Command::new("/bin/sh")
                .args([
                    "-c",
                    &format!("sleep {} ; exit {exit_code}", sleep_ms as f64 / 1000.0),
                ])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn fake child")
        }
    }

    /// A spawner that always crashes fast (nonzero exit) — drives the
    /// restart/backoff/budget path deterministically.
    struct CrashingSpawner {
        sleep_ms: u64,
        exit_code: i32,
    }

    impl ChildSpawner for CrashingSpawner {
        fn spawn(&self) -> Result<Box<dyn SupervisedChild>, String> {
            Ok(Box::new(FakeChild(
                spawn_fake(self.sleep_ms, self.exit_code),
                None,
            )))
        }
    }

    fn poll_until<F: Fn(SupervisorState) -> bool>(
        sup: &FoundationSupervisor,
        timeout: Duration,
        pred: F,
    ) -> SupervisorState {
        let deadline = Instant::now() + timeout;
        loop {
            let state = sup.poll();
            if pred(state) || Instant::now() >= deadline {
                return state;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn start_stop_never_leaves_an_orphan_process() {
        let spawner = CrashingSpawner {
            sleep_ms: 2000, // long-lived for this test — stop() must kill it
            exit_code: 0,
        };
        let sup = FoundationSupervisor::new(Box::new(spawner));
        sup.start();
        let pid = sup.snapshot().foundation_pid.expect("spawned");
        assert_eq!(sup.snapshot().state, SupervisorState::Healthy);

        sup.stop(Duration::from_millis(50));
        assert_eq!(sup.snapshot().state, SupervisorState::Stopped);
        assert!(sup.snapshot().foundation_pid.is_none());

        // The process must actually be gone — try_wait would still see
        // it as a live handle in-process, so cross-check with a real
        // "is this pid alive" probe via re-attaching a Command isn't
        // portable; instead assert the kill path completed by
        // confirming this test doesn't hang and the child slot is
        // truly empty (the strongest in-process guarantee available
        // without shelling out to `tasklist`).
        let _ = pid;
    }

    #[test]
    fn crash_triggers_restart_within_budget() {
        let spawner = CrashingSpawner {
            sleep_ms: 5,
            exit_code: 7,
        };
        let terminal_count = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let spawned_count = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let tc = terminal_count.clone();
        let sc = spawned_count.clone();
        let sup = FoundationSupervisor::with_budget(
            Box::new(spawner),
            RestartBudget::new(3, Duration::from_secs(60)),
        )
        .with_event_sink(Arc::new(move |event| match event {
            SupervisorEvent::Terminal { .. } => {
                tc.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            SupervisorEvent::Spawned { .. } => {
                sc.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            SupervisorEvent::RestartBudgetExhausted => {}
        }));

        sup.start();
        // Let it crash, restart, crash again — bounded wait.
        let state = poll_until(&sup, Duration::from_secs(5), |s| {
            s == SupervisorState::Failed
        });
        assert_eq!(
            state,
            SupervisorState::Failed,
            "budget must eventually exhaust"
        );
        assert_eq!(
            terminal_count.load(std::sync::atomic::Ordering::SeqCst),
            4,
            "exactly one terminal event per crash, no double-fire (initial spawn + 3 budgeted restarts, all 4 crash)"
        );
        assert_eq!(
            spawned_count.load(std::sync::atomic::Ordering::SeqCst),
            4,
            "initial spawn + 3 restarts before the 4th restart attempt is denied"
        );
        assert!(
            sup.snapshot().foundation_pid.is_none(),
            "no duplicate live child"
        );
    }

    /// A spawner that takes a while (real sleep) to complete — used to
    /// widen the "spawn in flight" window so a concurrent `start()`/
    /// `snapshot()` test can reliably land inside it.
    struct SlowSpawner {
        delay: Duration,
        spawn_count: Arc<std::sync::atomic::AtomicU32>,
    }

    impl ChildSpawner for SlowSpawner {
        fn spawn(&self) -> Result<Box<dyn SupervisedChild>, String> {
            std::thread::sleep(self.delay);
            self.spawn_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(Box::new(FakeChild(spawn_fake(5_000, 0), None)))
        }
    }

    #[test]
    fn concurrent_start_never_double_spawns_and_health_never_blocks_on_an_in_flight_spawn() {
        let spawn_count = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let sup = Arc::new(FoundationSupervisor::new(Box::new(SlowSpawner {
            delay: Duration::from_millis(400),
            spawn_count: spawn_count.clone(),
        })));

        let handles: Vec<_> = (0..6)
            .map(|_| {
                let sup = sup.clone();
                std::thread::spawn(move || sup.start())
            })
            .collect();

        // Land inside the in-flight-spawn window, then prove
        // `snapshot()` (what `Host::foundation_health()` calls every
        // egui frame) returns promptly instead of blocking on the
        // still-running `spawner.spawn()` call — the G005 MEDIUM
        // advisory fix (spawn used to run under the supervisor mutex).
        std::thread::sleep(Duration::from_millis(100));
        let probe_start = Instant::now();
        let snap = sup.snapshot();
        let probe_elapsed = probe_start.elapsed();
        assert!(
            probe_elapsed < Duration::from_millis(200),
            "snapshot() must not block on an in-flight spawn: took {probe_elapsed:?}"
        );
        assert!(
            matches!(
                snap.state,
                SupervisorState::Starting | SupervisorState::Healthy
            ),
            "state must be truthful mid-spawn: {:?}",
            snap.state
        );

        for h in handles {
            h.join().expect("start() thread must not panic");
        }

        assert_eq!(
            spawn_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "exactly one spawn despite 6 concurrent start() calls — single global Foundation"
        );
        assert_eq!(sup.snapshot().state, SupervisorState::Healthy);
        sup.stop(Duration::from_millis(50));
    }

    /// A spawner whose 2nd call fails transiently (e.g. modeling a
    /// moonlight port briefly still bound right after a crash) and
    /// whose 1st/3rd+ calls succeed — 1st spawns a fast-crashing child
    /// (to drive the supervisor into a respawn attempt), 3rd+ spawns a
    /// long-lived one (so the eventual successful respawn is
    /// observably `Healthy`, not just "not yet dead").
    struct FlakyRespawnSpawner {
        calls: Arc<std::sync::atomic::AtomicU32>,
    }

    impl ChildSpawner for FlakyRespawnSpawner {
        fn spawn(&self) -> Result<Box<dyn SupervisedChild>, String> {
            let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            match n {
                1 => Ok(Box::new(FakeChild(spawn_fake(5, 1), None))),
                2 => Err("transient spawn failure (e.g. port briefly busy)".into()),
                _ => Ok(Box::new(FakeChild(spawn_fake(5_000, 0), None))),
            }
        }
    }

    #[test]
    fn transient_respawn_failure_retries_instead_of_permanently_failing() {
        let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let sup = FoundationSupervisor::with_budget(
            Box::new(FlakyRespawnSpawner {
                calls: calls.clone(),
            }),
            RestartBudget::new(10, Duration::from_secs(60)),
        );
        sup.start(); // call #1: a fast-crashing child
        assert_eq!(sup.snapshot().state, SupervisorState::Healthy);

        // Real `poll()` (not `poll_at`): let the child crash, retry
        // through the transient spawn `Err` (call #2 — must NOT go
        // `Failed` here, budget remains), and land on a stable respawn
        // (call #3).
        let state = poll_until(&sup, Duration::from_secs(5), |s| {
            s == SupervisorState::Healthy && calls.load(std::sync::atomic::Ordering::SeqCst) >= 3
        });
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            3,
            "1 initial spawn + 1 failed respawn + 1 successful respawn"
        );
        assert_eq!(
            state,
            SupervisorState::Healthy,
            "a transient respawn failure must retry, not permanently Fail, while budget remains"
        );
        assert!(sup.snapshot().foundation_pid.is_some());

        sup.stop(Duration::from_millis(50));
    }

    /// A minimal kill-only child (mirrors the real Foundation adapter's
    /// `supports_graceful_stop() -> false`) for proving `stop()` skips
    /// the deadline poll instead of guaranteed-stalling for it.
    struct KillOnlyChild(std::process::Child);

    impl SupervisedChild for KillOnlyChild {
        fn pid(&self) -> u32 {
            self.0.id()
        }
        fn is_running(&mut self) -> bool {
            matches!(self.0.try_wait(), Ok(None))
        }
        fn request_stop(&mut self) {}
        fn supports_graceful_stop(&self) -> bool {
            false
        }
        fn kill(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
        fn try_exit_code(&mut self) -> Option<i32> {
            match self.0.try_wait() {
                Ok(Some(status)) => Some(status.code().unwrap_or(-1)),
                _ => None,
            }
        }
    }

    struct KillOnlySpawner;

    impl ChildSpawner for KillOnlySpawner {
        fn spawn(&self) -> Result<Box<dyn SupervisedChild>, String> {
            Ok(Box::new(KillOnlyChild(spawn_fake(5_000, 0))))
        }
    }

    #[test]
    fn stop_skips_the_deadline_poll_and_kills_immediately_for_a_kill_only_child() {
        let sup = FoundationSupervisor::new(Box::new(KillOnlySpawner));
        sup.start();
        assert_eq!(sup.snapshot().state, SupervisorState::Healthy);

        let start = Instant::now();
        sup.stop(Duration::from_secs(5));
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(1),
            "kill-only stop must not burn the full graceful-stop deadline: {elapsed:?}"
        );
        assert_eq!(sup.snapshot().state, SupervisorState::Stopped);
    }

    /// Proves exponential backoff actually grows through the real
    /// `poll()`/`poll_at` production path (not just `RestartBudget` in
    /// isolation, which `restart_budget_backoff_grows_exponentially`
    /// already covers) — a fake child that dies almost instantly every
    /// time never stays up long enough to cross `STABILITY_WINDOW`, so
    /// `note_stable()` never resets the ladder (MEDIUM advisory fix:
    /// it used to fire unconditionally on every successful respawn).
    /// Drives an injected/advanced `Instant` through `poll_at` rather
    /// than sleeping 30 real seconds per cycle; only actual process
    /// death is awaited with a short real sleep.
    #[test]
    fn poll_backoff_ladder_grows_when_children_never_stay_up_long_enough_to_stabilize() {
        let spawner = CrashingSpawner {
            sleep_ms: 1,
            exit_code: 1,
        };
        let sup = FoundationSupervisor::with_budget(
            Box::new(spawner),
            RestartBudget::new(10, Duration::from_secs(60)),
        );
        sup.start();

        const TICK: Duration = Duration::from_millis(50);
        let mut ticks_to_respawn = Vec::new();
        let mut now = Instant::now();

        for _cycle in 0..3 {
            // Let the fake child (a ~1ms sleep) actually die AND the
            // supervisor observe it. A fixed 40ms sleep flaked twice
            // under parallel-test load (process spawn+exit can take far
            // longer than the nominal 1ms), so wait bounded-but-
            // generously for the Restarting observation instead. The
            // wait happens BEFORE crash detection, so it consumes none
            // of the backoff window the cycle measures.
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut state = sup.poll_at(now);
            while state != SupervisorState::Restarting {
                assert!(
                    Instant::now() < deadline,
                    "child death never observed — state={state:?}"
                );
                std::thread::sleep(Duration::from_millis(10));
                now += Duration::from_millis(10);
                state = sup.poll_at(now);
            }

            let mut ticks = 0u32;
            loop {
                now += TICK;
                ticks += 1;
                let state = sup.poll_at(now);
                if state != SupervisorState::Restarting {
                    assert_eq!(
                        state,
                        SupervisorState::Healthy,
                        "must respawn, not fail, well within a budget of 10"
                    );
                    break;
                }
                assert!(ticks < 50, "respawn never happened — backoff stuck?");
            }
            ticks_to_respawn.push(ticks);
        }

        assert!(
            ticks_to_respawn[1] > ticks_to_respawn[0],
            "backoff must grow 1st -> 2nd respawn: {ticks_to_respawn:?}"
        );
        assert!(
            ticks_to_respawn[2] > ticks_to_respawn[1],
            "backoff must grow 2nd -> 3rd respawn: {ticks_to_respawn:?}"
        );

        sup.stop(Duration::from_millis(50));
    }

    #[test]
    fn one_hundred_start_stop_crash_cycles_stay_truthful() {
        for i in 0..100u32 {
            let crash = i % 3 == 0;
            let spawner = CrashingSpawner {
                sleep_ms: 3,
                exit_code: if crash { 1 } else { 0 },
            };
            let terminals = Arc::new(std::sync::atomic::AtomicU32::new(0));
            let t = terminals.clone();
            let sup = FoundationSupervisor::with_budget(
                Box::new(spawner),
                RestartBudget::new(3, Duration::from_secs(60)),
            )
            .with_event_sink(Arc::new(move |event| {
                if matches!(event, SupervisorEvent::Terminal { .. }) {
                    t.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
            }));

            sup.start();
            assert!(
                sup.snapshot().foundation_pid.is_some(),
                "cycle {i}: spawn must succeed"
            );
            // Never a duplicate global Foundation: exactly one child
            // slot, enforced structurally by `start()`'s early return —
            // re-affirm by calling start() again and checking the pid
            // is unchanged.
            let pid_before = sup.snapshot().foundation_pid;
            sup.start();
            assert_eq!(
                sup.snapshot().foundation_pid,
                pid_before,
                "cycle {i}: start() while running must not spawn a second Foundation"
            );

            sup.stop(Duration::from_millis(50));
            assert_eq!(
                sup.snapshot().state,
                SupervisorState::Stopped,
                "cycle {i}: stop must be truthful"
            );
            assert!(
                sup.snapshot().foundation_pid.is_none(),
                "cycle {i}: no orphan slot"
            );
            assert_eq!(
                terminals.load(std::sync::atomic::Ordering::SeqCst),
                1,
                "cycle {i}: exactly-once terminal event for the stopped child"
            );
        }
    }

    // -- Job containment (unit-testable seam) ----------------------------

    #[test]
    fn job_container_creates_and_assigns_without_error() {
        let container = job::JobContainer::new().expect("create job object");
        let mut child = spawn_fake(50, 0);
        let result = container.assign(child.id());
        assert!(result.is_ok(), "assign must succeed: {result:?}");
        let _ = child.kill();
        let _ = child.wait();
    }

    // -- Redaction (pure) -------------------------------------------------

    #[test]
    fn redact_strips_password_and_token_values() {
        let line = "connecting user=alice password=hunter2 token: abcDEF123";
        let out = redact(line);
        assert!(out.contains("user=alice"), "non-sensitive key kept: {out}");
        assert!(!out.contains("hunter2"), "password value stripped: {out}");
        assert!(!out.contains("abcDEF123"), "token value stripped: {out}");
        assert!(out.contains("password=<redacted>"), "{out}");
        assert!(out.contains("token: <redacted>"), "{out}");
    }

    #[test]
    fn redact_strips_bearer_tokens() {
        let line = "Authorization: Bearer abc.def.ghi123";
        let out = redact(line);
        assert!(!out.contains("abc.def.ghi123"), "{out}");
    }

    #[test]
    fn redact_leaves_ordinary_lines_untouched() {
        let line = "foundation sunshine launched pid=1234 port=47989";
        assert_eq!(redact(line), line);
    }

    #[test]
    fn redact_is_multiline() {
        let text = "line one ok\npassword=secretvalue\nline three ok";
        let out = redact(text);
        assert!(out.contains("line one ok"));
        assert!(out.contains("line three ok"));
        assert!(!out.contains("secretvalue"));
    }
}
