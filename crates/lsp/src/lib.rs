#![doc = include_str!("../README.md")]
#![doc(
    html_logo_url = "https://raw.githubusercontent.com/paradigmxyz/solar/main/assets/logo.png",
    html_favicon_url = "https://raw.githubusercontent.com/paradigmxyz/solar/main/assets/favicon.ico"
)]
#![cfg_attr(docsrs, feature(doc_cfg))]

use crate::global_state::GlobalState;
use async_lsp::{
    ClientSocket, client_monitor::ClientProcessMonitorLayer, concurrency::ConcurrencyLayer,
    router::Router, server::LifecycleLayer, tracing::TracingLayer,
};
use lsp_types::{notification as notif, request as req};
use serde_json as _;
use solar_config::LspArgs;
use std::ops::ControlFlow;
use tower::ServiceBuilder;

mod config;
mod global_state;
mod handlers;
mod proto;
mod serde;
mod utils;
mod vfs;
mod watch;
mod workspace;

pub(crate) type NotifyResult = ControlFlow<async_lsp::Result<()>>;

fn new_router(client: ClientSocket) -> Router<GlobalState> {
    let this = GlobalState::new(client);
    let mut router = Router::new(this);

    // Lifecycle
    router
        .request::<req::Initialize, _>(GlobalState::on_initialize)
        .notification::<notif::Initialized>(GlobalState::on_initialized)
        .request::<req::Shutdown, _>(|_, _| std::future::ready(Ok(())))
        .notification::<notif::Exit>(|_, _| ControlFlow::Break(Ok(())));

    // Workspace management
    router
        .notification::<notif::DidChangeWorkspaceFolders>(handlers::did_change_workspace_folders)
        .notification::<notif::DidChangeWatchedFiles>(handlers::did_change_watched_files);

    // Notifications
    router
        .notification::<notif::DidOpenTextDocument>(handlers::did_open_text_document)
        .notification::<notif::DidCloseTextDocument>(handlers::did_close_text_document)
        .notification::<notif::DidChangeTextDocument>(handlers::did_change_text_document)
        .notification::<notif::DidChangeConfiguration>(handlers::did_change_configuration);

    router
}

/// Start the LSP server over stdin/stdout.
///
/// This future is long running and will not stop until the server exits.
pub async fn run_server_stdio(_args: LspArgs) -> async_lsp::Result<()> {
    // Prefer truly asynchronous piped stdin/stdout without blocking tasks.
    #[cfg(unix)]
    let (stdin, stdout) =
        (async_lsp::stdio::PipeStdin::lock_tokio()?, async_lsp::stdio::PipeStdout::lock_tokio()?);

    // Fallback to spawn blocking read/write otherwise.
    #[cfg(not(unix))]
    let (stdin, stdout) = (
        tokio_util::compat::TokioAsyncReadCompatExt::compat(tokio::io::stdin()),
        tokio_util::compat::TokioAsyncWriteCompatExt::compat_write(tokio::io::stdout()),
    );

    let (eloop, _) = async_lsp::MainLoop::new_server(|client| {
        ServiceBuilder::new()
            .layer(TracingLayer::default())
            .layer(LifecycleLayer::default())
            // TODO: infer concurrency
            .layer(ConcurrencyLayer::new(2.try_into().unwrap()))
            .layer(ClientProcessMonitorLayer::new(client.clone()))
            .service(new_router(client))
    });

    eloop.run_buffered(stdin, stdout).await
}

#[cfg(test)]
mod tests {
    use std::{ops::ControlFlow, time::Duration};

    use async_lsp::{AnyNotification, LspService};
    use lsp_types::{
        ClientCapabilities, DidChangeWatchedFilesClientCapabilities, DidChangeWatchedFilesParams,
        FileChangeType, FileEvent, InitializeParams, RegistrationParams,
        WorkspaceClientCapabilities, notification::Notification, request::Request,
    };
    use serde_json::{Value, json};
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

    use super::*;

    #[test]
    fn router_handles_watched_file_changes() {
        tokio::runtime::Builder::new_current_thread().build().unwrap().block_on(async {
            let mut router = new_router(ClientSocket::new_closed());
            let params = DidChangeWatchedFilesParams {
                changes: vec![FileEvent::new(
                    lsp_types::Url::parse("file:///workspace/src/Test.sol").unwrap(),
                    FileChangeType::CHANGED,
                )],
            };
            let notification: AnyNotification = serde_json::from_value(serde_json::json!({
                "method": notif::DidChangeWatchedFiles::METHOD,
                "params": params,
            }))
            .unwrap();

            assert!(matches!(router.notify(notification), ControlFlow::Continue(())));
        });
    }

