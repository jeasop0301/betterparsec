use std::{
    ops::Range,
    sync::{Arc, atomic::AtomicU32},
};

use async_trait::async_trait;
use common::{
    api_bindings::{GeneralServerMessage, StreamSettings, StreamerStatsUpdate, TransportChannelId},
    ipc::{ServerIpcMessage, StreamerIpcMessage},
};
use log::warn;
use moonlight_common::stream::{
    audio::{AudioConfig, OpusMultistreamConfig},
    video::{DecodeResult, VideoDecodeUnit, VideoSetup},
};
use thiserror::Error;

use crate::cc::CcShared;
use common::buffer::ByteBuffer;

use self::metrics::VideoTransportStats;

pub(crate) mod cursor_tracker;
pub(crate) mod cursor_wire;
pub(crate) mod metrics;
pub mod web_socket;
pub mod webrtc;

// Input/control wire format promoted to common (unified-app risk #2) so the
// native client encodes with the exact code this crate decodes.
pub use common::input_wire::{InboundPacket, TransportChannel};

#[derive(Debug, Error)]
pub enum TransportError {
    #[error("the channel was closed")]
    ChannelClosed,
    #[error("the transport was closed")]
    Closed,
    #[error("implementation: {0}")]
    Implementation(anyhow::Error),
}

#[derive(Debug)]
pub enum OutboundPacket {
    General {
        message: GeneralServerMessage,
    },
    Stats(StreamerStatsUpdate),
    ControllerRumble {
        controller_number: u8,
        low_frequency_motor: u16,
        high_frequency_motor: u16,
    },
    ControllerTriggerRumble {
        controller_number: u8,
        left_trigger_motor: u16,
        right_trigger_motor: u16,
    },
    Rtt {
        sequence_number: u16,
    },
}

impl OutboundPacket {
    pub fn serialize(&self, raw_buffer: &mut Vec<u8>) -> Option<(TransportChannel, Range<usize>)> {
        match self {
            Self::General { message } => {
                let Ok(text) = serde_json::to_string(&message) else {
                    warn!("Failed to send general message: {message:?}");
                    return None;
                };
                raw_buffer.resize(text.len() + 2, 0u8);
                let mut buffer = ByteBuffer::new(raw_buffer as &mut [u8]);

                buffer.put_u16(text.len() as u16);
                buffer.put_utf8_raw(&text);

                buffer.flip();
                Some((
                    TransportChannel(TransportChannelId::GENERAL),
                    buffer.into_raw().1,
                ))
            }
            Self::Stats(stats) => {
                let Ok(text) = serde_json::to_string(&stats) else {
                    warn!("Failed to send stats message: {stats:?}");
                    return None;
                };
                raw_buffer.resize(text.len() + 2, 0u8);
                let mut buffer = ByteBuffer::new(raw_buffer as &mut [u8]);

                buffer.put_u16(text.len() as u16);
                buffer.put_utf8_raw(&text);

                buffer.flip();
                Some((
                    TransportChannel(TransportChannelId::STATS),
                    buffer.into_raw().1,
                ))
            }
            Self::ControllerRumble {
                controller_number,
                low_frequency_motor,
                high_frequency_motor,
            } => {
                raw_buffer.resize(6, 0);
                let mut buffer = ByteBuffer::new(raw_buffer as &mut [u8]);

                // Requires 6 bytes
                buffer.put_u8(0);
                buffer.put_u8(*controller_number);
                buffer.put_u16(*low_frequency_motor);
                buffer.put_u16(*high_frequency_motor);

                buffer.flip();
                Some((
                    TransportChannel(TransportChannelId::CONTROLLER0 + controller_number),
                    buffer.into_raw().1,
                ))
            }
            Self::ControllerTriggerRumble {
                controller_number,
                left_trigger_motor,
                right_trigger_motor,
            } => {
                raw_buffer.resize(6, 0);
                let mut buffer = ByteBuffer::new(raw_buffer as &mut [u8]);

                // Requires 6 bytes
                buffer.put_u8(0);
                buffer.put_u8(*controller_number);
                buffer.put_u16(*left_trigger_motor);
                buffer.put_u16(*right_trigger_motor);

                buffer.flip();
                Some((
                    TransportChannel(TransportChannelId::CONTROLLER0 + controller_number),
                    buffer.into_raw().1,
                ))
            }
            Self::Rtt { sequence_number } => {
                raw_buffer.resize(3, 0);
                let mut buffer = ByteBuffer::new(raw_buffer as &mut [u8]);

                buffer.put_u8(0);
                buffer.put_u16(*sequence_number);

                Some((
                    TransportChannel(TransportChannelId::RTT),
                    buffer.into_raw().1,
                ))
            }
        }
    }
}

#[derive(Debug)]
pub enum TransportEvent {
    StartStream { settings: StreamSettings },
    RecvPacket(InboundPacket),
    SendIpc(StreamerIpcMessage),
    Closed,
}

#[async_trait]
pub trait TransportEvents {
    /// Some InboundPackets are not handled by the consumer of this interface -> they must be handled by this Transport impl:
    /// - RequestIdr -> you should request an idr via the send_video_unit fn
    async fn poll_event(&mut self) -> Result<TransportEvent, TransportError>;
}
#[async_trait]
pub trait TransportSender {
    async fn setup_video(&self, setup: VideoSetup) -> i32;
    async fn send_video_unit<'a>(
        &'a self,
        unit: VideoDecodeUnit<&'a [u8]>,
    ) -> Result<DecodeResult, TransportError>;

    /// Takes and resets the current interval's application-owned video queue
    /// counters. Transports without an equivalent queue return `None`.
    fn take_video_transport_stats(&self) -> Option<VideoTransportStats> {
        None
    }

    /// Returns the live ABR target when this transport produces one.
    /// This is a target signal, not proof that the host encoder applied it.
    fn runtime_bitrate_target_kbps(&self) -> Option<Arc<AtomicU32>> {
        None
    }

    /// Returns the frame-delay CC shared state when this transport runs a
    /// congestion controller. The apply path composes its published target
    /// with the ABR target via [`crate::cc::effective_target_kbps`].
    fn runtime_cc_shared(&self) -> Option<Arc<CcShared>> {
        None
    }

    async fn setup_audio(
        &self,
        audio_config: AudioConfig,
        stream_config: OpusMultistreamConfig,
    ) -> i32;
    async fn send_audio_sample(&self, data: &[u8]) -> Result<(), TransportError>;

    async fn on_setup_complete(&self);

    async fn send(&self, packet: OutboundPacket) -> Result<(), TransportError>;

    async fn on_ipc_message(&self, message: ServerIpcMessage) -> Result<(), TransportError>;

    async fn close(&self) -> Result<(), TransportError>;
}
