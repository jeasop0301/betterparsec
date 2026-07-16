//! BetterParsec web-server as an embeddable library (design D3,
//! docs/design/unified-app-architecture.md): account, pairing, and
//! signaling actix services callable in-process by the unified app's
//! host role. The `web-server` binary (`src/main.rs`) stays a thin CLI
//! wrapper (config file + logging) around [`start`]; embedding hosts
//! call [`build`] to get a stoppable server bound to a concrete address.
//!
//! The client protocol (login → ws signaling → WebRTC) is unchanged by
//! embedding — web clients keep working against the embedded instance.

use std::net::SocketAddr;

use actix_web::{
    App as ActixApp, HttpServer,
    body::MessageBody,
    dev::{Server, ServiceRequest, ServiceResponse},
    http::header::HeaderMap,
    middleware::{self},
    web::{Data, scope},
};
use common::config::Config;
use openssl::ssl::{SslAcceptor, SslFiletype, SslMethod};
use tracing::{Level, Span, info, span};
use tracing_actix_web::{RootSpanBuilder, TracingLogger};

use crate::{
    api::{api_service, login_limiter::LoginLimiter},
    app::App,
    web::{web_config_js_service, web_service},
};

mod api;
mod app;
pub mod human_json;
mod web;

/// Read and parse a config file in the human-json format the CLI accepts.
/// `Ok(None)` when the file does not exist (caller decides the default).
pub fn load_config_file(path: &std::path::Path) -> Result<Option<Config>, anyhow::Error> {
    use anyhow::Context;
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let json = human_json::preprocess_human_json(text);
            let config = serde_json::from_str(&json)
                .with_context(|| format!("invalid config file {}", path.display()))?;
            Ok(Some(config))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(anyhow::Error::new(e).context(format!("read {}", path.display()))),
    }
}

pub fn ensure_rustls_crypto_provider() {
    // Idempotent and race-safe: install_default fails only when another
    // thread won the race, which leaves a provider installed either way.
    let _ = rustls::crypto::ring::default_provider().install_default();
}

struct ActixDebugSpan;

impl ActixDebugSpan {
    fn sanitize_headers(headers: &HeaderMap) -> Vec<(String, String)> {
        const SENSITIVE: &[&str] = &["authorization", "cookie", "set-cookie"];

        headers
            .iter()
            .map(|(name, value)| {
                let name_str = name.as_str().to_string();

                let value_str = if SENSITIVE.contains(&name_str.to_ascii_lowercase().as_str()) {
                    "<redacted>".to_string()
                } else {
                    value.to_str().unwrap_or("<binary>").to_string()
                };

                (name_str, value_str)
            })
            .collect()
    }
}

impl RootSpanBuilder for ActixDebugSpan {
    fn on_request_start(request: &ServiceRequest) -> Span {
        if tracing::enabled!(Level::TRACE) {
            span!(
                Level::TRACE,
                "http_request",
                method = %request.method(),
                uri = %request.uri(),
                headers = ?Self::sanitize_headers(request.headers()),
                peer_addr = ?request.peer_addr(),
            )
        } else {
            span!(
                Level::DEBUG,
                "http_request",
                method = %request.method(),
                uri = %request.uri(),
            )
        }
    }
    fn on_request_end<B: MessageBody>(
        _span: Span,
        _outcome: &Result<ServiceResponse<B>, actix_web::Error>,
    ) {
    }
}

/// A bound-but-not-yet-running server. `server` completes when stopped
/// (`server.handle().stop(..)`) — the embedder decides the runtime.
/// `addrs` carries the concrete bind addresses (port 0 resolved).
pub struct BoundServer {
    pub server: Server,
    pub addrs: Vec<SocketAddr>,
}

/// Construct app state and bind the HTTP(S) server without running it.
pub async fn build(config: Config) -> Result<BoundServer, anyhow::Error> {
    let app = App::new(config.clone()).await?;
    let app = Data::new(app);
    let limiter = Data::new(LoginLimiter::new());

    let bind_address = app.config().web_server.bind_address;
    let server = HttpServer::new({
        let url_path_prefix = config.web_server.url_path_prefix.clone();
        let app = app.clone();
        let limiter = limiter.clone();

        move || {
            ActixApp::new()
                .wrap(TracingLogger::<ActixDebugSpan>::new())
                .service(
                    scope(&url_path_prefix)
                        .app_data(app.clone())
                        .app_data(limiter.clone())
                        .wrap(
                            middleware::DefaultHeaders::new()
                                .add((
                                    "Cache-Control",
                                    "no-store, no-cache, must-revalidate, private",
                                ))
                                .add(("Pragma", "no-cache"))
                                .add(("Expires", "0")),
                        )
                        .service(api_service())
                        .service(web_config_js_service())
                        .service(web_service()),
                )
        }
    });

    let (server, addrs) = if let Some(certificate) = config.web_server.certificate.as_ref() {
        info!("[Server]: Running Https Server with ssl tls");

        let mut builder = SslAcceptor::mozilla_intermediate(SslMethod::tls())
            .expect("failed to create ssl tls acceptor");
        builder
            .set_private_key_file(&certificate.private_key_pem, SslFiletype::PEM)
            .expect("failed to set private key");
        builder
            .set_certificate_chain_file(&certificate.certificate_pem)
            .expect("failed to set certificate");

        let bound = server.bind_openssl(bind_address, builder)?;
        let addrs = bound.addrs();
        (bound.run(), addrs)
    } else {
        let bound = server.bind(bind_address)?;
        let addrs = bound.addrs();
        (bound.run(), addrs)
    };

    Ok(BoundServer { server, addrs })
}

