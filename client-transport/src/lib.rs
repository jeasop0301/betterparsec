//! client-transport — the native (moonlight-qt fork) client's transport
//! sidecar cdylib (m6-native-spike.md Option-3, M6 W1).
//!
//! Receive path: `video_fec` DataChannel bytes → `transport_core::video_rx`
//! (FEC decode + reassembly) → [`frame_queue::FrameQueue`] → C ABI pull loop
//! ([`capi::ct_receiver_wait_frame`]) consumed by the FFmpeg decoder thread
//! in place of `LiWaitForNextVideoFrame`.
//!
//! The WebRTC/signaling client half of W1 (ICE + DataChannel subscribe)
//! attaches on top of [`capi::ct_receiver_on_message`]; the C header
//! contract lives in `include/client_transport.h`.

pub mod capi;
pub mod frame_queue;
