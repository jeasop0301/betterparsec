use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use async_trait::async_trait;
use bytes::Bytes;
use common::{
    api_bindings::{StreamClientMessage, StreamerStatsUpdate, TransportChannelId},
    ipc::{ServerIpcMessage, StreamerIpcMessage},
};
use log::{trace, warn};
use moonlight_common::stream::{
    audio::{AudioConfig, OpusMultistreamConfig},
    video::{DecodeResult, FrameType, VideoDecodeUnit, VideoSetup},
};
use tokio::{
    spawn,
    sync::{
        Mutex,
        mpsc::{Receiver, Sender, channel},
    },
    time::sleep,
};

use crate::transport::{
    InboundPacket, OutboundPacket, TransportChannel, TransportError, TransportEvent,
    TransportEvents, TransportSender,
};
use common::buffer::ByteBuffer;

pub async fn new() -> Result<(WebSocketTransportSender, WebSocketTransportEvents), anyhow::Error> {
    let (event_sender, event_receiver) = channel::<TransportEvent>(20);

    // TODO: use the video_frame_queue_size with packet rtt info to estimate latency of pictures and request idr if too big
    // P2 wire-in (deferred, S5): `should_request_idr` below is the pure
    // decision fn for this. Wiring it here needs `video_frame_queue_size`
    // threaded into this fn (it isn't a parameter of `new` today) plus a
    // live RTT sample source (see `recv_rtt`/`rtt` field below) sampled on
    // an interval to drive `WebSocketTransportSender::needs_idr`. Deferred
    // as a later live A/B slice; pure fn + test land now.

    let (clipboard_apply_tx, clipboard_apply_rx) = channel::<String>(20);

    let sender = WebSocketTransportSender {
        event_sender,
        rtt: Arc::new(Mutex::new((Instant::now(), 0))),
        needs_idr: AtomicBool::new(false),
        clipboard_apply_tx,
    };

    // This will start the loop of sending / receiving
    recv_rtt(sender.rtt.clone(), sender.event_sender.clone(), 0).await;

    // M4 cursor P1: host-authority cursor channel, transport-agnostic —
    // on this transport the POS wire rides a CURSOR-prefixed ws frame
    // (cursor-channel.md §3; DCV ships the same authority over TCP).
    crate::transport::cursor_tracker::spawn(crate::transport::cursor_tracker::WebSocketCursorSink(
        sender.event_sender.clone(),
    ));
    // Clipboard sync v1: bidirectional text sync, transport-agnostic — on
    // this transport both directions ride CLIPBOARD-prefixed ws frames
    // (clipboard.rs; research/05 gap vs Parsec/DCV).
    crate::transport::clipboard::spawn(
        crate::transport::clipboard::WebSocketClipboardSink(sender.event_sender.clone()),
        clipboard_apply_rx,
    );

    Ok((sender, WebSocketTransportEvents { event_receiver }))
}

pub struct WebSocketTransportEvents {
    event_receiver: Receiver<TransportEvent>,
}

#[async_trait]
impl TransportEvents for WebSocketTransportEvents {
    async fn poll_event(&mut self) -> Result<TransportEvent, TransportError> {
        trace!("Polling WebSocketEvents");
        self.event_receiver
            .recv()
            .await
            .ok_or(TransportError::Closed)
    }
}

pub struct WebSocketTransportSender {
    event_sender: Sender<TransportEvent>,
    /// Time when it was sent, sequence_number
    rtt: Arc<Mutex<(Instant, u16)>>,
    needs_idr: AtomicBool,
    /// Forwards decoded inbound CLIPBOARD text (client → host) to the
    /// clipboard watcher's apply path (transport::clipboard).
    clipboard_apply_tx: Sender<String>,
}

async fn send_packet(
    event_sender: &Sender<TransportEvent>,
    packet: OutboundPacket,
) -> Result<(), TransportError> {
    let mut new_buffer = Vec::new();

    let (id, mut range) = match packet.serialize(&mut new_buffer) {
        Some(packet) => packet,
        None => {
            warn!("Failed to serialize packet: {packet:?}");
            return Ok(());
        }
    };

    if range.start == 0 {
        new_buffer.resize(range.end - range.start + 1, 0);
        new_buffer.copy_within(range.clone(), range.start + 1);
        range.start += 1;
    }
    new_buffer[range.start - 1] = id.0;

    if event_sender
        .send(TransportEvent::SendIpc(
            StreamerIpcMessage::WebSocketTransport(Bytes::from(new_buffer)),
        ))
        .await
        .is_err()
    {
        return Err(TransportError::Closed);
    }

    Ok(())
}

