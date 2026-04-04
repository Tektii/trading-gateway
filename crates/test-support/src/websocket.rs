//! Mock WebSocket server for testing adapter WebSocket providers.
//!
//! Accepts a single client connection and bridges messages through channels,
//! allowing tests to send messages to the client and inspect received messages.

use std::net::SocketAddr;

use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;

/// A mock WebSocket server that accepts one client connection.
///
/// # Usage
///
/// ```ignore
/// let (server, tx, mut rx) = MockWsServer::start().await;
///
/// // Client connects to server.url()
/// // Send a message to the connected client:
/// tx.send("hello".into()).unwrap();
///
/// // Read a message sent by the client:
/// let msg = rx.recv().await.unwrap();
///
/// server.shutdown().await;
/// ```
pub struct MockWsServer {
    addr: SocketAddr,
    task: JoinHandle<()>,
}

impl MockWsServer {
    /// Start the mock server on a random available port.
    ///
    /// Returns:
    /// - The server handle (for URL access and shutdown)
    /// - A sender to push messages to the connected client
    /// - A receiver to read messages sent by the client
    pub async fn start() -> (
        Self,
        mpsc::UnboundedSender<String>,
        mpsc::UnboundedReceiver<String>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("failed to bind mock WS server");
        let addr = listener.local_addr().expect("failed to get local addr");

        let (outgoing_tx, mut outgoing_rx) = mpsc::unbounded_channel::<String>();
        let (incoming_tx, incoming_rx) = mpsc::unbounded_channel::<String>();

        let task = tokio::spawn(async move {
            // Accept one connection.
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };

            let Ok(ws_stream) = tokio_tungstenite::accept_async(stream).await else {
                tracing::warn!("mock WS server: failed to accept WebSocket upgrade");
                return;
            };

            let (mut sink, mut stream) = ws_stream.split();

            // Bridge: outgoing channel → WS sink, WS stream → incoming channel.
            let write_task = tokio::spawn(async move {
                while let Some(msg) = outgoing_rx.recv().await {
                    if sink.send(Message::Text(msg.into())).await.is_err() {
                        break;
                    }
                }
            });

            let read_task = tokio::spawn(async move {
                while let Some(Ok(msg)) = stream.next().await {
                    if let Message::Text(text) = msg {
                        let _ = incoming_tx.send(text.to_string());
                    }
                }
            });

            // Wait for either side to finish.
            tokio::select! {
                () = async { write_task.await.ok(); } => {},
                () = async { read_task.await.ok(); } => {},
            }
        });

        let server = Self { addr, task };
        (server, outgoing_tx, incoming_rx)
    }

    /// WebSocket URL for clients to connect to (e.g., `ws://127.0.0.1:12345`).
    #[must_use]
    pub fn url(&self) -> String {
        format!("ws://{}", self.addr)
    }

    /// Shut down the server and wait for the accept task to finish.
    pub async fn shutdown(self) {
        self.task.abort();
        let _ = self.task.await;
    }
}
