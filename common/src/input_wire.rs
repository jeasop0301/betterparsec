//! Input/control DataChannel wire format — single source of truth.
//!
//! Promoted from `streamer/src/transport/mod.rs` (2026-07-14,
//! unified-app-architecture.md risk #2) so the native client encodes with
//! the exact code the streamer decodes, instead of hand-mirroring the TS
//! encoder (`web/stream/input.ts`).
//!
//! All multi-byte integers are **big-endian** (`ByteBuffer` default —
//! matches the TS `DataView` default). This is the opposite of the
//! `video_fec` wire (little-endian); do not mix the two conventions.

use log::warn;
use moonlight_common::stream::control::{
    ControllerButtons, ControllerCapabilities, ControllerType, KeyAction, KeyFlags, KeyModifiers,
    MouseButton, MouseButtonAction, TouchEventType,
};
use num::FromPrimitive;

use crate::api_bindings::{GeneralClientMessage, TransportChannelId};
use crate::buffer::ByteBuffer;

/// Look at TransportChannelId
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransportChannel(pub u8);

/// A packet travelling client → host on an input/control channel.
#[derive(Debug)]
pub enum InboundPacket {
    General {
        message: GeneralClientMessage,
    },
    MouseMove {
        delta_x: i16,
        delta_y: i16,
    },
    MousePosition {
        x: i16,
        y: i16,
        reference_width: i16,
        reference_height: i16,
    },
    MouseButton {
        action: MouseButtonAction,
        button: MouseButton,
    },
    HighResScroll {
        delta_x: i16,
        delta_y: i16,
    },
    Scroll {
        delta_x: i8,
        delta_y: i8,
    },
    Key {
        action: KeyAction,
        modifiers: KeyModifiers,
        key: u16,
        flags: KeyFlags,
    },
    Text {
        text: String,
    },
    ControllerConnected {
        id: u8,
        ty: ControllerType,
        supported_buttons: ControllerButtons,
        capabilities: ControllerCapabilities,
    },
    ControllerDisconnected {
        id: u8,
    },
    ControllerState {
        id: u8,
        buttons: ControllerButtons,
        left_trigger: u8,
        right_trigger: u8,
        left_stick_x: i16,
        left_stick_y: i16,
        right_stick_x: i16,
        right_stick_y: i16,
    },
    Touch {
        pointer_id: u32,
        x: f32,
        y: f32,
        pressure_or_distance: f32,
        contact_area_major: f32,
        contact_area_minor: f32,
        rotation: Option<u16>,
        event_type: TouchEventType,
    },
    Rtt {
        sequence_number: u16,
    },
    RequestVideoIdr,
}

impl InboundPacket {
    const DEFAULT_CONTROLLER_BUTTONS: ControllerButtons = ControllerButtons::all();
    const DEFAULT_CONTROLLER_CAPABILITIES: ControllerCapabilities = ControllerCapabilities::empty();

    pub const CONTROLLER_CHANNELS: [u8; 16] = [
        TransportChannelId::CONTROLLER0,
        TransportChannelId::CONTROLLER1,
        TransportChannelId::CONTROLLER2,
        TransportChannelId::CONTROLLER3,
        TransportChannelId::CONTROLLER4,
        TransportChannelId::CONTROLLER5,
        TransportChannelId::CONTROLLER6,
        TransportChannelId::CONTROLLER7,
        TransportChannelId::CONTROLLER8,
        TransportChannelId::CONTROLLER9,
        TransportChannelId::CONTROLLER10,
        TransportChannelId::CONTROLLER11,
        TransportChannelId::CONTROLLER12,
        TransportChannelId::CONTROLLER13,
        TransportChannelId::CONTROLLER14,
        TransportChannelId::CONTROLLER15,
    ];

