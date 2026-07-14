//! transport-core — pure (no async, no I/O) transport codec/wire modules
//! shared between the streamer (host send path) and the M6 native client
//! cdylib (receive path). Extracted from streamer per m6-native-spike.md
//! Option-3 W1.

pub mod fec;
pub mod fec_wire;
pub mod video_rx;