/// Bind and run until the server is stopped (the binary's path).
pub async fn start(config: Config) -> Result<(), anyhow::Error> {
    build(config).await?.server.await?;
    Ok(())
}

// ── Embedded runner (unified app host role) ────────────────────────────────

/// A server running on its own thread with a dedicated actix `System`.
/// The embedder needs no actix/tokio of its own; [`EmbeddedServer::stop`]
/// (or `Drop`) shuts it down gracefully and joins the thread.
pub struct EmbeddedServer {
    addrs: Vec<SocketAddr>,
    handle: actix_web::dev::ServerHandle,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl EmbeddedServer {
    /// The concrete bind addresses (port 0 resolved).
    pub fn addrs(&self) -> &[SocketAddr] {
        &self.addrs
    }

    /// Graceful stop; joins the server thread.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        if let Some(t) = self.thread.take() {
            futures::executor::block_on(self.handle.stop(true));
            let _ = t.join();
        }
    }
}

impl Drop for EmbeddedServer {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Bind and run the server on a dedicated thread. Returns once the bind
/// completed (or failed) — the caller thread never touches actix.
pub fn spawn_embedded(config: Config) -> Result<EmbeddedServer, anyhow::Error> {
    ensure_rustls_crypto_provider();
    let (tx, rx) = std::sync::mpsc::channel();
    let thread = std::thread::Builder::new()
        .name("bp-web-server".into())
        .spawn(move || {
            actix_web::rt::System::new().block_on(async move {
                match build(config).await {
                    Ok(bound) => {
                        let handle = bound.server.handle();
                        let _ = tx.send(Ok((bound.addrs, handle)));
                        if let Err(e) = bound.server.await {
                            tracing::error!("embedded web-server exited with error: {e}");
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e));
                    }
                }
            });
        })?;
    match rx.recv() {
        Ok(Ok((addrs, handle))) => Ok(EmbeddedServer {
            addrs,
            handle,
            thread: Some(thread),
        }),
        Ok(Err(e)) => {
            let _ = thread.join();
            Err(e)
        }
        Err(_) => {
            let _ = thread.join();
            Err(anyhow::anyhow!(
                "embedded web-server thread died during bind"
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ensure_rustls_crypto_provider;
    use moonlight_common::http::client::{
        async_client::RequestClient, tokio_hyper::TokioHyperClient,
    };

    fn test_config(tag: &str) -> (common::config::Config, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "bp-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let mut config = common::config::Config::default();
        config.data_storage = common::config::StorageConfig::Json {
            path: dir.join("data.json").to_string_lossy().into_owned(),
            session_expiration_check_interval: std::time::Duration::from_secs(3600),
        };
        config.web_server.bind_address = "127.0.0.1:0".parse().expect("addr");
        config.web_server.certificate = None;
        (config, dir)
    }

    #[test]
    fn moonlight_https_client_can_be_constructed() {
        ensure_rustls_crypto_provider();
        TokioHyperClient::with_defaults()
            .expect("Moonlight HTTPS client construction must not panic");
    }

    /// Embedding contract (unified app host role): build binds an
    /// ephemeral port, the server answers HTTP, and the handle stops it
    /// gracefully — all inside a caller-provided actix runtime.
    #[actix_web::test]
    async fn embedded_server_boots_serves_and_stops() {
        ensure_rustls_crypto_provider();

        let (config, dir) = test_config("embed-test");

        let bound = super::build(config).await.expect("build embedded server");
        let addr = bound.addrs[0];
        assert_ne!(addr.port(), 0, "port 0 resolved to a concrete port");
        let handle = bound.server.handle();
        let join = actix_web::rt::spawn(bound.server);

        // Raw HTTP roundtrip — no client dependency.
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut sock = tokio::net::TcpStream::connect(addr).await.expect("connect");
        sock.write_all(
            b"GET /api/config.js HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        )
        .await
        .expect("send request");
        let mut response = Vec::new();
        sock.read_to_end(&mut response)
            .await
            .expect("read response");
        let head = String::from_utf8_lossy(&response);
        assert!(
            head.starts_with("HTTP/1.1 "),
            "embedded server answered HTTP, got: {}",
            &head[..head.len().min(80)]
        );

        handle.stop(true).await;
        join.await.expect("server task").expect("server exit");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The threaded embedder (unified app host role): no actix runtime on
    /// the caller side, sync HTTP answer, graceful stop joins the thread.
    #[test]
    fn spawn_embedded_serves_without_caller_runtime() {
        let (config, dir) = test_config("spawn-test");
        let server = super::spawn_embedded(config).expect("spawn embedded server");
        let addr = server.addrs()[0];
        assert_ne!(addr.port(), 0);

        use std::io::{Read, Write};
        let mut sock = std::net::TcpStream::connect(addr).expect("connect");
        sock.write_all(
            b"GET /api/config.js HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        )
        .expect("send request");
        let mut response = Vec::new();
        sock.read_to_end(&mut response).expect("read response");
        assert!(
            response.starts_with(b"HTTP/1.1 "),
            "embedded server answered HTTP"
        );

        server.stop(); // joins the bp-web-server thread
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Bind failures surface as errors instead of a wedged thread.
    #[test]
    fn spawn_embedded_reports_bind_conflict() {
        let (config, dir) = test_config("spawn-conflict");
        let first = super::spawn_embedded(config).expect("first bind");
        let (mut config2, dir2) = test_config("spawn-conflict2");
        config2.web_server.bind_address = first.addrs()[0];
        assert!(
            super::spawn_embedded(config2).is_err(),
            "second bind on the same port must fail cleanly"
        );
        first.stop();
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }
}
