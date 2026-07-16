use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};

use actix_web::{
    Error, HttpRequest, HttpResponse, get, post, rt as actix_rt,
    web::{Data, Json, Payload},
};
use actix_ws::{Closed, Message, Session};
use common::{
    api_bindings::{
        LogMessageType, PostCancelRequest, PostCancelResponse, StreamClientMessage,
        StreamServerMessage,
    },
    ipc::{ServerIpcMessage, StreamerConfig, StreamerIpcMessage, create_child_ipc},
    serialize_json,
};
use log::{debug, error, info, warn};
use tokio::{spawn, time::sleep};

use tracing::{Level, instrument, span};

use crate::app::{
    App, AppError,
    host::{AppId, HostId},
    user::AuthenticatedUser,
};

#[get("/host/stream")]
#[instrument(
    name = "start_host",
    skip(web_app, user, payload, launcher, lifecycle),
    fields(user_id = %user.id())
)]
pub async fn start_host(
    web_app: Data<App>,
    mut user: AuthenticatedUser,
    request: HttpRequest,
    payload: Payload,
    launcher: Data<crate::LauncherHandle>,
    lifecycle: Data<Option<crate::LifecycleSink>>,
) -> Result<HttpResponse, Error> {
    let (response, mut session, mut stream) = actix_ws::handle(&request, payload)?;

    let client_unique_id = user.host_unique_id().await?;

    let permissions = user.role().await?.permissions().await?;

    let web_app = web_app.clone();
    actix_rt::spawn(async move {
        // -- Init and Configure
        let message;
        loop {
            message = match stream.recv().await {
                Some(Ok(Message::Text(text))) => text,
                Some(Ok(Message::Binary(_))) => {
                    return;
                }
                Some(Ok(_)) => continue,
                Some(Err(_)) => {
                    return;
                }
                None => {
                    return;
                }
            };
            break;
        }

        let message = match serde_json::from_str::<StreamClientMessage>(&message) {
            Ok(value) => value,
            Err(_) => {
                return;
            }
        };

        let StreamClientMessage::Init {
            host_id,
            app_id,
            video_frame_queue_size,
            audio_sample_queue_size,
        } = message
        else {
            let _ = session.close(None).await;

            warn!("WebSocket didn't send init as first message, closing it");
            return;
        };

        let host_id = HostId(host_id);
        let app_id = AppId(app_id);

        // -- Collect host data
        let mut host = match user.host(host_id).await {
            Ok(host) => host,
            Err(AppError::HostNotFound) => {
                let _ = send_ws_message(
                    &mut session,
                    StreamServerMessage::DebugLog {
                        message: "Failed to start stream because the host was not found"
                            .to_string(),
                        ty: Some(LogMessageType::FatalDescription),
                    },
                )
                .await;
                let _ = session.close(None).await;
                return;
            }
            Err(err) => {
                warn!("failed to start stream for host {host_id:?} (at host): {err}");

                let _ = send_ws_message(
                    &mut session,
                    StreamServerMessage::DebugLog {
                        message: "Failed to start stream because of a server error".to_string(),
                        ty: Some(LogMessageType::FatalDescription),
                    },
                )
                .await;
                let _ = session.close(None).await;
                return;
            }
        };

        let apps = match host.list_apps(&mut user).await {
            Ok(apps) => apps,
            Err(err) => {
                warn!("failed to start stream for host {host_id:?} (at list_apps): {err}");

                let _ = send_ws_message(
                    &mut session,
                    StreamServerMessage::DebugLog {
                        message: "Failed to start stream because of a server error".to_string(),
                        ty: Some(LogMessageType::FatalDescription),
                    },
                )
                .await;
                let _ = session.close(None).await;
                return;
            }
        };

        let Some(app) = apps.into_iter().find(|app| app.id == app_id) else {
            warn!("failed to start stream for host {host_id:?} because the app couldn't be found!");

            let _ = send_ws_message(
                &mut session,
                StreamServerMessage::DebugLog {
                    message: "Failed to start stream because the app was not found".to_string(),
                    ty: Some(LogMessageType::FatalDescription),
                },
            )
            .await;
            let _ = session.close(None).await;
            return;
        };

        let (address, http_port) = match host.address_port(&mut user).await {
            Ok(address_port) => address_port,
            Err(err) => {
                warn!("failed to start stream for host {host_id:?} (at get address_port): {err}");

                let _ = send_ws_message(
                    &mut session,
                    StreamServerMessage::DebugLog {
                        message: "Failed to start stream because of a server error".to_string(),
                        ty: Some(LogMessageType::FatalDescription),
                    },
                )
                .await;
                let _ = session.close(None).await;
                return;
            }
        };

        let pair_info = match host.pair_info(&mut user).await {
            Ok(pair_info) => pair_info,
            Err(err) => {
                warn!("failed to start stream for host {host_id:?} (at get pair_info): {err}");

                let _ = send_ws_message(
                    &mut session,
                    StreamServerMessage::DebugLog {
                        message: "Failed to start stream because the host is not paired"
                            .to_string(),
                        ty: Some(LogMessageType::FatalDescription),
                    },
                )
                .await;
                let _ = session.close(None).await;
                return;
            }
        };

        // -- Send App info
        let _ = send_ws_message(
            &mut session,
            StreamServerMessage::UpdateApp { app: app.into() },
        )
        .await;

        // -- Starting stage: launch streamer
        let _ = send_ws_message(
            &mut session,
            StreamServerMessage::DebugLog {
                message: "Launching streamer".to_string(),
                ty: None,
            },
        )
        .await;

        // Spawn child (via the injected G005 launcher — the standalone
        // binary's `crate::DefaultStreamerLauncher` behaves exactly like
        // the inline spawn this replaced).
        let streamer_path = resolve_streamer_path(&web_app.config().streamer_path);
        debug!(
            "[Stream]: launching streamer from {}",
            streamer_path.display()
        );
        let launched = match launcher.launch(&streamer_path).await {
            Ok(launched) => launched,
            Err(err) => {
                error!("[Stream]: failed to spawn streamer process: {err}");

                let _ = send_ws_message(
                    &mut session,
                    StreamServerMessage::DebugLog {
                        message: "Failed to start stream because of a server error".to_string(),
                        ty: Some(LogMessageType::FatalDescription),
                    },
                )
                .await;
                let _ = session.close(None).await;
                return;
            }
        };
        let streamer_pid = launched.pid;
        let mut child = launched.child;
        let stdin = launched.stdin;
        let stdout = launched.stdout;
        if let Some(sink) = lifecycle.as_ref() {
            sink(crate::StreamerLifecycleEvent::Spawned { pid: streamer_pid });
        }

        // Create ipc
        static CHILD_COUNTER: AtomicUsize = AtomicUsize::new(0);
        let id = CHILD_COUNTER.fetch_add(1, Ordering::Relaxed);
        let span = span!(Level::INFO, "ipc", child_id = id);

        let (mut ipc_sender, mut ipc_receiver) = create_child_ipc::<
            ServerIpcMessage,
            StreamerIpcMessage,
        >(span, stdin, stdout, child.stderr.take())
        .await;

        // Redirect ipc message into ws
        spawn({
            let mut ipc_sender = ipc_sender.clone();
            let lifecycle = lifecycle.clone();
            async move {
                let mut warned_closed = false;
                let mut terminal_error_code: Option<i32> = None;
                let mut graceful_stop = false;
                while let Some(message) = ipc_receiver.recv().await {
                    match message {
                        StreamerIpcMessage::WebSocket(message) => {
                            if let StreamServerMessage::ConnectionTerminated { error_code } =
                                &message
                            {
                                terminal_error_code = Some(*error_code);
                            }
                            if let Err(Closed) = send_ws_message(&mut session, message).await
                                && !warned_closed
                            {
                                warn!(
                                    "[Ipc]: Tried to send a ws message (text) but the socket is already closed"
                                );
                                ipc_sender.send(ServerIpcMessage::Stop).await;
                                warned_closed = true;
                            }
                        }
                        StreamerIpcMessage::WebSocketTransport(data) => {
                            if let Err(Closed) = session.binary(data).await
                                && !warned_closed
                            {
                                warn!(
                                    "[Ipc]: Tried to send a ws message (binary) but the socket is already closed"
                                );
                                ipc_sender.send(ServerIpcMessage::Stop).await;
                                warned_closed = true;
                            }
                        }
                        StreamerIpcMessage::Stop => {
                            debug!("[Ipc]: ipc receiver stopped by streamer");
                            graceful_stop = true;
                            break;
                        }
                    }
                }
                info!("[Ipc]: ipc receiver is closed");
                if let Some(error_code) = terminal_error_code {
                    info!("[Ipc]: session ended with terminal error_code={error_code}");
                }

                // The streamer sends an explicit Stop once it has already
                // shut down its own session (including, per G003, right
                // after it surfaces a typed ConnectionTerminated) — there is
                // nothing left to wait for, so close/kill immediately
                // instead of letting the terminal reason sit behind a blind
                // sleep. Only fall back to the grace period when the ipc
                // channel closed without an explicit Stop (crash / dropped
                // child), since that case has no shutdown signal to trust.
                if !graceful_stop {
                    sleep(Duration::from_secs(10)).await;
                }

                // close the websocket when the streamer crashed / disconnected / whatever
                if let Err(err) = session.close(None).await {
                    warn!("failed to close streamer web socket: {err}");
                }

                // kill the streamer
                if let Err(err) = child.kill().await {
                    warn!("failed to kill streamer child: {err}");
                }
                if let Some(sink) = lifecycle.as_ref() {
                    if let Some(error_code) = terminal_error_code {
                        sink(crate::StreamerLifecycleEvent::Terminated { error_code });
                    }
                    sink(crate::StreamerLifecycleEvent::Exited { pid: streamer_pid });
                }
            }
        });

        // Send init into ipc
        ipc_sender
            .send(ServerIpcMessage::Init {
                config: StreamerConfig {
                    webrtc: web_app.config().webrtc.clone(),
                    log_level: web_app.config().log.level_filter,
                },
                host_address: address,
                host_http_port: http_port,
                client_unique_id: Some(client_unique_id),
                client_private_key: pair_info.client_private_key,
                client_certificate: pair_info.client_certificate,
                server_certificate: pair_info.server_certificate,
                app_id: app_id.0,
                video_frame_queue_size,
                audio_sample_queue_size,
                permissions,
            })
            .await;

        // Redirect ws message into ipc
        while let Some(Ok(message)) = stream.recv().await {
            match message {
                Message::Text(text) => {
                    let Ok(message) = serde_json::from_str::<StreamClientMessage>(&text) else {
                        warn!("[Stream]: failed to deserialize from json");
                        return;
                    };

                    ipc_sender.send(ServerIpcMessage::WebSocket(message)).await;
                }
                Message::Binary(binary) => {
                    ipc_sender
                        .send(ServerIpcMessage::WebSocketTransport(binary))
                        .await;
                }
                _ => {}
            }
        }
    });

    Ok(response)
}

