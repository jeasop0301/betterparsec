//! Privacy-safe microphone capture gate (G025). Microphone audio may leave the
//! client ONLY when every condition holds at once: permission is granted, the
//! mic is not muted, a device is present, and the session is active. Muting is
//! fail-closed (audio stops immediately), any uncertainty fails closed, and a
//! reconnect drops the session and device so capture never silently resumes
//! before the conditions are re-established.
//!
//! Pure and headless: the platform permission prompt, device enumeration, and
//! actual Opus capture (streamer/native/web audio) sit on top of this gate.

/// Microphone permission state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MicPermission {
    /// Not yet decided by the user.
    Prompt,
    /// The user granted microphone access.
    Granted,
    /// The user denied microphone access.
    Denied,
}

/// Privacy-safe microphone capture gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MicrophoneGate {
    permission: MicPermission,
    muted: bool,
    device: Option<u64>,
    session_active: bool,
}

impl Default for MicrophoneGate {
    fn default() -> Self {
        Self {
            permission: MicPermission::Prompt,
            muted: false,
            device: None,
            session_active: false,
        }
    }
}

impl MicrophoneGate {
    /// A gate with no permission, no device, and no active session — capture
    /// off.
    pub fn new() -> Self {
        Self::default()
    }

    /// The privacy-safe capture decision: audio may be captured and sent only
    /// when permission is granted, the mic is not muted, a device is present,
    /// and the session is active.
    pub fn can_capture(&self) -> bool {
        matches!(self.permission, MicPermission::Granted)
            && !self.muted
            && self.device.is_some()
            && self.session_active
    }

    /// The permission state.
    pub fn permission(&self) -> MicPermission {
        self.permission
    }

    /// Whether the mic is muted.
    pub fn is_muted(&self) -> bool {
        self.muted
    }

    /// Record the platform permission decision.
    pub fn set_permission(&mut self, permission: MicPermission) {
        self.permission = permission;
    }

    /// Mute the mic — fail-closed: capture stops immediately.
    pub fn mute(&mut self) {
        self.muted = true;
    }

    /// Unmute the mic (an explicit user action; capture resumes only if the
    /// other conditions also hold).
    pub fn unmute(&mut self) {
        self.muted = false;
    }

    /// Set (or clear, with `None`) the active capture device. A device switch
    /// passes `None` first if the previous device must be released before the
    /// new one is confirmed.
    pub fn set_device(&mut self, device: Option<u64>) {
        self.device = device;
    }

    /// Mark the session active/inactive.
    pub fn set_session_active(&mut self, active: bool) {
        self.session_active = active;
    }

    /// Any uncertainty (a device error, an in-flight permission change): fail
    /// closed by muting. The user must explicitly unmute to resume.
    pub fn fail_closed(&mut self) {
        self.muted = true;
    }

    /// Reconnect: fail closed. The session goes inactive and the device is
    /// dropped so capture cannot silently resume before both are re-established.
    /// Permission (a user decision) persists.
    pub fn on_reconnect(&mut self) {
        self.session_active = false;
        self.device = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ready() -> MicrophoneGate {
        // A fully-permitted, active, unmuted gate with a device.
        let mut gate = MicrophoneGate::new();
        gate.set_permission(MicPermission::Granted);
        gate.set_device(Some(1));
        gate.set_session_active(true);
        gate
    }

    #[test]
    fn default_gate_never_captures() {
        let gate = MicrophoneGate::new();
        assert!(!gate.can_capture());
        assert_eq!(gate.permission(), MicPermission::Prompt);
    }

    #[test]
    fn all_conditions_required_to_capture() {
        assert!(ready().can_capture());

        // Missing permission.
        let mut g = ready();
        g.set_permission(MicPermission::Denied);
        assert!(!g.can_capture());
        g.set_permission(MicPermission::Prompt);
        assert!(!g.can_capture());

        // No device.
        let mut g = ready();
        g.set_device(None);
        assert!(!g.can_capture());

        // Inactive session.
        let mut g = ready();
        g.set_session_active(false);
        assert!(!g.can_capture());
    }

    #[test]
    fn mute_is_fail_closed() {
        let mut g = ready();
        assert!(g.can_capture());
        g.mute();
        assert!(g.is_muted());
        assert!(!g.can_capture());
        g.unmute();
        assert!(g.can_capture());
    }

    #[test]
    fn fail_closed_mutes_on_uncertainty() {
        let mut g = ready();
        g.fail_closed();
        assert!(!g.can_capture());
        assert!(g.is_muted());
    }

    #[test]
    fn device_switch_drops_capture_until_confirmed() {
        let mut g = ready();
        assert!(g.can_capture());
        // Release the old device first (switch in progress).
        g.set_device(None);
        assert!(!g.can_capture());
        // New device confirmed.
        g.set_device(Some(2));
        assert!(g.can_capture());
    }

    #[test]
    fn reconnect_fails_closed_and_requires_re_establishment() {
        let mut g = ready();
        assert!(g.can_capture());
        g.on_reconnect();
        assert!(!g.can_capture());
        // Permission (a user decision) persists across the reconnect.
        assert_eq!(g.permission(), MicPermission::Granted);
        // Capture resumes only once the session and device are re-established.
        g.set_session_active(true);
        assert!(!g.can_capture()); // device still missing
        g.set_device(Some(3));
        assert!(g.can_capture());
    }
}
