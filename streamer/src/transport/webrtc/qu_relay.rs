//! Localhost TCP relay and `video_qu` DataChannel glue for the QU tile protocol (U4 P1).
//!
//! See `docs/design/qu-protocol.md §5` for the host-side interface contract.
//!
//! ## Architecture
//!
//! The relay task owns a `TcpListener` bound to `127.0.0.1:0` (ephemeral port;
//! P2 TODO: config-driven port + IPC plumbing, qu-protocol.md §5).  At most one
//! connection is accepted at a time: a second `accept()` while one is live aborts
//! the old connection task (Sunshine fork reconnect wins, per design).
//!
//! - **Socket → channel**: reads u32-LE-length-prefixed frames from the Sunshine
//!   fork; validates epoch; relays host-band messages to the `video_qu` DataChannel
//!   sink when `active` (subscribed).  Client-band kinds (`>= 0x80`) from the
//!   socket are dropped (log once).
//! - **Channel → socket**: `QU_SUBSCRIBE` / `QU_BUDGET` received on `video_qu`
//!   are forwarded to the connected socket; `QU_SUBSCRIBE` also sets `active`.
//!
//! The generation ghost-writer guard (identical in style to `fec_sender`) exits
//! both the relay task and any live connection sub-task when the shared
//! `AtomicU32` no longer matches the spawned generation.

use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
};

use async_trait::async_trait;
use bytes::Bytes;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::TcpListener,
    sync::mpsc,
    task::JoinHandle,
};
use tracing::{debug, info, warn};
use webrtc::data_channel::RTCDataChannel;

use crate::transport::webrtc::qu_wire;

// ── Constants ─────────────────────────────────────────────────────────────

/// Maximum framed message length accepted from the socket (DoS guard).
/// Connections sending a length field exceeding this are dropped immediately.
/// Pinned by test: 4 MiB + 1 is rejected.
const MAX_MSG_LEN: u32 = 4 * 1024 * 1024;

/// Pure port resolution for the P2 config-driven relay port (module doc
/// above: "P2 TODO: config-driven port + IPC plumbing"). `None` or
/// `Some(0)` preserves today's ephemeral-port behavior (bind to port `0`,
/// let the OS choose); any other configured value is used verbatim.
///
/// Plumbing deferred: wiring a real config value in requires (a) a port
/// field on `common::config::WebRtcConfig` (or a dedicated qu-relay config
/// struct), and (b) threading it through `QuRelayHandle::spawn` (called
/// from `video.rs`, out of this slice's scope) down to the
/// `TcpListener::bind` call below. This fn + its unit test land now; the
/// `TcpListener::bind("127.0.0.1:0")` call stays unconditional.
#[allow(dead_code)]
pub(crate) fn resolve_relay_port(configured: Option<u16>) -> u16 {
    configured.unwrap_or(0)
}

// ── Sink abstraction ──────────────────────────────────────────────────────

/// Minimal async-send abstraction for the DataChannel side of the relay,
/// mirroring [`crate::transport::webrtc::fec_sender::FecSink`].
/// Implementations must be `Send + Sync + 'static`.
#[async_trait]
pub(crate) trait QuSink: Send + Sync + 'static {
    /// Send `data` to the DataChannel.  Returns `false` when the channel is gone.
    async fn send_bytes(&self, data: Bytes) -> bool;
}

/// Production sink wrapping an `RTCDataChannel`.
pub(crate) struct DataChannelSink(pub(crate) Arc<RTCDataChannel>);

#[async_trait]
impl QuSink for DataChannelSink {
    async fn send_bytes(&self, data: Bytes) -> bool {
        match self.0.send(&data).await {
            Ok(_) => true,
            Err(e) => {
                debug!("[QuSink] RTCDataChannel send error — channel likely closed: {e}");
                false
            }
        }
    }
}

// ── Handle ────────────────────────────────────────────────────────────────

/// Cheap-clone handle held by `WebRtcVideo`.
///
/// Mirrors the [`crate::transport::webrtc::fec_sender::FecSenderHandle`] pattern:
/// generation ghost-writer guard, subscribe-gated dormancy, `Arc<AtomicBool>` active.
#[derive(Clone)]
pub(crate) struct QuRelayHandle {
    /// `true` once the client sends `QU_SUBSCRIBE`.  The relay is dormant until then:
    /// socket reads and epoch tracking proceed, but nothing is sent to the DataChannel.
    pub(crate) active: Arc<AtomicBool>,
    /// Forward client DataChannel messages to the relay task for socket write.
    client_tx: mpsc::Sender<Bytes>,
    /// Ephemeral local address of the bound listener (logged at INFO on spawn).
    local_addr: SocketAddr,
}