async fn send_ws_message(sender: &mut Session, message: StreamServerMessage) -> Result<(), Closed> {
    let Some(json) = serialize_json(&message) else {
        return Ok(());
    };

    sender.text(json).await
}

fn resolve_streamer_path(configured: &str) -> PathBuf {
    let configured_path = PathBuf::from(configured);
    if configured_path.is_file() {
        return configured_path;
    }

    let Some(candidate) = std::env::current_exe()
        .ok()
        .and_then(|current_exe| sibling_streamer_candidate(configured, &current_exe))
    else {
        return configured_path;
    };

    if candidate.is_file() {
        candidate
    } else {
        configured_path
    }
}

fn sibling_streamer_candidate(configured: &str, current_exe: &Path) -> Option<PathBuf> {
    let normalized = configured.replace('\\', "/");
    let normalized = normalized.strip_prefix("./").unwrap_or(&normalized);
    let default_name = format!("streamer{}", std::env::consts::EXE_SUFFIX);
    if normalized != "streamer" && normalized != default_name {
        return None;
    }

    current_exe.parent().map(|parent| parent.join(default_name))
}

#[cfg(test)]
mod streamer_path_tests {
    use super::sibling_streamer_candidate;
    use std::path::{Path, PathBuf};

    #[test]
    fn default_streamer_path_resolves_next_to_web_server() {
        let current_exe = Path::new("target")
            .join("debug")
            .join(format!("web-server{}", std::env::consts::EXE_SUFFIX));
        let expected = Path::new("target")
            .join("debug")
            .join(format!("streamer{}", std::env::consts::EXE_SUFFIX));

        assert_eq!(
            sibling_streamer_candidate("./streamer", &current_exe),
            Some(expected)
        );
    }

    #[test]
    fn windows_style_default_path_is_supported() {
        let current_exe = PathBuf::from("target")
            .join("debug")
            .join(format!("web-server{}", std::env::consts::EXE_SUFFIX));

        assert!(sibling_streamer_candidate(".\\streamer", &current_exe).is_some());
    }

    #[test]
    fn explicit_custom_path_is_never_rewritten() {
        let current_exe = Path::new("target")
            .join("debug")
            .join(format!("web-server{}", std::env::consts::EXE_SUFFIX));

        assert_eq!(
            sibling_streamer_candidate("C:/custom/streamer.exe", &current_exe),
            None
        );
    }
}

#[post("/host/cancel")]
pub async fn cancel_host(
    mut user: AuthenticatedUser,
    Json(request): Json<PostCancelRequest>,
) -> Result<Json<PostCancelResponse>, AppError> {
    let host_id = HostId(request.host_id);

    let mut host = user.host(host_id).await?;

    host.cancel_app(&mut user).await?;

    Ok(Json(PostCancelResponse { success: true }))
}