async fn recv_rtt(
    rtt_mutex: Arc<Mutex<(Instant, u16)>>,
    event_sender: Sender<TransportEvent>,
    recv_sequence_number: u16,
) {
    let (send, mut sequence_number) = {
        let rtt = rtt_mutex.lock().await;
        *rtt
    };

    let now = Instant::now();
    if recv_sequence_number != sequence_number {
        warn!(
            "Expected rtt packet with sequence_number {sequence_number} but got {recv_sequence_number}"
        );
    }

    // Calc rtt
    let rtt = now - send;

    // Send rtt via stats
    if let Err(err) = send_packet(
        &event_sender,
        OutboundPacket::Stats(StreamerStatsUpdate::BrowserRtt {
            rtt_ms: rtt.as_secs_f64() * 1000.0,
        }),
    )
    .await
    {
        warn!("Failed to send rtt stats update for web socket: {err}");
    }

    // Wait a few ms
    sleep(Duration::from_millis(200)).await;

    sequence_number += 1;
    {
        let mut rtt = rtt_mutex.lock().await;
        *rtt = (Instant::now(), sequence_number);
    }

    // Send new rtt packet
    if let Err(err) = send_packet(&event_sender, OutboundPacket::Rtt { sequence_number }).await {
        warn!("Failed to send web socket rtt packet with sequence number {sequence_number}: {err}");
    }
}

#[async_trait]
impl TransportSender for WebSocketTransportSender {
    async fn setup_video(&self, _setup: VideoSetup) -> i32 {
        // empty
        0
    }
    async fn send_video_unit<'a>(
        &'a self,
        unit: VideoDecodeUnit<&'a [u8]>,
    ) -> Result<DecodeResult, TransportError> {
        let mut new_buffer = vec![0; 6];

        let mut byte_buffer = ByteBuffer::new(new_buffer.as_mut_slice());
        byte_buffer.put_u8(TransportChannelId::HOST_VIDEO);
        byte_buffer.put_u8(match unit.frame_type {
            FrameType::Idr => 1,
            FrameType::PFrame => 0,
        });
        byte_buffer.put_u32(unit.timestamp.as_micros() as u32);

        for buffer in &unit.buffers {
            new_buffer.extend_from_slice(buffer.data);
        }
        // TODO: ignore h264/h265 fillerdata?
        if self
            .event_sender
            .send(TransportEvent::SendIpc(
                StreamerIpcMessage::WebSocketTransport(Bytes::from(new_buffer)),
            ))
            .await
            .is_err()
        {
            return Err(TransportError::Closed);
        }

        if self
            .needs_idr
            .compare_exchange(true, false, Ordering::SeqCst, Ordering::Relaxed)
            .is_ok()
        {
            return Ok(DecodeResult::NeedIdr);
        }

        Ok(DecodeResult::Ok)
    }

    async fn setup_audio(
        &self,
        _audio_config: AudioConfig,
        _stream_config: OpusMultistreamConfig,
    ) -> i32 {
        // empty
        0
    }
    async fn send_audio_sample(&self, data: &[u8]) -> Result<(), TransportError> {
        let mut new_buffer = vec![0];

        let mut byte_buffer = ByteBuffer::new(new_buffer.as_mut_slice());
        byte_buffer.put_u8(TransportChannelId::HOST_AUDIO);

        new_buffer.extend_from_slice(data);

        if self
            .event_sender
            .send(TransportEvent::SendIpc(
                StreamerIpcMessage::WebSocketTransport(Bytes::from(new_buffer)),
            ))
            .await
            .is_err()
        {
            return Err(TransportError::Closed);
        }

        Ok(())
    }

    async fn send(&self, packet: OutboundPacket) -> Result<(), TransportError> {
        send_packet(&self.event_sender, packet).await
    }

    async fn on_ipc_message(&self, message: ServerIpcMessage) -> Result<(), TransportError> {
        match message {
            ServerIpcMessage::WebSocketTransport(message) => {
                if message.is_empty() {
                    warn!("Empty packet received!");
                    return Ok(());
                }

                let channel_id = message[0];
                // Clipboard sync v1: CLIPBOARD frames carry the clipboard
                // wire format, not an InboundPacket — route them to the
                // watcher's apply path before InboundPacket::deserialize,
                // which does not know this channel and would only warn.
                if channel_id == TransportChannelId::CLIPBOARD {
                    if let Some(text) = crate::transport::clipboard::decode_text(&message[1..])
                        && self.clipboard_apply_tx.send(text).await.is_err()
                    {
                        warn!("[Clipboard] apply channel closed — dropping inbound text");
                    }
                    return Ok(());
                }
                let Some(packet) =
                    InboundPacket::deserialize(TransportChannel(channel_id), &message[1..])
                else {
                    warn!("Failed to receive packet on channel {channel_id}");
                    return Ok(());
                };

                if let InboundPacket::RequestVideoIdr = packet {
                    self.needs_idr.store(true, Ordering::Release);
                }

                if let InboundPacket::Rtt { sequence_number } = packet {
                    spawn(recv_rtt(
                        self.rtt.clone(),
                        self.event_sender.clone(),
                        sequence_number,
                    ));
                }

                if self
                    .event_sender
                    .send(TransportEvent::RecvPacket(packet))
                    .await
                    .is_err()
                {
                    return Err(TransportError::Closed);
                }
            }
            #[allow(clippy::collapsible_match)]
            ServerIpcMessage::WebSocket(StreamClientMessage::StartStream { settings }) => {
                if self
                    .event_sender
                    .send(TransportEvent::StartStream { settings })
                    .await
                    .is_err()
                {
                    warn!("Failed to send start stream event");
                    return Err(TransportError::Closed);
                }
            }
            // M4 stall watchdog: signaling-socket IDR request (RestartIce is
            // WebRTC-only and intentionally falls through to `_`).
            ServerIpcMessage::WebSocket(StreamClientMessage::RequestIdr) => {
                self.needs_idr.store(true, Ordering::Release);
            }
            _ => {}
        }
        Ok(())
    }

    async fn on_setup_complete(&self) {
        // empty
    }

    async fn close(&self) -> Result<(), TransportError> {
        // emtpy
        Ok(())
    }
}

