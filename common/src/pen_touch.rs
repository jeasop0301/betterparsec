//! Pen and touch input (G027). A multi-touch contact-lifetime tracker that
//! enforces the contact invariants (a contact must go down before it can move
//! or end, no duplicate-down, a bounded number of simultaneous contacts, and a
//! cancel-all on reconnect/focus-loss), plus bounded pen pressure/tilt
//! normalization and a client->stream display transform for contact
//! coordinates.
//!
//! Pure and headless. The Win32/PointerEvent capture (app-native/src/input.rs,
//! web/stream/input.ts), the input_wire encoding, and host pen/touch injection
//! build on this. The physical Wacom/Surface/touch matrix is separately gated.

/// The lifecycle phase of a touch contact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TouchPhase {
    Down,
    Move,
    Up,
    Cancel,
}

/// Maximum simultaneous touch contacts tracked.
pub const MAX_CONTACTS: usize = 10;

/// Maximum pen pressure (normalized range `0..=PEN_PRESSURE_MAX`).
pub const PEN_PRESSURE_MAX: u16 = 1024;

/// The result of applying a touch event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContactOutcome {
    /// A new contact went down.
    Started,
    /// An existing contact moved.
    Updated,
    /// A contact ended (up or cancel).
    Ended,
    /// The event violated a contact invariant and was dropped.
    Rejected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Contact {
    id: u32,
    x: i32,
    y: i32,
}

/// Tracks active touch contacts and enforces their lifetime invariants.
#[derive(Debug, Clone, Default)]
pub struct ContactTracker {
    active: Vec<Contact>,
}

impl ContactTracker {
    /// An empty tracker.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of active contacts.
    pub fn active_count(&self) -> usize {
        self.active.len()
    }

    /// Whether a contact with `id` is currently down.
    pub fn is_down(&self, id: u32) -> bool {
        self.active.iter().any(|c| c.id == id)
    }

    /// Apply one touch event, enforcing the contact invariants.
    pub fn apply(&mut self, id: u32, phase: TouchPhase, x: i32, y: i32) -> ContactOutcome {
        match phase {
            TouchPhase::Down => {
                if self.is_down(id) || self.active.len() >= MAX_CONTACTS {
                    return ContactOutcome::Rejected;
                }
                self.active.push(Contact { id, x, y });
                ContactOutcome::Started
            }
            TouchPhase::Move => {
                if let Some(contact) = self.active.iter_mut().find(|c| c.id == id) {
                    contact.x = x;
                    contact.y = y;
                    ContactOutcome::Updated
                } else {
                    ContactOutcome::Rejected
                }
            }
            TouchPhase::Up | TouchPhase::Cancel => {
                if let Some(pos) = self.active.iter().position(|c| c.id == id) {
                    self.active.remove(pos);
                    ContactOutcome::Ended
                } else {
                    ContactOutcome::Rejected
                }
            }
        }
    }

    /// Cancel every active contact (reconnect / focus loss). Returns how many
    /// were cancelled.
    pub fn cancel_all(&mut self) -> usize {
        let n = self.active.len();
        self.active.clear();
        n
    }
}

/// Normalize a pen sample: pressure clamped to `0..=PEN_PRESSURE_MAX`, tilt
/// clamped to the physical `-90..=90` degree range.
pub fn normalize_pen(pressure: u16, tilt_x: i32, tilt_y: i32) -> (u16, i8, i8) {
    let clamp_tilt = |t: i32| t.clamp(-90, 90) as i8;
    (
        pressure.min(PEN_PRESSURE_MAX),
        clamp_tilt(tilt_x),
        clamp_tilt(tilt_y),
    )
}

/// Transform a contact coordinate from the client rect into the stream
/// reference space. `None` when the client rect is degenerate.
pub fn transform_contact(
    x: i32,
    y: i32,
    client_w: i32,
    client_h: i32,
    stream_w: i32,
    stream_h: i32,
) -> Option<(i32, i32)> {
    if client_w <= 0 || client_h <= 0 {
        return None;
    }
    Some((
        (i64::from(x) * i64::from(stream_w) / i64::from(client_w)) as i32,
        (i64::from(y) * i64::from(stream_h) / i64::from(client_h)) as i32,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contact_lifetime_invariants() {
        let mut t = ContactTracker::new();
        // Move/Up before Down are rejected.
        assert_eq!(t.apply(1, TouchPhase::Move, 0, 0), ContactOutcome::Rejected);
        assert_eq!(t.apply(1, TouchPhase::Up, 0, 0), ContactOutcome::Rejected);
        // Down, then Move, then Up.
        assert_eq!(
            t.apply(1, TouchPhase::Down, 10, 20),
            ContactOutcome::Started
        );
        assert!(t.is_down(1));
        // Duplicate down is rejected.
        assert_eq!(t.apply(1, TouchPhase::Down, 0, 0), ContactOutcome::Rejected);
        assert_eq!(
            t.apply(1, TouchPhase::Move, 30, 40),
            ContactOutcome::Updated
        );
        assert_eq!(t.apply(1, TouchPhase::Up, 30, 40), ContactOutcome::Ended);
        assert!(!t.is_down(1));
        // Move after Up is rejected again.
        assert_eq!(t.apply(1, TouchPhase::Move, 0, 0), ContactOutcome::Rejected);
    }

    #[test]
    fn tracks_multiple_contacts_and_bounds_them() {
        let mut t = ContactTracker::new();
        for id in 0..MAX_CONTACTS as u32 {
            assert_eq!(t.apply(id, TouchPhase::Down, 0, 0), ContactOutcome::Started);
        }
        assert_eq!(t.active_count(), MAX_CONTACTS);
        // One more than the bound is rejected.
        assert_eq!(
            t.apply(999, TouchPhase::Down, 0, 0),
            ContactOutcome::Rejected
        );
        // Each contact ends independently.
        assert_eq!(t.apply(0, TouchPhase::Up, 0, 0), ContactOutcome::Ended);
        assert_eq!(t.active_count(), MAX_CONTACTS - 1);
    }

    #[test]
    fn cancel_all_clears_every_contact() {
        let mut t = ContactTracker::new();
        t.apply(1, TouchPhase::Down, 0, 0);
        t.apply(2, TouchPhase::Down, 0, 0);
        assert_eq!(t.cancel_all(), 2);
        assert_eq!(t.active_count(), 0);
        // After cancel, a stray move is rejected.
        assert_eq!(t.apply(1, TouchPhase::Move, 0, 0), ContactOutcome::Rejected);
    }

    #[test]
    fn pen_pressure_and_tilt_are_bounded() {
        assert_eq!(normalize_pen(2000, 200, -200), (PEN_PRESSURE_MAX, 90, -90));
        assert_eq!(normalize_pen(512, 45, -30), (512, 45, -30));
        assert_eq!(normalize_pen(0, 0, 0), (0, 0, 0));
    }

    #[test]
    fn contact_display_transform() {
        assert_eq!(
            transform_contact(960, 540, 1920, 1080, 3840, 2160),
            Some((1920, 1080))
        );
        assert_eq!(transform_contact(0, 0, 0, 1080, 3840, 2160), None);
    }
}
