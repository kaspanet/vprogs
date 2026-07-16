//! Reproduction: the bridge worker `expect`/`unwrap`s fields carried on the RPC wire, so a peer
//! that elides one kills the worker thread with a panic. The worker runs on a bare `thread::spawn`
//! with no `catch_unwind`, so the panic is not converted into an [`L1Event::Fatal`]: the thread
//! dies and consumers parked in `wait_and_pop` block forever with no indication.
//!
//! Each test drives a real L1 node through a proxy that elides exactly one field from the
//! `GetVirtualChainFromBlockV2` response, then asserts the bridge surfaces `Fatal`. Every test
//! fails by timing out, which is the defect: the parked consumer is never woken.
//!
//! `L1BridgeConfig::default()` reaches the public community resolver, so the peer serving these
//! responses is untrusted and this is a remote liveness kill.

use std::{
    collections::HashSet,
    sync::{Arc, Mutex},
    time::Duration,
};

use borsh::BorshDeserialize;
use futures::{SinkExt, StreamExt};
use kaspa_consensus_core::network::NetworkId;
use kaspa_rpc_core::{GetVirtualChainFromBlockV2Response, api::ops::RpcApiOps};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::mpsc,
};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, tungstenite::Message};
use vprogs_core_types::{ChainSink, SchedulerTransaction};
use vprogs_l1_bridge::{Command, L1Bridge, L1BridgeConfig, L1Event};
use vprogs_l1_types::{ChainBlockMetadata, ConnectStrategy, L1Transaction, NetworkType};
use vprogs_node_test_utils::{L1BridgeExt, L1Node};
use vprogs_storage_canonical_chain::CanonicalChainManager;
use workflow_rpc::{
    id::Id64,
    messages::borsh::{BorshReqHeader, BorshServerMessage, BorshServerMessageHeader},
};
use workflow_serializer::prelude::Serializable;

/// Time allowed for the node to start, the bridge to connect, and blocks to reach the sink.
const TIMEOUT: Duration = Duration::from_secs(30);

/// Time a consumer waits for the `Fatal` the worker owes it after the panic. The worker reaches the
/// panic within milliseconds of the tampered response, so exceeding this means no event is coming.
const FATAL_TIMEOUT: Duration = Duration::from_secs(15);

// ============================================================================
// The tampered field, one per panic site
// ============================================================================

/// A single field elided from every `GetVirtualChainFromBlockV2` response, named for the worker
/// site that consumes it without checking. An honest node at `Full` verbosity always populates
/// these; a malicious one is free not to.
#[derive(Clone, Copy, Debug)]
enum Elide {
    /// `worker.rs:459` - `header.hash.expect("missing hash")`.
    HeaderHash,
    /// `worker.rs:460` - `header.daa_score.expect("missing daa_score")`.
    HeaderDaaScore,
    /// `worker.rs:470` - `L1Transaction::try_from(tx.clone()).expect("missing tx fields")`.
    TxMass,
    /// `worker.rs:505` - `ChainBlockMetadata::try_from(header).unwrap()`, which also requires
    /// `timestamp`. Reached only when the fields checked earlier are present.
    HeaderTimestamp,
    /// `worker.rs:526` - `header.blue_score.expect("missing blue_score")` in `lane_state`.
    HeaderBlueScore,
}

impl Elide {
    /// Removes this variant's field from every chain block in `response`.
    fn apply(self, response: &mut GetVirtualChainFromBlockV2Response) {
        for block in response.chain_block_accepted_transactions.iter_mut() {
            let header = &mut block.chain_block_header;
            match self {
                Elide::HeaderHash => header.hash = None,
                Elide::HeaderDaaScore => header.daa_score = None,
                Elide::HeaderTimestamp => header.timestamp = None,
                Elide::HeaderBlueScore => header.blue_score = None,
                Elide::TxMass => {
                    for tx in block.accepted_transactions.iter_mut() {
                        tx.mass = None;
                    }
                }
            }
        }
    }
}

// ============================================================================
// Tampering wRPC proxy
// ============================================================================

/// A wRPC proxy between the bridge and a real L1 node.
///
/// It forwards every frame verbatim except the responses to `GetVirtualChainFromBlockV2`, from
/// which it elides one field. Requests carry `BorshReqHeader { id, op }` and responses carry
/// `BorshServerMessageHeader { id, kind, op }`; the response header's `op` is not populated for
/// method replies, so the proxy tracks which request ids it must tamper by their op.
struct TamperProxy {
    /// URL the bridge connects to, standing in for the node's own wRPC URL.
    url: String,
}

impl TamperProxy {
    /// Binds a proxy in front of `upstream` and serves connections until the test ends.
    async fn start(upstream: String, elide: Elide) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("proxy failed to bind");
        let port = listener.local_addr().expect("proxy has no local address").port();

        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let upstream = upstream.clone();
                tokio::spawn(async move {
                    let downstream = tokio_tungstenite::accept_async(stream)
                        .await
                        .expect("proxy failed the client handshake");
                    let (node, _) = tokio_tungstenite::connect_async(&upstream)
                        .await
                        .expect("proxy failed to reach the node");
                    pump(downstream, node, elide).await;
                });
            }
        });

        Self { url: format!("ws://127.0.0.1:{port}") }
    }

    /// The proxy's wRPC URL.
    fn url(&self) -> String {
        self.url.clone()
    }
}

