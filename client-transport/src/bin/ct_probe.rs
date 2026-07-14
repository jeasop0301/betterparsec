//! ct-probe — headless end-to-end verification of the W1 receive path.
//!
//! Connects to a running BetterParsec web-server exactly like the browser
//! (login → WS signaling → WebRTC answer → video_fec subscribe) and reports
//! frames received through the shared RxCore. Success = frames flow without
//! any browser involved.
//!
//! Usage:
//!   ct-probe <base_url> <user> <pass> [host_id] [app_id] [seconds]
//!   ct-probe https://localhost:8080 admin secret 0 0 20
//!
//! TLS: accepts any certificate (probe is a dev tool; the shipping path is
//! the SHA-256 pin in client_transport::tls).

use std::sync::Arc;
use std::time::{Duration, Instant};

use client_transport::capi::RxCore;
use client_transport::flow::FlowConfig;
use client_transport::session::{Session, SessionConfig, SessionState};
use client_transport::tls::ServerTrust;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,webrtc=warn,webrtc_ice=warn,webrtc_sctp=warn".into()),
        )
        .init();

    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 {
        eprintln!("usage: ct-probe <base_url> <user> <pass> [host_id] [app_id] [seconds]");
        std::process::exit(2);
    }
    let base_url = args[1].trim_end_matches('/').to_string();
    let username = args[2].clone();
    let password = args[3].clone();
    let host_id: u32 = args.get(4).map_or(0, |s| s.parse().unwrap_or(0));
    let app_id: u32 = args.get(5).map_or(0, |s| s.parse().unwrap_or(0));
    let seconds: u64 = args.get(6).map_or(20, |s| s.parse().unwrap_or(20));

    let core = Arc::new(RxCore::new(0));
    let config = SessionConfig {
        base_url,
        username,
        password,
        trust: ServerTrust::InsecureAcceptAny,
        flow: FlowConfig {
            host_id,
            app_id,
            video_frame_queue_size: 3,
            audio_sample_queue_size: 20,
            bitrate_kbps: 8000,
            width: 1920,
            height: 1080,
            fps: 60,
            supported_codecs: client_transport::session::H264_BIT,
        },
    };

    let session = Session::start(config, core.clone());

    // Pull frames like the decoder thread would.
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let mut frames: u64 = 0;
    let mut bytes: u64 = 0;
    let mut key_frames: u64 = 0;
    let mut first_frame_at: Option<Duration> = None;
    let started = Instant::now();

    while Instant::now() < deadline {
        match core.wait_frame(Duration::from_millis(250)) {
            Some(unit) => {
                if frames == 0 {
                    first_frame_at = Some(started.elapsed());
                    println!(
                        "FIRST-FRAME frame_id={} key={} ts_us={} bytes={} after={:?}",
                        unit.frame_id,
                        unit.is_key,
                        unit.timestamp_us,
                        unit.data.len(),
                        started.elapsed(),
                    );
                }
                frames += 1;
                bytes += unit.data.len() as u64;
                if unit.is_key {
                    key_frames += 1;
                }
            }
            None => {
                let st = session.state();
                if st == SessionState::Failed || st == SessionState::Stopped {
                    eprintln!("session ended early: {st:?}");
                    break;
                }
            }
        }
    }

    let state = session.state();
    let params = session.stream_params();
    session.stop();

    println!("--- ct-probe report ---");
    println!("session_state:   {state:?}");
    println!("stream_params:   {params:?}");
    println!("frames:          {frames} ({key_frames} key)");
    println!("payload_bytes:   {bytes}");
    println!("first_frame_at:  {first_frame_at:?}");

    if frames > 0 && state == SessionState::Streaming {
        println!("CT-PROBE-OK");
    } else {
        println!("CT-PROBE-FAILED");
        std::process::exit(1);
    }
}
