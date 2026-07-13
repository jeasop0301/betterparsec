//! `ice_servers` — betterparsec feature #2 helper binary.
//!
//! Prints a JSON array of WebRTC ICE servers (a coturn TURN entry with a
//! short-lived credential) to stdout, in exactly the shape the streamer's
//! `ice_server_script` hook deserializes (`Vec<RtcIceServer>`:
//! `[{"urls":[...],"username":"...","credential":"..."}]`).
//!
//! Point `webrtc.ice_server_script` (or `WEBRTC_ICE_SERVER_SCRIPT`) at this
//! binary. It takes no arguments (the hook execs a bare path, no shell), and
//! self-sources its config from the environment it inherits:
//!   TURN_URLS    comma-separated, e.g. "turns:vps.example.com:443?transport=tcp"
//!   TURN_SECRET  coturn static-auth-secret
//!   TURN_TTL     credential lifetime in seconds (default 86400)
//!   TURN_NAME    username label embedded before the ':' (default "betterparsec")
//!
//! Using a real binary rather than a shell script avoids the Windows
//! ".cmd wrapper" pitfall of the bare-path exec contract. If TURN_URLS or
//! TURN_SECRET are missing/empty it prints `[]` and exits 0, so the base stack
//! simply falls back to its statically configured ICE servers.

use std::time::{SystemTime, UNIX_EPOCH};

use common::turn;
use serde::Serialize;

#[derive(Serialize)]
struct IceServer {
    urls: Vec<String>,
    username: String,
    credential: String,
}

fn main() {
    let (urls, secret) = match (std::env::var("TURN_URLS"), std::env::var("TURN_SECRET")) {
        (Ok(u), Ok(s)) if !u.trim().is_empty() && !s.is_empty() => (u, s),
        _ => {
            // No TURN configured → no dynamic servers; base falls back to static.
            println!("[]");
            return;
        }
    };

    let ttl = std::env::var("TURN_TTL")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(86_400);
    let name = std::env::var("TURN_NAME").unwrap_or_else(|_| "betterparsec".to_string());
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let creds = match turn::generate(&secret, &name, ttl, now) {
        Ok(creds) => creds,
        Err(err) => {
            eprintln!("ice_servers: failed to generate TURN credentials: {err}");
            std::process::exit(1);
        }
    };

    let servers = vec![IceServer {
        urls: urls
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        username: creds.username,
        credential: creds.credential,
    }];

    match serde_json::to_string(&servers) {
        Ok(json) => println!("{json}"),
        Err(err) => {
            eprintln!("ice_servers: failed to serialize ICE servers: {err}");
            std::process::exit(1);
        }
    }
}