/// Relays frames both ways, eliding a field from every virtual-chain response.
async fn pump(
    downstream: WebSocketStream<TcpStream>,
    upstream: WebSocketStream<MaybeTlsStream<TcpStream>>,
    elide: Elide,
) {
    let (mut down_tx, mut down_rx) = downstream.split();
    let (mut up_tx, mut up_rx) = upstream.split();

    // Ids of in-flight virtual-chain requests, recorded on the way out and consumed on the way
    // back. Shared because the two directions are pumped concurrently.
    let pending: Arc<Mutex<HashSet<Id64>>> = Arc::new(Mutex::new(HashSet::new()));

    let to_node = {
        let pending = pending.clone();
        async move {
            while let Some(Ok(msg)) = down_rx.next().await {
                if let Message::Binary(bytes) = &msg {
                    record_virtual_chain_request(bytes, &pending);
                }
                if up_tx.send(msg).await.is_err() {
                    return;
                }
            }
        }
    };

    let to_client = async move {
        while let Some(Ok(msg)) = up_rx.next().await {
            let msg = match &msg {
                Message::Binary(bytes) => tamper(bytes, &pending, elide).unwrap_or(msg),
                _ => msg,
            };
            if down_tx.send(msg).await.is_err() {
                return;
            }
        }
    };

    // Either direction closing tears the connection down.
    futures::pin_mut!(to_node, to_client);
    futures::future::select(to_node, to_client).await;
}

/// Records `bytes`' request id if it is a virtual-chain request, so its reply can be tampered.
fn record_virtual_chain_request(bytes: &[u8], pending: &Mutex<HashSet<Id64>>) {
    let header = match BorshReqHeader::<RpcApiOps, Id64>::deserialize(&mut &bytes[..]) {
        Ok(header) => header,
        Err(_) => return,
    };
    if header.op != RpcApiOps::GetVirtualChainFromBlockV2 {
        return;
    }
    if let Some(id) = header.id {
        pending.lock().expect("pending set poisoned").insert(id);
    }
}

/// Rewrites `bytes` with the field elided, or `None` if the frame is not a virtual-chain response.
fn tamper(bytes: &[u8], pending: &Mutex<HashSet<Id64>>, elide: Elide) -> Option<Message> {
    let message = BorshServerMessage::<RpcApiOps, Id64>::try_from(bytes).ok()?;
    let id = message.header.id?;
    if !pending.lock().expect("pending set poisoned").remove(&id) {
        return None;
    }

    // A tracked id whose payload does not decode is an error reply, which is passed through.
    let mut response =
        Serializable::<GetVirtualChainFromBlockV2Response>::deserialize(&mut &message.payload[..])
            .ok()?
            .into_inner();
    elide.apply(&mut response);

    let payload = borsh::to_vec(&Serializable(response)).expect("response must reserialize");
    let header = BorshServerMessageHeader::new(message.header.id, message.header.kind, message.header.op);
    let rewritten =
        BorshServerMessage::new(header, &payload).try_to_vec().expect("frame must reserialize");
    Some(Message::Binary(rewritten))
}

// ============================================================================
// Sink
// ============================================================================

/// A [`ChainSink`] over a real [`CanonicalChainManager`], recording what the bridge schedules.
#[derive(Clone)]
struct RecordingSink(Arc<Mutex<SinkInner>>);

struct SinkInner {
    /// The canonical chain the bridge drives, providing the ids/metadata it reads back.
    manager: CanonicalChainManager<ChainBlockMetadata>,
    /// How many blocks the bridge has scheduled.
    scheduled: usize,
}

impl RecordingSink {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(SinkInner {
            manager: CanonicalChainManager::default(),
            scheduled: 0,
        })))
    }

    /// How many blocks the bridge has scheduled so far.
    fn scheduled(&self) -> usize {
        self.0.lock().expect("sink poisoned").scheduled
    }
}

impl ChainSink<ChainBlockMetadata, L1Transaction> for RecordingSink {
    fn append(
        &mut self,
        metadata: ChainBlockMetadata,
        _txs: Vec<SchedulerTransaction<L1Transaction>>,
    ) -> u64 {
        let mut inner = self.0.lock().expect("sink poisoned");
        let id = inner.manager.append(metadata).id;
        inner.scheduled += 1;
        id
    }

    fn rollback(&mut self, new_tip: u64) {
        self.0.lock().expect("sink poisoned").manager.rollback(new_tip);
    }

    fn finalize(&mut self, below: u64) {
        self.0.lock().expect("sink poisoned").manager.finalize(below);
    }

    fn tip(&self) -> u64 {
        self.0.lock().expect("sink poisoned").manager.chain().tip()
    }

    fn metadata(&self, id: u64) -> Option<ChainBlockMetadata> {
        self.0.lock().expect("sink poisoned").manager.metadata(id).copied()
    }