    /// Encode this packet for sending client → host.
    ///
    /// Returns the default channel and payload bytes, or `None` for packets
    /// that cannot be encoded (unknown rotation semantics etc. do not
    /// exist — all variants encode). The mouse channel defaults mirror the
    /// web client: `MouseMove` → MOUSE_RELATIVE, `MousePosition` →
    /// MOUSE_ABSOLUTE, buttons/scroll → MOUSE_RELIABLE; callers that need a
    /// different mouse channel can still send the same payload there (the
    /// three mouse channels share one payload format).
    pub fn encode(&self) -> Option<(TransportChannel, Vec<u8>)> {
        let mut out = Vec::with_capacity(32);
        let channel = match self {
            InboundPacket::General { message } => {
                let json = serde_json::to_string(message).ok()?;
                let bytes = json.as_bytes();
                let len = u16::try_from(bytes.len()).ok()?;
                out.extend_from_slice(&len.to_be_bytes());
                out.extend_from_slice(bytes);
                TransportChannelId::GENERAL
            }
            InboundPacket::MouseMove { delta_x, delta_y } => {
                out.push(0);
                out.extend_from_slice(&delta_x.to_be_bytes());
                out.extend_from_slice(&delta_y.to_be_bytes());
                TransportChannelId::MOUSE_RELATIVE
            }
            InboundPacket::MousePosition {
                x,
                y,
                reference_width,
                reference_height,
            } => {
                out.push(1);
                out.extend_from_slice(&x.to_be_bytes());
                out.extend_from_slice(&y.to_be_bytes());
                out.extend_from_slice(&reference_width.to_be_bytes());
                out.extend_from_slice(&reference_height.to_be_bytes());
                TransportChannelId::MOUSE_ABSOLUTE
            }
            InboundPacket::MouseButton { action, button } => {
                out.push(2);
                out.push(u8::from(*action == MouseButtonAction::Press));
                out.push(*button as u8);
                TransportChannelId::MOUSE_RELIABLE
            }
            InboundPacket::HighResScroll { delta_x, delta_y } => {
                out.push(3);
                out.extend_from_slice(&delta_x.to_be_bytes());
                out.extend_from_slice(&delta_y.to_be_bytes());
                TransportChannelId::MOUSE_RELIABLE
            }
            InboundPacket::Scroll { delta_x, delta_y } => {
                out.push(4);
                out.extend_from_slice(&delta_x.to_be_bytes());
                out.extend_from_slice(&delta_y.to_be_bytes());
                TransportChannelId::MOUSE_RELIABLE
            }
            InboundPacket::Key {
                action,
                modifiers,
                key,
                flags: _, // not on the wire (decoder always yields empty)
            } => {
                out.push(0);
                out.push(u8::from(*action == KeyAction::Down));
                out.push(modifiers.bits() as u8);
                out.extend_from_slice(&key.to_be_bytes());
                TransportChannelId::KEYBOARD
            }
            InboundPacket::Text { text } => {
                // Length prefix counts CHARACTERS (decoder uses char count).
                let chars = u8::try_from(text.chars().count()).ok()?;
                out.push(1);
                out.push(chars);
                out.extend_from_slice(text.as_bytes());
                TransportChannelId::KEYBOARD
            }
            InboundPacket::ControllerConnected {
                id,
                ty: _, // not on the wire (decoder always yields Unknown)
                supported_buttons,
                capabilities,
            } => {
                out.push(0);
                out.push(*id);
                out.extend_from_slice(&supported_buttons.bits().to_be_bytes());
                out.extend_from_slice(&capabilities.bits().to_be_bytes());
                TransportChannelId::CONTROLLERS
            }
            InboundPacket::ControllerDisconnected { id } => {
                out.push(1);
                out.push(*id);
                TransportChannelId::CONTROLLERS
            }
            InboundPacket::ControllerState {
                id,
                buttons,
                left_trigger,
                right_trigger,
                left_stick_x,
                left_stick_y,
                right_stick_x,
                right_stick_y,
            } => {
                let channel = *Self::CONTROLLER_CHANNELS.get(*id as usize)?;
                out.push(0);
                out.extend_from_slice(&buttons.bits().to_be_bytes());
                out.push(*left_trigger);
                out.push(*right_trigger);
                out.extend_from_slice(&left_stick_x.to_be_bytes());
                out.extend_from_slice(&left_stick_y.to_be_bytes());
                out.extend_from_slice(&right_stick_x.to_be_bytes());
                out.extend_from_slice(&right_stick_y.to_be_bytes());
                channel
            }
            InboundPacket::Touch {
                pointer_id,
                x,
                y,
                pressure_or_distance,
                contact_area_major,
                contact_area_minor,
                rotation,
                event_type,
            } => {
                out.push(match event_type {
                    TouchEventType::Down => 0,
                    TouchEventType::Move => 1,
                    TouchEventType::Cancel => 2,
                    // The wire only carries Down/Move/Cancel (decoder domain);
                    // other host-side event types cannot be expressed.
                    _ => return None,
                });
                out.extend_from_slice(&pointer_id.to_be_bytes());
                out.extend_from_slice(&x.to_be_bytes());
                out.extend_from_slice(&y.to_be_bytes());
                out.extend_from_slice(&pressure_or_distance.to_be_bytes());
                out.extend_from_slice(&contact_area_major.to_be_bytes());
                out.extend_from_slice(&contact_area_minor.to_be_bytes());
                out.extend_from_slice(&rotation.unwrap_or(0).to_be_bytes());
                TransportChannelId::TOUCH
            }
            InboundPacket::Rtt { sequence_number } => {
                out.push(0);
                out.extend_from_slice(&sequence_number.to_be_bytes());
                TransportChannelId::RTT
            }
            InboundPacket::RequestVideoIdr => {
                out.push(0);
                TransportChannelId::HOST_VIDEO
            }
        };
        Some((TransportChannel(channel), out))
    }

