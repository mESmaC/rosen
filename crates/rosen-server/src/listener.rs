//! The accept loop: one task per connected client.

use crate::{connection, ServerState};
use rosen_protocol::Listener;
use std::sync::Arc;
use std::time::Duration;

/// Accepts clients until shutdown is requested.
pub async fn serve(mut listener: Listener, state: Arc<ServerState>) {
    loop {
        let accepted = tokio::select! {
            accepted = listener.accept() => accepted,
            _ = state.shutdown_requested() => return,
        };
        match accepted {
            Ok(stream) => {
                tokio::spawn(connection::serve_connection(stream, Arc::clone(&state)));
            }
            Err(err) => {
                // One failed accept (a client that vanished mid-connect) must not
                // stop the daemon; back off briefly so a persistent error cannot spin.
                eprintln!("rosen-server: accept failed: {err}");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
}