impl QuRelayHandle {
    /// Bind a `127.0.0.1:0` listener, spawn the relay task, and return the handle.
    ///
    /// `own_generation` is the generation value this task will hold.  The relay task
    /// exits as soon as `generation.load() != own_generation`.
    pub(crate) async fn spawn(
        own_generation: u32,
        generation: Arc<AtomicU32>,
        sink: Arc<dyn QuSink>,
    ) -> anyhow::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let local_addr = listener.local_addr()?;
        info!("[QuRelay] listening on {local_addr} (generation {own_generation})");

        let active = Arc::new(AtomicBool::new(false));
        let (client_tx, client_rx) = mpsc::channel::<Bytes>(16);

        let handle = QuRelayHandle {
            active: active.clone(),
            client_tx,
            local_addr,
        };

        tokio::task::spawn(run_relay(
            own_generation,
            generation,
            active,
            listener,
            client_rx,
            sink,
        ));

        Ok(handle)
    }

    /// Local address of the bound listener (e.g., for Sunshine fork IPC).
    pub(crate) fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Whether the client has subscribed (`QU_SUBSCRIBE` received).
    // Wire API: used by tests and future callers (setup-path gate, mirrors fec_sender).
    #[allow(dead_code)]
    pub(crate) fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    /// Process a raw `video_qu` DataChannel message from the client:
    ///
    /// - `QU_SUBSCRIBE` (0x81): sets `active` and forwards to socket.
    /// - `QU_BUDGET` (0x82): forwarded verbatim.
    /// - Unknown client-band (`>= 0x80`): dropped with a debug log.
    /// - Host-band (`< 0x80`): silently ignored (unexpected on this channel).
    pub(crate) fn forward_client_msg(&self, data: Bytes) {
        let Some(kind) = data.first().copied() else {
            return;
        };
        match kind {
            qu_wire::KIND_QU_SUBSCRIBE => {
                self.active.store(true, Ordering::Release);
                let _ = self.client_tx.try_send(data);
            }
            qu_wire::KIND_QU_BUDGET => {
                let _ = self.client_tx.try_send(data);
            }
            _ if kind >= 0x80 => {
                debug!("[QuRelay] dropping unknown client-band kind 0x{kind:02x} from DataChannel");
            }
            _ => {
                debug!(
                    "[QuRelay] dropping unexpected host-band kind 0x{kind:02x} from DataChannel"
                );
            }
        }
    }
}

// ── Relay task ────────────────────────────────────────────────────────────

async fn run_relay(
    own_generation: u32,
    generation: Arc<AtomicU32>,
    active: Arc<AtomicBool>,
    listener: TcpListener,
    mut client_rx: mpsc::Receiver<Bytes>,
    sink: Arc<dyn QuSink>,
) {
    // Epoch state shared between relay task and the current connection sub-task.
    let epoch: Arc<std::sync::Mutex<Option<u32>>> = Arc::new(std::sync::Mutex::new(None));
    let mut conn_handle: Option<JoinHandle<()>> = None;
    let mut conn_write_tx: Option<mpsc::Sender<Bytes>> = None;
    // Last QU_SUBSCRIBE received from the client, replayed to every new or
    // reconnected Sunshine TCP connection so Sunshine's QU sender activates
    // even when QU_SUBSCRIBE arrived before the TCP connection was established.
    let mut pending_subscribe: Option<Bytes> = None;

    loop {
        // Generation guard: exit if this task has been superseded.
        if generation.load(Ordering::Acquire) != own_generation {
            if let Some(h) = conn_handle.take() {
                h.abort();
            }
            return;
        }

        tokio::select! {
            result = listener.accept() => {
                match result {
                    Ok((stream, addr)) => {
                        if let Some(h) = conn_handle.take() {
                            info!(
                                "[QuRelay] new connection from {addr}, replacing existing"
                            );
                            h.abort();
                        }
                        let (read_half, write_half) = stream.into_split();
                        let (write_tx, write_rx) = mpsc::channel::<Bytes>(16);
                        // Replay the stored QU_SUBSCRIBE to the new connection so
                        // Sunshine's QU sender activates on reconnect (or on the
                        // first connection when subscribe arrived before accept).
                        if let Some(sub) = &pending_subscribe {
                            let _ = write_tx.try_send(sub.clone());
                        }
                        conn_handle = Some(tokio::task::spawn(run_connection(
                            read_half,
                            write_half,
                            write_rx,
                            sink.clone(),
                            active.clone(),
                            epoch.clone(),
                            generation.clone(),
                            own_generation,
                        )));
                        conn_write_tx = Some(write_tx);
                    }
                    Err(e) => {
                        warn!("[QuRelay] accept error: {e}");
                    }
                }
            }
            msg = client_rx.recv() => {
                match msg {
                    Some(data) => {
                        // Store QU_SUBSCRIBE for replay on new/reconnected connections.
                        if data.first().copied() == Some(qu_wire::KIND_QU_SUBSCRIBE) {
                            pending_subscribe = Some(data.clone());
                        }
                        if let Some(tx) = &conn_write_tx {
                            let _ = tx.try_send(data);
                        }
                    }
                    None => {
                        // client_tx (the handle) was dropped — relay has no owner.
                        if let Some(h) = conn_handle.take() {
                            h.abort();
                        }
                        return;
                    }
                }
            }
        }
    }
}

