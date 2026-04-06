//! Implements methods for testing [`Handshake`]

#![allow(clippy::unwrap_in_result)]

use super::*;

impl<S, C> Handshake<S, C>
where
    S: Service<Request, Response = Response, Error = BoxError> + Clone + Send + 'static,
    S::Future: Send,
    C: ChainTip + Clone + Send + 'static,
{
    /// Returns a count of how many connection nonces are stored in this [`Handshake`]
    pub async fn nonce_count(&self) -> usize {
        self.nonces.lock().await.len()
    }
}

#[test]
fn connected_addr_trace_label_reveals_socket_addr() {
    let connected_addr = ConnectedAddr::new_outbound_direct(
        "192.168.180.9:10000"
            .parse::<PeerSocketAddr>()
            .expect("valid peer socket address"),
    );

    assert_eq!(
        connected_addr.get_transient_addr_label(),
        "v4redacted:10000"
    );
    assert_eq!(
        connected_addr.get_transient_addr_label_for_tracing(),
        "192.168.180.9:10000"
    );
}

#[test]
fn isolated_connected_addr_trace_label_stays_isolated() {
    let connected_addr = ConnectedAddr::new_isolated();

    assert_eq!(connected_addr.get_transient_addr_label(), "isolated");
    assert_eq!(
        connected_addr.get_transient_addr_label_for_tracing(),
        "isolated"
    );
}