    fn id(&self, block_hash: &[u8; 32]) -> Option<u64> {
        self.0.lock().expect("sink poisoned").manager.id(block_hash)
    }

    fn shutdown(self) {}
}

// ============================================================================
// Harness
// ============================================================================

/// Keeps the API command channel's sender alive so the worker's command branch stays open.
type ApiGuard = mpsc::Sender<Command<RecordingSink>>;

/// Starts a node, a proxy eliding `elide`, and a connected bridge behind the proxy.
async fn setup(elide: Elide) -> (L1Node, L1Bridge, RecordingSink, ApiGuard) {
    let node = L1Node::new(NetworkId::new(NetworkType::Simnet), None).await;
    let proxy = TamperProxy::start(node.wrpc_borsh_url(), elide).await;

    let config = L1BridgeConfig::default()
        .with_url(Some(proxy.url()))
        .with_network_type(NetworkType::Simnet)
        .with_connect_strategy(ConnectStrategy::Fallback);

    let sink = RecordingSink::new();
    let (api_tx, api_rx) = mpsc::channel(1);
    let bridge = L1Bridge::new(config, sink.clone(), api_rx);

    // Drain `Connected` so a later `wait_and_pop` parks on the queue, as a consumer would.
    bridge.wait_for(TIMEOUT, |e| matches!(e, L1Event::Connected)).await;

    (node, bridge, sink, api_tx)
}

/// Mines a block whose tampered response reaches the worker, then asserts the bridge reports the
/// malformed response as `Fatal`.
///
/// The bridge owes every consumer either progress or a terminal event. Panicking on the worker
/// thread delivers neither.
async fn assert_fatal_on_elided_field(elide: Elide, site: &str) {
    let (node, bridge, sink, _api) = setup(elide).await;

    // Two blocks: Kaspa accepts a block's transactions on the next chain block, so the second
    // makes the first's txs (and header) land in a virtual-chain response the proxy tampers.
    node.mine_blocks(2).await;

    let event = tokio::time::timeout(FATAL_TIMEOUT, bridge.wait_and_pop()).await;
    assert!(
        matches!(event, Ok(L1Event::Fatal { .. })),
        "{site}: expected Fatal on the elided field, got {event:?} \
         (worker scheduled {} blocks before dying)",
        sink.scheduled(),
    );

    node.shutdown().await;
}

// ============================================================================
// One case per panic site
// ============================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "repro: worker.rs:459 panics on an elided header hash, killing the thread with no Fatal"]
async fn elided_header_hash_reports_fatal() {
    assert_fatal_on_elided_field(Elide::HeaderHash, "worker.rs:459").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "repro: worker.rs:460 panics on an elided daa_score, killing the thread with no Fatal"]
async fn elided_header_daa_score_reports_fatal() {
    assert_fatal_on_elided_field(Elide::HeaderDaaScore, "worker.rs:460").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "repro: worker.rs:470 panics on an elided tx field, killing the thread with no Fatal"]
async fn elided_transaction_field_reports_fatal() {
    assert_fatal_on_elided_field(Elide::TxMass, "worker.rs:470").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "repro: worker.rs:505 panics on an elided timestamp, killing the thread with no Fatal"]
async fn elided_header_timestamp_reports_fatal() {
    assert_fatal_on_elided_field(Elide::HeaderTimestamp, "worker.rs:505").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "repro: worker.rs:526 panics on an elided blue_score, killing the thread with no Fatal"]
async fn elided_header_blue_score_reports_fatal() {
    assert_fatal_on_elided_field(Elide::HeaderBlueScore, "worker.rs:526").await;
}

// `worker.rs:543` (`header.daa_score.expect("missing daa_score")` in `lane_state`) has no case of
// its own: it reads the same `Option` on the same header that `worker.rs:460` already unwrapped
// earlier in the loop, so any response that would trip it trips `:460` first. It is unreachable
// while `:460` stands, and `elided_header_daa_score_reports_fatal` covers the field.

// ============================================================================
// The consequence: a dead worker and a parked consumer
// ============================================================================

/// Verifies the worker survives a malformed response well enough to keep serving the chain, or
/// else reports that it cannot.
///
/// After the panic the thread is gone: no further block is ever scheduled and no event is ever
/// queued, so a consumer in `wait_and_pop` parks forever. Nothing observable distinguishes this
/// from a quiet chain.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "repro: the panicked worker thread dies silently, freezing the sink and parking consumers"]
async fn worker_survives_or_reports_a_malformed_response() {
    let (node, bridge, sink, _api) = setup(Elide::HeaderDaaScore).await;

    node.mine_blocks(2).await;

    // Give the worker time to reach the tampered response and die.
    tokio::time::sleep(Duration::from_secs(5)).await;
    let scheduled_at_death = sink.scheduled();

    // Keep mining: a live worker would schedule these.
    node.mine_blocks(5).await;
    tokio::time::sleep(Duration::from_secs(5)).await;

    assert!(
        sink.scheduled() > scheduled_at_death,
        "worker stopped scheduling after the malformed response (stuck at {scheduled_at_death} \
         blocks) and queued no event to say so: {:?}",
        bridge.drain(),
    );

    node.shutdown().await;
}