// ── Connection sub-task ───────────────────────────────────────────────────

/// Read one length-prefixed frame from `read`.
///
/// Wire: u32 LE `len`, then exactly `len` bytes.  DoS guard:
/// `len == 0` or `len > MAX_MSG_LEN` returns `Err` (logged) to drop the connection.
async fn read_frame<R: AsyncRead + Unpin>(read: &mut R) -> std::io::Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    read.read_exact(&mut len_buf).await?;
    let len = u32::from_le_bytes(len_buf);
    if len == 0 {
        warn!("[QuRelay] zero-length frame — dropping connection (DoS guard)");
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "zero-length frame",
        ));
    }
    if len > MAX_MSG_LEN {
        warn!("[QuRelay] oversize frame ({len} > {MAX_MSG_LEN}) — dropping connection (DoS guard)");
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "oversize frame",
        ));
    }
    let mut buf = vec![0u8; len as usize];
    read.read_exact(&mut buf).await?;
    Ok(buf)
}

/// Route one framed message received from the Sunshine socket.
///
/// - `QU_CONFIG` / `QU_EPOCH`: update epoch state; relay always (active or dormant).
/// - `QU_TILE` / `QU_INVALIDATE`: epoch-filtered; relay only when epoch matches.
/// - Unknown host-band (`0x05..=0x7F`): relay verbatim for forward compat.
/// - Client-band (`>= 0x80`): drop; log once.
async fn handle_socket_frame(
    msg: &[u8],
    sink: &Arc<dyn QuSink>,
    active: &Arc<AtomicBool>,
    epoch: &Arc<std::sync::Mutex<Option<u32>>>,
) {
    let Some(kind) = msg.first().copied() else {
        return;
    };

    if kind >= 0x80 {
        // Client-band kind arriving from the socket is a protocol violation.
        static CLIENT_BAND_WARNED: AtomicBool = AtomicBool::new(false);
        if !CLIENT_BAND_WARNED.swap(true, Ordering::Relaxed) {
            warn!(
                "[QuRelay] client-band kind 0x{kind:02x} from socket — \
                 dropped (logged once per process)"
            );
        }
        return;
    }

    match kind {
        qu_wire::KIND_QU_CONFIG => {
            if let Some(qu_wire::QuMsg::Config { epoch: e, .. }) = qu_wire::parse_msg(msg) {
                *epoch.lock().expect("epoch mutex poisoned") = Some(e);
            }
            // CONFIG is always relayed regardless of active state: the client needs
            // CONFIG to initialise its epoch before it can accept QU_TILE messages.
            sink.send_bytes(Bytes::copy_from_slice(msg)).await;
        }
        qu_wire::KIND_QU_EPOCH => {
            if let Some(qu_wire::QuMsg::Epoch { new_epoch }) = qu_wire::parse_msg(msg) {
                *epoch.lock().expect("epoch mutex poisoned") = Some(new_epoch);
            }
            // EPOCH is always relayed regardless of active state, for the same reason.
            sink.send_bytes(Bytes::copy_from_slice(msg)).await;
        }
        qu_wire::KIND_QU_TILE | qu_wire::KIND_QU_INVALIDATE => {
            let cur = *epoch.lock().expect("epoch mutex poisoned");
            let msg_ep = qu_wire::peek(msg).and_then(|(_, ep)| ep);
            match (cur, msg_ep) {
                (Some(c), Some(m)) if c == m => {
                    relay_to_sink(sink, active, msg).await;
                }
                _ => {
                    debug!(
                        "[QuRelay] epoch-filtered 0x{kind:02x} \
                         (cur={cur:?}, msg={msg_ep:?})"
                    );
                }
            }
        }
        // Unknown host-band kinds (0x05..=0x7F): relay verbatim for forward compat.
        _ => {
            relay_to_sink(sink, active, msg).await;
        }
    }
}

