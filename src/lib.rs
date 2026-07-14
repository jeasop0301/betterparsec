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
mod web;

pub fn ensure_rustls_crypto_provider() {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        rustls::crypto::ring::default_provider()
            .install_default()
            .expect("failed to install the rustls ring crypto provider");
    }
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


#[cfg(test)]
mod tests {
    use super::ensure_rustls_crypto_provider;
    use moonlight_common::http::client::{
        async_client::RequestClient, tokio_hyper::TokioHyperClient,
    };

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

        let dir = std::env::temp_dir().join(format!(
            "bp-embed-test-{}-{}",
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

        let bound = super::build(config).await.expect("build embedded server");
        let addr = bound.addrs[0];
        assert_ne!(addr.port(), 0, "port 0 resolved to a concrete port");
        let handle = bound.server.handle();
        let join = actix_web::rt::spawn(bound.server);

        // Raw HTTP roundtrip — no client dependency.
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut sock = tokio::net::TcpStream::connect(addr).await.expect("connect");
        sock.write_all(b"GET /api/config.js HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .expect("send request");
        let mut response = Vec::new();
        sock.read_to_end(&mut response).await.expect("read response");
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
}
