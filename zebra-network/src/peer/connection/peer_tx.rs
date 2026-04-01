//! The peer message sender channel.

use futures::{FutureExt, Sink, SinkExt};

use zebra_chain::serialization::SerializationError;
use zebra_trace::{schema::PeerMessage, trace_event};

use crate::{constants::REQUEST_TIMEOUT, protocol::external::Message, PeerError};

/// A wrapper type for a peer connection message sender.
///
/// Used to apply a timeout to send messages.
#[derive(Clone, Debug)]
pub struct PeerTx<Tx>
where
    Tx: Sink<Message, Error = SerializationError> + Unpin,
{
    /// A channel for sending Zcash messages to the connected peer.
    ///
    /// This channel accepts [`Message`]s.
    inner: Tx,

    /// Structured JSONL tracer for outbound peer message events.
    tracer: zebra_trace::Tracer,

    /// The peer address label for trace events.
    peer_addr: String,
}

impl<Tx> PeerTx<Tx>
where
    Tx: Sink<Message, Error = SerializationError> + Unpin,
{
    /// Create a new `PeerTx` wrapping the given sink with a tracer and peer address.
    pub fn new(inner: Tx, tracer: zebra_trace::Tracer, peer_addr: String) -> Self {
        PeerTx {
            inner,
            tracer,
            peer_addr,
        }
    }

    /// Sends `msg` on `self.inner`, returning a timeout error if it takes too long.
    pub async fn send(&mut self, msg: Message) -> Result<(), PeerError> {
        let command = msg.command().to_string();
        let message_bytes = msg.wire_size();
        let block_hash = msg.single_block_hash().map(|hash| hash.to_string());
        let block_height = msg.block_height();
        let result = tokio::time::timeout(REQUEST_TIMEOUT, self.inner.send(msg))
            .await
            .map_err(|_| PeerError::ConnectionSendTimeout)?
            .map_err(Into::into);

        if result.is_ok() {
            trace_event!(
                self.tracer,
                PeerMessage {
                    direction: "out".to_string(),
                    command: command,
                    peer_addr: self.peer_addr.clone(),
                    message_bytes: message_bytes,
                    block_hash: block_hash,
                    block_height: block_height,
                }
            );
        }

        result
    }

    /// Flush any remaining output and close this [`PeerTx`], if necessary.
    pub async fn close(&mut self) -> Result<(), SerializationError> {
        self.inner.close().await
    }
}

impl<Tx> Drop for PeerTx<Tx>
where
    Tx: Sink<Message, Error = SerializationError> + Unpin,
{
    fn drop(&mut self) {
        // Do a last-ditch close attempt on the sink
        self.close().now_or_never();
    }
}