    #[test]
    fn initialized_registers_watched_files_when_client_supports_dynamic_registration() {
        tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap().block_on(
            async {
                tokio::time::timeout(Duration::from_secs(5), async {
                    let (server_main, _) = async_lsp::MainLoop::new_server(new_router);
                    let (server_stream, client_stream) = tokio::io::duplex(64 * 1024);
                    let (server_read, server_write) = tokio::io::split(server_stream);
                    let server_task = tokio::spawn(async move {
                        server_main
                            .run_buffered(server_read.compat(), server_write.compat_write())
                            .await
                    });

                    let (client_read, mut client_write) = tokio::io::split(client_stream);
                    let mut client_read = BufReader::new(client_read);
                    let initialize_params = InitializeParams {
                        capabilities: ClientCapabilities {
                            workspace: Some(WorkspaceClientCapabilities {
                                did_change_watched_files: Some(
                                    DidChangeWatchedFilesClientCapabilities {
                                        dynamic_registration: Some(true),
                                        relative_pattern_support: None,
                                    },
                                ),
                                ..Default::default()
                            }),
                            ..Default::default()
                        },
                        ..Default::default()
                    };

                    write_lsp_message(
                        &mut client_write,
                        json!({
                            "jsonrpc": "2.0",
                            "id": 1,
                            "method": req::Initialize::METHOD,
                            "params": initialize_params,
                        }),
                    )
                    .await;
                    assert_eq!(read_lsp_message(&mut client_read).await["id"], 1);

                    write_lsp_message(
                        &mut client_write,
                        json!({
                            "jsonrpc": "2.0",
                            "method": notif::Initialized::METHOD,
                            "params": {},
                        }),
                    )
                    .await;

                    let register_request =
                        read_until_method(&mut client_read, "client/registerCapability").await;
                    let registration_id = register_request["id"].clone();
                    let params: RegistrationParams =
                        serde_json::from_value(register_request["params"].clone()).unwrap();
                    let registration = params.registrations.into_iter().next().unwrap();
                    assert_eq!(registration.method, notif::DidChangeWatchedFiles::METHOD);

                    write_lsp_message(
                        &mut client_write,
                        json!({
                            "jsonrpc": "2.0",
                            "id": registration_id,
                            "result": null,
                        }),
                    )
                    .await;
                    write_lsp_message(
                        &mut client_write,
                        json!({
                            "jsonrpc": "2.0",
                            "method": notif::Exit::METHOD,
                            "params": null,
                        }),
                    )
                    .await;

                    server_task.await.unwrap().unwrap();
                })
                .await
                .unwrap();
            },
        );
    }

    #[test]
    fn initialized_does_not_register_watched_files_without_client_support() {
        tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap().block_on(
            async {
                tokio::time::timeout(Duration::from_secs(5), async {
                    let (server_main, _) = async_lsp::MainLoop::new_server(new_router);
                    let (server_stream, client_stream) = tokio::io::duplex(64 * 1024);
                    let (server_read, server_write) = tokio::io::split(server_stream);
                    let server_task = tokio::spawn(async move {
                        server_main
                            .run_buffered(server_read.compat(), server_write.compat_write())
                            .await
                    });

                    let (client_read, mut client_write) = tokio::io::split(client_stream);
                    let mut client_read = BufReader::new(client_read);

                    write_lsp_message(
                        &mut client_write,
                        json!({
                            "jsonrpc": "2.0",
                            "id": 1,
                            "method": req::Initialize::METHOD,
                            "params": InitializeParams::default(),
                        }),
                    )
                    .await;
                    assert_eq!(read_lsp_message(&mut client_read).await["id"], 1);

                    write_lsp_message(
                        &mut client_write,
                        json!({
                            "jsonrpc": "2.0",
                            "method": notif::Initialized::METHOD,
                            "params": {},
                        }),
                    )
                    .await;

                    let message = read_lsp_message(&mut client_read).await;
                    assert_eq!(message["method"], notif::LogMessage::METHOD);

                    write_lsp_message(
                        &mut client_write,
                        json!({
                            "jsonrpc": "2.0",
                            "method": notif::Exit::METHOD,
                            "params": null,
                        }),
                    )
                    .await;

                    server_task.await.unwrap().unwrap();
                })
                .await
                .unwrap();
            },
        );
    }

    async fn write_lsp_message(writer: &mut (impl AsyncWriteExt + Unpin), message: Value) {
        let body = serde_json::to_vec(&message).unwrap();
        writer
            .write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes())
            .await
            .unwrap();
        writer.write_all(&body).await.unwrap();
        writer.flush().await.unwrap();
    }

    async fn read_until_method(reader: &mut (impl AsyncBufReadExt + Unpin), method: &str) -> Value {
        for _ in 0..4 {
            let message = read_lsp_message(reader).await;
            if message["method"] == method {
                return message;
            }
        }

        panic!("server did not send `{method}`");
    }

    async fn read_lsp_message(reader: &mut (impl AsyncBufReadExt + Unpin)) -> Value {
        let mut content_len = None;
        let mut line = String::new();
        loop {
            line.clear();
            reader.read_line(&mut line).await.unwrap();
            if line == "\r\n" {
                break;
            }

            if let Some(value) =
                line.strip_suffix("\r\n").and_then(|line| line.strip_prefix("Content-Length: "))
            {
                content_len = Some(value.parse::<usize>().unwrap());
            }
        }

        let mut body = vec![0; content_len.unwrap()];
        reader.read_exact(&mut body).await.unwrap();
        serde_json::from_slice(&body).unwrap()
    }
}