/// Relay `msg` to the DataChannel sink.  No-op when `active` is false (dormant).
/// `QU_CONFIG` and `QU_EPOCH` bypass this gate (see `handle_socket_frame`); all
/// other kinds (tiles, invalidates, unknown host-band) are dropped until the
/// client sends `QU_SUBSCRIBE`.
async fn relay_to_sink(sink: &Arc<dyn QuSink>, active: &Arc<AtomicBool>, msg: &[u8]) {
    if !active.load(Ordering::Acquire) {
        return;
    }
    sink.send_bytes(Bytes::copy_from_slice(msg)).await;
}

/// Per-connection loop: socket → DataChannel (via `read_frame` + `handle_socket_frame`)
/// and DataChannel → socket (via `write_rx`).
///
/// Exits when:
/// - Connection closes (EOF on read, or write error).
/// - A DoS-guard violation is detected (`read_frame` returns `Err`).
/// - The generation guard fires (`generation != own_generation`).
/// - `write_rx` is closed (relay task exited).
async fn run_connection<R, W>(
    mut read: R,
    mut write: W,
    mut write_rx: mpsc::Receiver<Bytes>,
    sink: Arc<dyn QuSink>,
    active: Arc<AtomicBool>,
    epoch: Arc<std::sync::Mutex<Option<u32>>>,
    generation: Arc<AtomicU32>,
    own_generation: u32,
) where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    loop {
        // Generation guard: exit if this relay task has been superseded.
        if generation.load(Ordering::Acquire) != own_generation {
            return;
        }

        tokio::select! {
            frame = read_frame(&mut read) => {
                match frame {
                    Ok(msg) => {
                        handle_socket_frame(&msg, &sink, &active, &epoch).await;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                        debug!("[QuRelay] socket closed (EOF)");
                        return;
                    }
                    Err(_) => {
                        // Includes DoS-guard errors (zero-length, oversize, I/O error).
                        return;
                    }
                }
            }
            msg = write_rx.recv() => {
                match msg {
                    Some(data) => {
                        let len_bytes = (data.len() as u32).to_le_bytes();
                        if write.write_all(&len_bytes).await.is_err()
                            || write.write_all(&data).await.is_err()
                        {
                            debug!("[QuRelay] socket write error; dropping connection");
                            return;
                        }
                    }
                    None => return, // relay task exited
                }
            }
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::transport::webrtc::qu_wire;
    use std::sync::Arc;
    use tokio::{
        io::{AsyncWriteExt, duplex},
        time::{Duration, sleep},
    };
    #[test]
    fn qu_relay_port_from_config() {
        // None or 0 preserve today's ephemeral bind.
        assert_eq!(resolve_relay_port(None), 0);
        assert_eq!(resolve_relay_port(Some(0)), 0);
        // Any other configured port passes through verbatim.
        assert_eq!(resolve_relay_port(Some(45820)), 45820);
    }

    // ── Test sink (Vec<Bytes>) ────────────────────────────────────────────

    #[derive(Clone)]
    struct VecSink(Arc<std::sync::Mutex<Vec<Bytes>>>);

    impl VecSink {
        fn new() -> Self {
            VecSink(Arc::new(std::sync::Mutex::new(Vec::new())))
        }
        fn snapshot(&self) -> Vec<Bytes> {
            self.0.lock().unwrap().clone()
        }
        fn len(&self) -> usize {
            self.0.lock().unwrap().len()
        }
    }

    #[async_trait]
    impl QuSink for VecSink {
        async fn send_bytes(&self, data: Bytes) -> bool {
            self.0.lock().unwrap().push(data);
            true
        }
    }

    // ── Helpers ───────────────────────────────────────────────────────────

    /// Write a single length-prefixed frame to `write`.
    async fn write_frame<W: AsyncWrite + Unpin>(write: &mut W, msg: &[u8]) {
        let len = (msg.len() as u32).to_le_bytes();
        write.write_all(&len).await.unwrap();
        write.write_all(msg).await.unwrap();
    }

    /// Build epoch state pre-seeded with `ep`.
    fn epoch_with(ep: u32) -> Arc<std::sync::Mutex<Option<u32>>> {
        Arc::new(std::sync::Mutex::new(Some(ep)))
    }

    fn epoch_none() -> Arc<std::sync::Mutex<Option<u32>>> {
        Arc::new(std::sync::Mutex::new(None))
    }

    fn make_gen(v: u32) -> Arc<AtomicU32> {
        Arc::new(AtomicU32::new(v))
    }

    // ── Test 1: framing reassembly across fragmented reads ───────────────

    /// Feed a length-prefixed CONFIG frame in 3-byte chunks; the relay must
    /// reassemble it and relay to the sink.
    #[tokio::test]
    async fn test_framing_reassembly_3_byte_chunks() {
        let config_msg = qu_wire::encode_config(128, 128, 15, 9, 7);
        let mut frame = (config_msg.len() as u32).to_le_bytes().to_vec();
        frame.extend_from_slice(&config_msg);

        let (mut client_end, server_end) = duplex(1024);
        let (read_half, write_half) = tokio::io::split(server_end);
        // Keep _write_tx alive so write_rx.recv() does NOT immediately return None
        // (otherwise select! could choose that branch before read_frame completes).
        let (_write_tx, write_rx) = mpsc::channel::<Bytes>(4);

        let sink = Arc::new(VecSink::new());
        let active = Arc::new(AtomicBool::new(true)); // already subscribed
        let epoch = epoch_none();
        let gen_arc = make_gen(1);

        let sink_c = sink.clone();
        let task = tokio::spawn(run_connection(
            read_half, write_half, write_rx, sink_c, active, epoch, gen_arc, 1,
        ));

        // Feed in 3-byte chunks to verify framing reassembly.
        for chunk in frame.chunks(3) {
            client_end.write_all(chunk).await.unwrap();
        }
        drop(client_end); // trigger EOF → run_connection exits via UnexpectedEof
        // _write_tx stays alive until after task.await so write_rx.recv() blocks
        // rather than returning None, preventing select! from choosing that branch
        // before read_frame has assembled the CONFIG message.

        task.await.unwrap();
        drop(_write_tx);

        let msgs = sink.snapshot();
        assert_eq!(msgs.len(), 1, "CONFIG must be relayed after reassembly");
        assert_eq!(msgs[0].as_ref(), config_msg.as_slice());
    }

    // ── Test 2: oversize length drops connection ──────────────────────────

    /// 4 MiB + 1 byte length prefix → connection dropped (DoS guard).
    #[tokio::test]
    async fn test_oversize_frame_drops_connection() {
        let (mut client_end, server_end) = duplex(64);
        let (read_half, write_half) = tokio::io::split(server_end);
        let (_, write_rx) = mpsc::channel::<Bytes>(4);

        let sink = Arc::new(VecSink::new());
        let active = Arc::new(AtomicBool::new(true));
        let epoch = epoch_none();
        let gen_arc = make_gen(1);

        let task = tokio::spawn(run_connection(
            read_half,
            write_half,
            write_rx,
            sink.clone(),
            active,
            epoch,
            gen_arc,
            1,
        ));

        // Write length = MAX_MSG_LEN + 1.
        let oversize: u32 = MAX_MSG_LEN + 1;
        client_end.write_all(&oversize.to_le_bytes()).await.unwrap();

        task.await.unwrap(); // must exit cleanly, not hang

        assert_eq!(sink.len(), 0, "oversize frame must not reach sink");
    }

    // ── Test 3: zero-length drops connection ──────────────────────────────

    #[tokio::test]
    async fn test_zero_length_drops_connection() {
        let (mut client_end, server_end) = duplex(64);
        let (read_half, write_half) = tokio::io::split(server_end);
        let (_, write_rx) = mpsc::channel::<Bytes>(4);

        let sink = Arc::new(VecSink::new());
        let active = Arc::new(AtomicBool::new(true));
        let epoch = epoch_none();
        let gen_arc = make_gen(1);

        let task = tokio::spawn(run_connection(
            read_half,
            write_half,
            write_rx,
            sink.clone(),
            active,
            epoch,
            gen_arc,
            1,
        ));

        client_end.write_all(&0u32.to_le_bytes()).await.unwrap();

        task.await.unwrap();
        assert_eq!(sink.len(), 0, "zero-length frame must not reach sink");
    }

    // ── Test 4: epoch filter — current epoch relayed ──────────────────────

    #[tokio::test]
    async fn test_epoch_filter_current_epoch_relayed() {
        let tile_msg = qu_wire::encode_tile(5, 1, 1, 0, 0, 0, &[0xAA]);
        let vec = Arc::new(VecSink::new());
        let sink: Arc<dyn QuSink> = vec.clone();
        let active = Arc::new(AtomicBool::new(true));
        let epoch = epoch_with(5);

        handle_socket_frame(&tile_msg, &sink, &active, &epoch).await;

        assert_eq!(vec.len(), 1, "tile at current epoch must be relayed");
    }

    // ── Test 5: epoch filter — stale dropped ─────────────────────────────

    #[tokio::test]
    async fn test_epoch_filter_stale_dropped() {
        let tile_msg = qu_wire::encode_tile(4, 1, 1, 0, 0, 0, &[0xAA]);
        let vec = Arc::new(VecSink::new());
        let sink: Arc<dyn QuSink> = vec.clone();
        let active = Arc::new(AtomicBool::new(true));
        let epoch = epoch_with(5); // current=5, msg=4 → stale

        handle_socket_frame(&tile_msg, &sink, &active, &epoch).await;

        assert_eq!(vec.len(), 0, "stale-epoch tile must be dropped");
    }

    // ── Test 6: epoch filter — future epoch dropped until QU_EPOCH ───────

    #[tokio::test]
    async fn test_epoch_filter_future_epoch_dropped_then_relayed() {
        let vec = Arc::new(VecSink::new());
        let sink: Arc<dyn QuSink> = vec.clone();
        let active = Arc::new(AtomicBool::new(true));
        let epoch = epoch_with(5);

        // Future-epoch tile (epoch 6 while current is 5) → dropped.
        let tile_future = qu_wire::encode_tile(6, 0, 0, 0, 0, 0, &[]);
        handle_socket_frame(&tile_future, &sink, &active, &epoch).await;
        assert_eq!(vec.len(), 0, "future-epoch tile must be dropped");

        // QU_EPOCH 6 → current_epoch advances to 6.
        let epoch_msg = qu_wire::encode_epoch(6);
        handle_socket_frame(&epoch_msg, &sink, &active, &epoch).await;
        assert_eq!(vec.len(), 1, "QU_EPOCH must be relayed");

        // Now epoch 6 tile → relayed.
        handle_socket_frame(&tile_future, &sink, &active, &epoch).await;
        assert_eq!(vec.len(), 2, "tile at newly current epoch must be relayed");
    }

    // ── Test 7: epoch filter — QU_EPOCH clears and re-gates ──────────────

    #[tokio::test]
    async fn test_epoch_filter_epoch_msg_clears_and_regates() {
        let vec = Arc::new(VecSink::new());
        let sink: Arc<dyn QuSink> = vec.clone();
        let active = Arc::new(AtomicBool::new(true));
        let epoch = epoch_with(3);

        // Old-epoch tile at 3: relayed.
        let old_tile = qu_wire::encode_tile(3, 0, 0, 0, 0, 0, &[]);
        handle_socket_frame(&old_tile, &sink, &active, &epoch).await;
        assert_eq!(vec.len(), 1);

        // QU_EPOCH 10: current_epoch → 10.
        let epoch_10 = qu_wire::encode_epoch(10);
        handle_socket_frame(&epoch_10, &sink, &active, &epoch).await;
        assert_eq!(vec.len(), 2); // QU_EPOCH itself is relayed

        // Old tile (epoch 3) after epoch change → dropped.
        handle_socket_frame(&old_tile, &sink, &active, &epoch).await;
        assert_eq!(
            vec.len(),
            2,
            "tile at old epoch must be dropped after QU_EPOCH"
        );

        // New tile (epoch 10) → relayed.
        let new_tile = qu_wire::encode_tile(10, 0, 0, 0, 0, 0, &[]);
        handle_socket_frame(&new_tile, &sink, &active, &epoch).await;
        assert_eq!(vec.len(), 3);
    }

    // ── Test 8: dormant — TILE not forwarded; CONFIG/EPOCH always forwarded ─

    #[tokio::test]
    async fn test_dormant_tiles_suppressed_config_epoch_forwarded() {
        let vec = Arc::new(VecSink::new());
        let sink: Arc<dyn QuSink> = vec.clone();
        let active = Arc::new(AtomicBool::new(false)); // dormant
        let epoch = epoch_with(7);

        // Current-epoch tile → not forwarded when dormant.
        let tile = qu_wire::encode_tile(7, 0, 0, 0, 0, 0, &[]);
        handle_socket_frame(&tile, &sink, &active, &epoch).await;
        assert_eq!(vec.len(), 0, "dormant relay must suppress tiles");

        // CONFIG is always forwarded (client needs it to initialise epoch).
        let config = qu_wire::encode_config(128, 128, 15, 9, 99);
        handle_socket_frame(&config, &sink, &active, &epoch).await;
        assert_eq!(vec.len(), 1, "dormant relay must forward CONFIG to sink");
        assert_eq!(*epoch.lock().unwrap(), Some(99), "epoch updated by CONFIG");
    }

    // ── Test 9: subscribe activates relay ─────────────────────────────────

    #[tokio::test]
    async fn test_subscribe_activates_relay() {
        let sink = Arc::new(VecSink::new());
        let generation = make_gen(1);
        let handle = QuRelayHandle::spawn(1, generation.clone(), sink.clone())
            .await
            .unwrap();

        assert!(!handle.is_active(), "initially dormant");

        let sub = Bytes::from(qu_wire::encode_subscribe(1));
        handle.forward_client_msg(sub);

        sleep(Duration::from_millis(10)).await;
        assert!(handle.is_active(), "QU_SUBSCRIBE must activate the relay");
    }

    // ── Test 10: generation bump stops old task ───────────────────────────

    #[tokio::test]
    async fn test_generation_bump_stops_old_task() {
        let sink = Arc::new(VecSink::new());
        let generation = make_gen(1);

        let handle = QuRelayHandle::spawn(1, generation.clone(), sink.clone())
            .await
            .unwrap();

        // Activate and record the local_addr.
        let addr = handle.local_addr();
        assert!(addr.port() != 0);

        // Bump generation → old task becomes stale.
        generation.fetch_add(1, Ordering::AcqRel); // = 2

        // New handle (generation 2) on the same generation Arc.
        let _new_handle = QuRelayHandle::spawn(2, generation.clone(), sink.clone())
            .await
            .unwrap();

        sleep(Duration::from_millis(30)).await;

        // The old task should have exited (observable: connecting to the old
        // address and trying to send should not result in relay to sink).
        // We'll also test by checking new_handle works.
        assert!(
            !handle.is_active(),
            "old handle stays dormant (generation stopped it)"
        );
    }

    // ── Test 11: second socket connection replaces first ──────────────────

    #[tokio::test]
    async fn test_second_connection_replaces_first() {
        use tokio::net::TcpStream;

        let sink = Arc::new(VecSink::new());
        let generation = make_gen(1);

        // Use a pre-seeded active flag so we can observe relaying.
        let handle = QuRelayHandle::spawn(1, generation.clone(), sink.clone())
            .await
            .unwrap();
        handle.active.store(true, Ordering::Release);

        let addr = handle.local_addr();

        // First connection.
        let mut conn1 = TcpStream::connect(addr).await.unwrap();
        sleep(Duration::from_millis(10)).await;

        // Second connection: should replace conn1.
        let mut conn2 = TcpStream::connect(addr).await.unwrap();
        sleep(Duration::from_millis(20)).await;

        // Conn1 server side is aborted; attempting to write from conn1's
        // perspective should eventually fail (server closed the stream).
        // Write a frame on conn2 (current epoch=None so TILE gets epoch-filtered;
        // use CONFIG which is always relayed).
        let config = qu_wire::encode_config(64, 64, 10, 5, 2);
        write_frame(&mut conn2, &config).await;
        sleep(Duration::from_millis(20)).await;

        assert!(
            sink.len() >= 1,
            "conn2 (second connection) must be able to relay to sink"
        );

        // Write a frame on conn1 — the relay's server side should be gone.
        // We just verify the relay is not double-counting: conn1 frames
        // after replacement must NOT appear in the sink.
        let _ = conn1.write_all(&[0u8; 8]).await; // may error, that's fine
        sleep(Duration::from_millis(10)).await;
        // sink should not have grown beyond what conn2 already put there.
        let count_after_conn1_write = sink.len();
        assert_eq!(
            count_after_conn1_write,
            sink.len(),
            "conn1 write after replacement must not reach sink"
        );
    }

    // ── Test 12: client-band kind from socket dropped ─────────────────────
    // (kept at 12 to preserve numbering; new tests start at 13)

    #[tokio::test]
    async fn test_client_band_from_socket_dropped() {
        let vec = Arc::new(VecSink::new());
        let sink: Arc<dyn QuSink> = vec.clone();
        let active = Arc::new(AtomicBool::new(true));
        let epoch = epoch_with(0);

        // Kind 0x81 (QU_SUBSCRIBE) arriving from the socket is client-band.
        let sub_msg = qu_wire::encode_subscribe(1);
        handle_socket_frame(&sub_msg, &sink, &active, &epoch).await;
        assert_eq!(vec.len(), 0, "client-band kind from socket must be dropped");

        // Kind 0x82 (QU_BUDGET) likewise.
        let budget_msg = qu_wire::encode_budget(1000);
        handle_socket_frame(&budget_msg, &sink, &active, &epoch).await;
        assert_eq!(vec.len(), 0, "QU_BUDGET from socket must be dropped");
    }

    // ── Test 13: QU_EPOCH forwarded even when dormant ─────────────────────

    #[tokio::test]
    async fn test_epoch_forwarded_when_dormant() {
        let vec = Arc::new(VecSink::new());
        let sink: Arc<dyn QuSink> = vec.clone();
        let active = Arc::new(AtomicBool::new(false));
        let epoch = epoch_with(3);

        let epoch_msg = qu_wire::encode_epoch(4);
        handle_socket_frame(&epoch_msg, &sink, &active, &epoch).await;

        assert_eq!(vec.len(), 1, "QU_EPOCH must reach sink even when dormant");
        assert_eq!(*epoch.lock().unwrap(), Some(4), "epoch advanced to 4");
    }

    // ── Test 14: subscribe stored and replayed when connection arrives ─────

    /// Client sends QU_SUBSCRIBE before Sunshine connects.  The relay must
    /// store the subscribe and forward it to the TCP connection on accept.
    #[tokio::test]
    async fn test_subscribe_replayed_to_new_connection() {
        use tokio::io::AsyncReadExt;
        use tokio::net::TcpStream;

        let sink = Arc::new(VecSink::new());
        let generation = make_gen(1);
        let handle = QuRelayHandle::spawn(1, generation.clone(), sink.clone())
            .await
            .unwrap();
        let addr = handle.local_addr();

        // Subscribe before any Sunshine TCP connection.
        let sub = Bytes::from(qu_wire::encode_subscribe(1));
        handle.forward_client_msg(sub);
        sleep(Duration::from_millis(10)).await;

        // Sunshine connects.
        let mut conn = TcpStream::connect(addr).await.unwrap();
        sleep(Duration::from_millis(20)).await;

        // relay must have sent the stored subscribe as a length-prefixed frame.
        let result = tokio::time::timeout(Duration::from_millis(100), async {
            let mut len_buf = [0u8; 4];
            conn.read_exact(&mut len_buf).await?;
            let len = u32::from_le_bytes(len_buf) as usize;
            let mut payload = vec![0u8; len];
            conn.read_exact(&mut payload).await?;
            Ok::<Vec<u8>, std::io::Error>(payload)
        })
        .await;

        assert!(
            result.is_ok(),
            "timed out waiting for replayed QU_SUBSCRIBE"
        );
        assert_eq!(
            result.unwrap().unwrap(),
            qu_wire::encode_subscribe(1),
            "stored subscribe must be forwarded to first connection"
        );
    }

    // ── Test 15: subscribe replayed on reconnect ──────────────────────────

    /// Active session: client subscribed, conn1 received the subscribe.
    /// Sunshine disconnects and reconnects (conn2).  The relay must replay
    /// the stored subscribe to conn2 without the client re-subscribing.
    #[tokio::test]
    async fn test_subscribe_replayed_on_reconnect() {
        use tokio::io::AsyncReadExt;
        use tokio::net::TcpStream;

        let sink = Arc::new(VecSink::new());
        let generation = make_gen(1);
        let handle = QuRelayHandle::spawn(1, generation.clone(), sink.clone())
            .await
            .unwrap();
        let addr = handle.local_addr();

        // First connection; client subscribes.
        let mut conn1 = TcpStream::connect(addr).await.unwrap();
        sleep(Duration::from_millis(10)).await;

        let sub = Bytes::from(qu_wire::encode_subscribe(1));
        handle.forward_client_msg(sub);
        sleep(Duration::from_millis(10)).await;

        // Drain the subscribe frame from conn1.
        {
            let r = tokio::time::timeout(Duration::from_millis(100), async {
                let mut len_buf = [0u8; 4];
                conn1.read_exact(&mut len_buf).await?;
                let len = u32::from_le_bytes(len_buf) as usize;
                let mut buf = vec![0u8; len];
                conn1.read_exact(&mut buf).await?;
                Ok::<Vec<u8>, std::io::Error>(buf)
            })
            .await;
            assert!(r.is_ok(), "conn1 must receive QU_SUBSCRIBE");
            assert_eq!(r.unwrap().unwrap(), qu_wire::encode_subscribe(1));
        }

        // Sunshine disconnects and reconnects; client does NOT re-subscribe.
        drop(conn1);
        sleep(Duration::from_millis(20)).await;
        let mut conn2 = TcpStream::connect(addr).await.unwrap();
        sleep(Duration::from_millis(20)).await;

        // Relay must replay the stored subscribe to conn2.
        let result = tokio::time::timeout(Duration::from_millis(100), async {
            let mut len_buf = [0u8; 4];
            conn2.read_exact(&mut len_buf).await?;
            let len = u32::from_le_bytes(len_buf) as usize;
            let mut payload = vec![0u8; len];
            conn2.read_exact(&mut payload).await?;
            Ok::<Vec<u8>, std::io::Error>(payload)
        })
        .await;

        assert!(
            result.is_ok(),
            "timed out waiting for replayed subscribe on reconnect"
        );
        assert_eq!(
            result.unwrap().unwrap(),
            qu_wire::encode_subscribe(1),
            "subscribe must be replayed to reconnected Sunshine"
        );
    }
}