/// Pure RTT-aware IDR-request decision (P2 TODO wire-in deferred — see
/// comment in [`new`] above). Estimates the queue's drain latency as
/// `frame_queue_size * frame_interval_ms` and compares it, plus one RTT of
/// round-trip slack, against `max_latency_ms`. Returns `true` when the
/// queue has backed up far enough that a fresh IDR should be requested
/// before the picture backlog exceeds the acceptable latency budget.
#[allow(dead_code)]
fn should_request_idr(
    frame_queue_size: usize,
    rtt_ms: u32,
    frame_interval_ms: u32,
    max_latency_ms: u32,
) -> bool {
    let queue_latency_ms = u32::try_from(frame_queue_size)
        .unwrap_or(u32::MAX)
        .saturating_mul(frame_interval_ms);
    queue_latency_ms.saturating_add(rtt_ms) > max_latency_ms
}

#[cfg(test)]
mod idr_decision_tests {
    use super::should_request_idr;

    #[test]
    fn rtt_spike_requests_idr() {
        // 5 queued frames @ 16ms/frame = 80ms queue latency; a 40ms rtt
        // spike pushes total estimated latency to 120ms, over the 100ms
        // budget -> request IDR.
        assert!(should_request_idr(5, 40, 16, 100));
    }

    #[test]
    fn healthy_queue_does_not_request_idr() {
        // 2 queued frames @ 16ms/frame = 32ms + a normal 10ms rtt = 42ms,
        // comfortably under the 100ms budget.
        assert!(!should_request_idr(2, 10, 16, 100));
    }

    #[test]
    fn zero_interval_never_requests_idr_from_queue_alone() {
        // frame_interval_ms == 0 means the queue term contributes nothing;
        // an in-budget rtt alone must not trip the decision.
        assert!(!should_request_idr(1000, 50, 0, 100));
    }
}
