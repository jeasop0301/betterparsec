//! client-transport — BetterParsec native client transport engine
//! (m6-native-spike.md Option-3, M6 W1).
//!
//! Product direction (owner, 2026-07-14): the end state is a unified
//! bidirectional Sunshine+Moonlight app with a fully custom UI. This crate
//! is deliberately UI-agnostic: everything here survives the shell swap
//! (moonlight-qt fork today, our own native shell later).
//!
//! Receive path: signaling ([`session`] + [`flow`]) → WebRTC peer →
//! `video_fec` DataChannel → `transport_core::video_rx` (FEC decode +
//! reassembly) → [`frame_queue::FrameQueue`] → C ABI pull loop
//! ([`capi::ct_receiver_wait_frame`]) consumed by the decoder thread in
//! place of `LiWaitForNextVideoFrame`.

pub mod capi;
pub mod cursor;
pub mod flow;
pub mod frame_queue;
pub mod session;
pub mod tls;
pub mod watchdog;