    pub fn deserialize(channel: TransportChannel, bytes: &[u8]) -> Option<Self> {
        let mut buffer = ByteBuffer::new(bytes);

        match channel {
            TransportChannel(TransportChannelId::GENERAL) => {
                if buffer.remaining() < 2 {
                    warn!("[InboudPacket]: failed to read general message");
                    return None;
                }

                let len = buffer.get_u16();
                let text = match buffer.get_utf8_raw(len as usize) {
                    Ok(text) => text,
                    Err(err) => {
                        warn!("[InboudPacket]: failed to read a general message: {err}");
                        return None;
                    }
                };
                let message = match serde_json::from_str(text) {
                    Ok(message) => message,
                    Err(err) => {
                        warn!("[InboudPacket]: failed to deserialize general message: {err}");
                        return None;
                    }
                };

                Some(Self::General { message })
            }
            TransportChannel(TransportChannelId::STATS) => {
                warn!("[InboundPacket]: tried to deserialize stats packet, this shouldn't happen");
                None
            }
            TransportChannel(TransportChannelId::HOST_VIDEO) => {
                if buffer.remaining() < 1 {
                    warn!("[InboudPacket]: failed to video message");
                    return None;
                }

                let ty = buffer.get_u8();
                if ty == 0 {
                    Some(InboundPacket::RequestVideoIdr)
                } else {
                    warn!("[InboundPacket]: failed to deserialize host video packet");
                    None
                }
            }
            TransportChannel(TransportChannelId::HOST_AUDIO) => {
                warn!(
                    "[InboundPacket]: tried to deserialize host audio packet, this shouldn't happen"
                );
                None
            }
            TransportChannel(
                TransportChannelId::MOUSE_ABSOLUTE
                | TransportChannelId::MOUSE_RELIABLE
                | TransportChannelId::MOUSE_RELATIVE,
            ) => {
                if buffer.remaining() < 1 {
                    warn!("[InboudPacket]: failed to read mouse message");
                    return None;
                }

                let ty = buffer.get_u8();
                if ty == 0 {
                    // Move
                    if buffer.remaining() < 4 {
                        warn!("[InboudPacket]: failed to read mouse move message");
                        return None;
                    }

                    let delta_x = buffer.get_i16();
                    let delta_y = buffer.get_i16();

                    Some(InboundPacket::MouseMove { delta_x, delta_y })
                } else if ty == 1 {
                    // Position
                    if buffer.remaining() < 8 {
                        warn!("[InboudPacket]: failed to read mouse position message");
                        return None;
                    }

                    let x = buffer.get_i16();
                    let y = buffer.get_i16();
                    let reference_width = buffer.get_i16();
                    let reference_height = buffer.get_i16();

                    Some(InboundPacket::MousePosition {
                        x,
                        y,
                        reference_width,
                        reference_height,
                    })
                } else if ty == 2 {
                    // Button Press / Release
                    if buffer.remaining() < 2 {
                        warn!("[InboudPacket]: failed to read mouse press / release message");
                        return None;
                    }

                    let action = if buffer.get_bool() {
                        MouseButtonAction::Press
                    } else {
                        MouseButtonAction::Release
                    };
                    let Some(button) = MouseButton::from_u8(buffer.get_u8()) else {
                        warn!("[InboundPacket]: received invalid mouse button");
                        return None;
                    };

                    Some(InboundPacket::MouseButton { action, button })
                } else if ty == 3 {
                    // Mouse Wheel High Res
                    if buffer.remaining() < 4 {
                        warn!("[InboudPacket]: failed to read mouse wheel high res message");
                        return None;
                    }

                    let delta_x = buffer.get_i16();
                    let delta_y = buffer.get_i16();

                    Some(InboundPacket::HighResScroll { delta_x, delta_y })
                } else if ty == 4 {
                    // Mouse Wheel Normal
                    if buffer.remaining() < 2 {
                        warn!("[InboudPacket]: failed to read mouse wheel normal message");
                        return None;
                    }

                    let delta_x = buffer.get_i8();
                    let delta_y = buffer.get_i8();

                    Some(InboundPacket::Scroll { delta_x, delta_y })
                } else {
                    warn!(
                        "[InboundPacket]: tried to deserialize mouse packet with type {ty}, this shouldn't happen"
                    );
                    None
                }
            }
            TransportChannel(TransportChannelId::KEYBOARD) => {
                if buffer.remaining() < 1 {
                    warn!("[InboudPacket]: failed to read keyboard message");
                    return None;
                }

                let ty = buffer.get_u8();
                if ty == 0 {
                    // Key press / release
                    if buffer.remaining() < 4 {
                        warn!("[InboudPacket]: failed to read key press / release message");
                        return None;
                    }

                    let action = if buffer.get_bool() {
                        KeyAction::Down
                    } else {
                        KeyAction::Up
                    };
                    let modifiers =
                        KeyModifiers::from_bits(buffer.get_u8() as i8).unwrap_or_else(|| {
                            warn!("[InboundPacket]: received invalid key modifiers");
                            KeyModifiers::empty()
                        });
                    let key = buffer.get_u16();

                    Some(InboundPacket::Key {
                        action,
                        modifiers,
                        key,
                        flags: KeyFlags::empty(),
                    })
                } else if ty == 1 {
                    if buffer.remaining() < 1 {
                        warn!("[InboudPacket]: failed to read key as text message");
                        return None;
                    }

                    let len = buffer.get_u8();
                    let Ok(key) = buffer.get_utf8_raw(len as usize) else {
                        warn!("[InboundPacket]: received invalid keyboard text message");
                        return None;
                    };

                    Some(InboundPacket::Text {
                        text: key.to_owned(),
                    })
                } else {
                    warn!(
                        "[InboundPacket]: tried to deserialize keyboard packet with type {ty}, this shouldn't happen"
                    );
                    None
                }
            }
            TransportChannel(TransportChannelId::TOUCH) => {
                if buffer.remaining() < 27 {
                    warn!("[InboudPacket]: failed to read touch message");
                    return None;
                }

                let event_type = match buffer.get_u8() {
                    0 => TouchEventType::Down,
                    1 => TouchEventType::Move,
                    2 => TouchEventType::Cancel,
                    _ => {
                        warn!("[InboundPacket]: received invalid touch event type");
                        return None;
                    }
                };
                let pointer_id = buffer.get_u32();
                let x = buffer.get_f32();
                let y = buffer.get_f32();
                let pressure_or_distance = buffer.get_f32();
                let contact_area_major = buffer.get_f32();
                let contact_area_minor = buffer.get_f32();
                let rotation = buffer.get_u16();

                Some(InboundPacket::Touch {
                    pointer_id,
                    x,
                    y,
                    pressure_or_distance,
                    contact_area_major,
                    contact_area_minor,
                    rotation: Some(rotation),
                    event_type,
                })
            }
            TransportChannel(TransportChannelId::CONTROLLERS) => {
                if buffer.remaining() < 1 {
                    warn!("[InboudPacket]: failed to read controller message");
                    return None;
                }

                let ty = buffer.get_u8();
                if ty == 0 {
                    // add controller
                    if buffer.remaining() < 7 {
                        warn!("[InboudPacket]: failed to controller add message");
                        return None;
                    }

                    let id = buffer.get_u8();
                    let supported_buttons = ControllerButtons::from_bits(buffer.get_u32())
                        .unwrap_or_else(|| {
                            warn!(
                                "[InboundPacket]: received a controller with invalid button layout"
                            );
                            Self::DEFAULT_CONTROLLER_BUTTONS
                        });
                    let capabilities = ControllerCapabilities::from_bits(buffer.get_u16())
                        .unwrap_or_else(|| {
                            warn!(
                                "[InboundPacket]: received a controller with invalid capabilities"
                            );
                            Self::DEFAULT_CONTROLLER_CAPABILITIES
                        });

                    Some(InboundPacket::ControllerConnected {
                        id,
                        ty: ControllerType::Unknown,
                        supported_buttons,
                        capabilities,
                    })
                } else if ty == 1 {
                    // Remove controller
                    if buffer.remaining() < 1 {
                        warn!("[InboudPacket]: failed to read controller remove message");
                        return None;
                    }

                    let id = buffer.get_u8();

                    Some(InboundPacket::ControllerDisconnected { id })
                } else {
                    warn!(
                        "[InboundPacket]: tried to deserialize controllers packet with type {ty}, this shouldn't happen"
                    );
                    None
                }
            }
            TransportChannel(channel_id) if Self::CONTROLLER_CHANNELS.contains(&channel_id) => {
                let Some((gamepad_id, _)) = Self::CONTROLLER_CHANNELS
                    .iter()
                    .enumerate()
                    .find(|(_, cmp_channel_id)| **cmp_channel_id == channel_id)
                else {
                    warn!("[InboundPacket]: unknown transport channel: {channel_id}");
                    return None;
                };

                if buffer.remaining() < 1 {
                    warn!(
                        "[InboudPacket]: failed to read controller state message {channel_id}, gamepad: {gamepad_id}"
                    );
                    return None;
                }

                let ty = buffer.get_u8();
                if ty == 0 {
                    // State
                    if buffer.remaining() < 14 {
                        warn!(
                            "[InboudPacket]: failed to read controller state message {channel_id}, gamepad: {gamepad_id}"
                        );
                        return None;
                    }

                    let Some(buttons) = ControllerButtons::from_bits(buffer.get_u32()) else {
                        warn!(
                            "[InboundPacket]: received invalid controller buttons for controller {gamepad_id}"
                        );
                        return None;
                    };

                    let left_trigger = buffer.get_u8();
                    let right_trigger = buffer.get_u8();
                    let left_stick_x = buffer.get_i16();
                    let left_stick_y = buffer.get_i16();
                    let right_stick_x = buffer.get_i16();
                    let right_stick_y = buffer.get_i16();

                    Some(InboundPacket::ControllerState {
                        id: gamepad_id as u8,
                        buttons,
                        left_trigger,
                        right_trigger,
                        left_stick_x,
                        left_stick_y,
                        right_stick_x,
                        right_stick_y,
                    })
                } else {
                    warn!(
                        "[InboundPacket]: tried to deserialize controller {gamepad_id} packet with type {ty}, this shouldn't happen"
                    );
                    None
                }
            }
            TransportChannel(TransportChannelId::RTT) => {
                let ty = buffer.get_u8();

                if ty == 0 {
                    if buffer.remaining() < 2 {
                        return None;
                    }
                    let sequence_number = buffer.get_u16();

                    Some(InboundPacket::Rtt { sequence_number })
                } else {
                    warn!(
                        "[InboundPacket]: tried to deserialize rtt packet with type {ty}, this shouldn't happen"
                    );
                    None
                }
            }
            _ => None,
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// encode → deserialize must reproduce the packet (field-for-field for
    /// everything the wire carries).
    fn round_trip(p: InboundPacket) -> InboundPacket {
        let (channel, bytes) = p.encode().expect("encodable");
        InboundPacket::deserialize(channel, &bytes).expect("decodable")
    }

    #[test]
    fn mouse_move_round_trip_and_byte_pin() {
        let (ch, bytes) = InboundPacket::MouseMove {
            delta_x: -2,
            delta_y: 300,
        }
        .encode()
        .expect("encodable");
        assert_eq!(ch.0, TransportChannelId::MOUSE_RELATIVE);
        // Byte pin (big-endian, TS DataView parity): [0, i16, i16].
        assert_eq!(bytes, [0, 0xFF, 0xFE, 0x01, 0x2C]);

        match round_trip(InboundPacket::MouseMove {
            delta_x: -2,
            delta_y: 300,
        }) {
            InboundPacket::MouseMove { delta_x, delta_y } => {
                assert_eq!((delta_x, delta_y), (-2, 300));
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn mouse_position_button_scroll_round_trips() {
        match round_trip(InboundPacket::MousePosition {
            x: 10,
            y: -20,
            reference_width: 1920,
            reference_height: 1080,
        }) {
            InboundPacket::MousePosition {
                x,
                y,
                reference_width,
                reference_height,
            } => assert_eq!(
                (x, y, reference_width, reference_height),
                (10, -20, 1920, 1080)
            ),
            other => panic!("wrong variant: {other:?}"),
        }

        match round_trip(InboundPacket::MouseButton {
            action: MouseButtonAction::Press,
            button: MouseButton::Left,
        }) {
            InboundPacket::MouseButton { action, button } => {
                assert_eq!(action, MouseButtonAction::Press);
                assert_eq!(button, MouseButton::Left);
            }
            other => panic!("wrong variant: {other:?}"),
        }

        match round_trip(InboundPacket::HighResScroll {
            delta_x: -120,
            delta_y: 120,
        }) {
            InboundPacket::HighResScroll { delta_x, delta_y } => {
                assert_eq!((delta_x, delta_y), (-120, 120));
            }
            other => panic!("wrong variant: {other:?}"),
        }

        match round_trip(InboundPacket::Scroll {
            delta_x: -1,
            delta_y: 3,
        }) {
            InboundPacket::Scroll { delta_x, delta_y } => {
                assert_eq!((delta_x, delta_y), (-1, 3));
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn key_round_trip_and_byte_pin() {
        let (ch, bytes) = InboundPacket::Key {
            action: KeyAction::Down,
            modifiers: KeyModifiers::empty(),
            key: 0x41, // 'A'
            flags: KeyFlags::empty(),
        }
        .encode()
        .expect("encodable");
        assert_eq!(ch.0, TransportChannelId::KEYBOARD);
        // [ty=0, down=1, modifiers=0, key u16 BE]
        assert_eq!(bytes, [0, 1, 0, 0x00, 0x41]);

        match round_trip(InboundPacket::Key {
            action: KeyAction::Up,
            modifiers: KeyModifiers::empty(),
            key: 0x1B,
            flags: KeyFlags::empty(),
        }) {
            InboundPacket::Key { action, key, .. } => {
                assert_eq!(action, KeyAction::Up);
                assert_eq!(key, 0x1B);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn text_round_trip_counts_characters_not_bytes() {
        // Multi-byte UTF-8: the length prefix is a character count
        // (decoder contract via get_utf8_raw).
        match round_trip(InboundPacket::Text {
            text: "한글ab".to_string(),
        }) {
            InboundPacket::Text { text } => assert_eq!(text, "한글ab"),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn controller_lifecycle_round_trips() {
        match round_trip(InboundPacket::ControllerConnected {
            id: 2,
            ty: ControllerType::Unknown,
            supported_buttons: ControllerButtons::all(),
            capabilities: ControllerCapabilities::empty(),
        }) {
            InboundPacket::ControllerConnected {
                id,
                supported_buttons,
                capabilities,
                ..
            } => {
                assert_eq!(id, 2);
                assert_eq!(supported_buttons, ControllerButtons::all());
                assert_eq!(capabilities, ControllerCapabilities::empty());
            }
            other => panic!("wrong variant: {other:?}"),
        }

        match round_trip(InboundPacket::ControllerDisconnected { id: 5 }) {
            InboundPacket::ControllerDisconnected { id } => assert_eq!(id, 5),
            other => panic!("wrong variant: {other:?}"),
        }

        // State goes on the per-controller channel; the id round-trips via
        // the channel index.
        let p = InboundPacket::ControllerState {
            id: 3,
            buttons: ControllerButtons::all(),
            left_trigger: 10,
            right_trigger: 250,
            left_stick_x: -32768,
            left_stick_y: 32767,
            right_stick_x: 0,
            right_stick_y: -1,
        };
        let (ch, bytes) = p.encode().expect("encodable");
        assert_eq!(ch.0, TransportChannelId::CONTROLLER3);
        match InboundPacket::deserialize(ch, &bytes).expect("decodable") {
            InboundPacket::ControllerState {
                id,
                left_stick_x,
                right_stick_y,
                ..
            } => {
                assert_eq!(id, 3);
                assert_eq!(left_stick_x, -32768);
                assert_eq!(right_stick_y, -1);
            }
            other => panic!("wrong variant: {other:?}"),
        }

        // Out-of-range controller id is unencodable, not a panic.
        assert!(
            InboundPacket::ControllerState {
                id: 16,
                buttons: ControllerButtons::all(),
                left_trigger: 0,
                right_trigger: 0,
                left_stick_x: 0,
                left_stick_y: 0,
                right_stick_x: 0,
                right_stick_y: 0,
            }
            .encode()
            .is_none()
        );
    }

    #[test]
    fn touch_rtt_idr_round_trips() {
        match round_trip(InboundPacket::Touch {
            pointer_id: 7,
            x: 0.5,
            y: 0.25,
            pressure_or_distance: 1.0,
            contact_area_major: 0.1,
            contact_area_minor: 0.05,
            rotation: Some(90),
            event_type: TouchEventType::Move,
        }) {
            InboundPacket::Touch {
                pointer_id,
                x,
                rotation,
                event_type,
                ..
            } => {
                assert_eq!(pointer_id, 7);
                assert_eq!(x, 0.5);
                assert_eq!(rotation, Some(90));
                assert_eq!(event_type, TouchEventType::Move);
            }
            other => panic!("wrong variant: {other:?}"),
        }

        match round_trip(InboundPacket::Rtt {
            sequence_number: 4242,
        }) {
            InboundPacket::Rtt { sequence_number } => assert_eq!(sequence_number, 4242),
            other => panic!("wrong variant: {other:?}"),
        }

        match round_trip(InboundPacket::RequestVideoIdr) {
            InboundPacket::RequestVideoIdr => {}
            other => panic!("wrong variant: {other:?}"),
        }
    }
}
