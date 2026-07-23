//! Malformed-response handling: the RPC peer carries header and transaction fields as `Option`,
//! and `L1BridgeConfig::default()` reaches the public community resolver, so the peer serving them
//! is untrusted. A peer that elides a required field can never yield progress, so the bridge owes
//! its consumers a terminal [`L1Event::Fatal`]. A worker panic instead would kill the worker
//! thread silently, stop the sink, and park `wait_and_pop` consumers forever with no indication.
//!
//! Each test drives a real L1 node through a proxy that elides exactly one field from every reply
//! carrying it, then asserts the bridge reports the malformed response.

use std::{
    panic,
    sync::{
        Arc, Mutex, Once,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use borsh::BorshDeserialize;
use futures::{SinkExt, StreamExt};
use kaspa_consensus_core::network::NetworkId;
use kaspa_rpc_core::{GetBlockResponse, GetVirtualChainFromBlockV2Response, api::ops::RpcApiOps};
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
    error::ServerError,
    id::Id64,
    messages::borsh::{BorshServerMessage, BorshServerMessageHeader, ServerMessageKind},
};
use workflow_serializer::prelude::Serializable;

/// Time allowed for the node to start, the bridge to connect, and a tampered response to be served.
const TIMEOUT: Duration = Duration::from_secs(30);

/// Time a consumer waits for the event the worker owes it after the tampered response. The worker
/// reaches the elided field within milliseconds of the response, so exceeding this means none is
/// coming.
const FATAL_TIMEOUT: Duration = Duration::from_secs(15);

// ============================================================================
// Panic recording
// ============================================================================

/// Panic reports from every thread in this binary, including the bridge worker's. A recorded
/// worker panic is the diagnostic when an assert below fails: its source location names the code
/// that consumed the elided field instead of surfacing an error.
static PANICS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Installs a hook recording every panic report while keeping the default one. Idempotent.
fn record_panics() {
    static INSTALLED: Once = Once::new();
    INSTALLED.call_once(|| {
        let default = panic::take_hook();
        panic::set_hook(Box::new(move |info| {
            PANICS.lock().expect("panic log poisoned").push(info.to_string());
            default(info);
        }));
    });
}

/// The recorded panic reports raised under `l1/bridge/src`, where the worker consumes wire fields.
///
/// Matching the location keeps this binary's own panics, such as a sibling test's assert, out of
/// the result.
fn bridge_panics() -> Vec<String> {
    PANICS
        .lock()
        .expect("panic log poisoned")
        .iter()
        .filter(|p| p.starts_with("panicked at l1/bridge/src/"))
        .cloned()
        .collect()
}

// ============================================================================
// The tampered field, one per panic site
// ============================================================================

/// A single field elided from every reply carrying it. An honest node at `Full` verbosity always
/// populates these; a malicious one is free not to.
#[derive(Clone, Copy, Debug)]
enum Elide {
    /// A field of the virtual-chain reply, read while syncing.
    Chain(ChainField),
    /// A block's `verbose_data`, read while seeding the genesis anchor below the sink.
    BlockVerboseData,
}

/// A field of a `GetVirtualChainFromBlockV2` response the worker reads.
#[derive(Clone, Copy, Debug)]
enum ChainField {
    /// The chain-block header's `hash`.
    HeaderHash,
    /// The chain-block header's `daa_score`.
    HeaderDaaScore,
    /// An accepted transaction's `storage_mass`, required by the L1 transaction conversion.
    TxStorageMass,
    /// The chain-block header's `timestamp`.
    HeaderTimestamp,
    /// The chain-block header's `blue_score`.
    HeaderBlueScore,
}

impl Elide {
    /// The reply this field is elided from.
    fn op(self) -> RpcApiOps {
        match self {
            Elide::Chain(_) => RpcApiOps::GetVirtualChainFromBlockV2,
            Elide::BlockVerboseData => RpcApiOps::GetBlock,
        }
    }

    /// Rewrites `payload` without this field, returning the rewritten payload and how many fields
    /// it removed, or `None` if the payload is not the reply's success arm.
    fn apply(self, payload: &[u8]) -> Option<(Vec<u8>, usize)> {
        match self {
            Elide::Chain(field) => {
                let mut response =
                    VccReply::deserialize(&mut &payload[..]).ok()?.ok()?.into_inner();
                let elided = field.apply(&mut response);
                let payload = borsh::to_vec::<VccReply>(&Ok(Serializable(response)))
                    .expect("response must reserialize");
                Some((payload, elided))
            }
            Elide::BlockVerboseData => {
                let mut response =
                    BlockReply::deserialize(&mut &payload[..]).ok()?.ok()?.into_inner();
                let elided = response.block.verbose_data.take().is_some() as usize;
                let payload = borsh::to_vec::<BlockReply>(&Ok(Serializable(response)))
                    .expect("response must reserialize");
                Some((payload, elided))
            }
        }
    }
}

impl ChainField {
    /// Removes this field throughout `response`, returning how many were removed.
    fn apply(self, response: &mut GetVirtualChainFromBlockV2Response) -> usize {
        let mut elided = 0;
        for block in Arc::make_mut(&mut response.chain_block_accepted_transactions) {
            let header = &mut block.chain_block_header;
            elided += match self {
                ChainField::HeaderHash => header.hash.take().is_some() as usize,
                ChainField::HeaderDaaScore => header.daa_score.take().is_some() as usize,
                ChainField::HeaderTimestamp => header.timestamp.take().is_some() as usize,
                ChainField::HeaderBlueScore => header.blue_score.take().is_some() as usize,
                ChainField::TxStorageMass => block
                    .accepted_transactions
                    .iter_mut()
                    .map(|tx| tx.storage_mass.take().is_some() as usize)
                    .sum::<usize>(),
            };
        }
        elided
    }
}

// ============================================================================
// Tampering wRPC proxy
// ============================================================================

/// A wRPC proxy between the bridge and a real L1 node.
///
/// It forwards every frame verbatim except the successful replies carrying the tampered field, from
/// which it elides that one field, and counts what it removed so a test can establish that the
/// worker was actually served a malformed response.
struct TamperProxy {
    /// URL the bridge connects to, standing in for the node's own wRPC URL.
    url: String,
    /// Fields removed from responses so far.
    elided: Arc<AtomicUsize>,
}

impl TamperProxy {
    /// Binds a proxy in front of `upstream` and serves connections until the test ends.
    async fn start(upstream: String, elide: Elide) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("proxy failed to bind");
        let port = listener.local_addr().expect("proxy has no local address").port();
        let elided = Arc::new(AtomicUsize::new(0));

        let counter = elided.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let upstream = upstream.clone();
                let counter = counter.clone();
                tokio::spawn(async move {
                    let downstream = tokio_tungstenite::accept_async(stream)
                        .await
                        .expect("proxy failed the client handshake");
                    let (node, _) = tokio_tungstenite::connect_async(&upstream)
                        .await
                        .expect("proxy failed to reach the node");
                    pump(downstream, node, elide, counter).await;
                });
            }
        });

        Self { url: format!("ws://127.0.0.1:{port}"), elided }
    }

    /// The proxy's wRPC URL.
    fn url(&self) -> String {
        self.url.clone()
    }

    /// Waits until the proxy has removed a field from a response the node actually served.
    ///
    /// Every later assert is about how the worker handles that response, so reaching it without a
    /// single elision would make those asserts say nothing about the sites they name.
    async fn wait_for_elision(&self, timeout: Duration) {
        let start = Instant::now();
        while self.elided.load(Ordering::Relaxed) == 0 {
            assert!(
                start.elapsed() <= timeout,
                "the proxy elided no field, so no tampered response ever reached the worker and \
                 this test would prove nothing about the site it names",
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

/// Relays frames both ways, eliding a field from every response carrying it.
async fn pump(
    downstream: WebSocketStream<TcpStream>,
    upstream: WebSocketStream<MaybeTlsStream<TcpStream>>,
    elide: Elide,
    elided: Arc<AtomicUsize>,
) {
    let (mut down_tx, mut down_rx) = downstream.split();
    let (mut up_tx, mut up_rx) = upstream.split();

    let to_node = async move {
        while let Some(Ok(msg)) = down_rx.next().await {
            if up_tx.send(msg).await.is_err() {
                return;
            }
        }
    };

    let to_client = async move {
        while let Some(Ok(msg)) = up_rx.next().await {
            let msg = match &msg {
                Message::Binary(bytes) => tamper(bytes, elide, &elided).unwrap_or(msg),
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

/// A virtual-chain method reply's payload: the handler's `Result`, as the wRPC server frames it.
type VccReply = Result<Serializable<GetVirtualChainFromBlockV2Response>, ServerError>;

/// A `GetBlock` reply's payload, framed the same way.
type BlockReply = Result<Serializable<GetBlockResponse>, ServerError>;

/// Rewrites `bytes` with `elide`'s field removed, or `None` if the frame is not the successful
/// reply carrying it.
fn tamper(bytes: &[u8], elide: Elide, elided: &AtomicUsize) -> Option<Message> {
    let message = BorshServerMessage::<RpcApiOps, Id64>::try_from(bytes).ok()?;
    if !matches!(message.header.kind, ServerMessageKind::Success)
        || message.header.op != Some(elide.op())
    {
        return None;
    }

    // Only the `Ok` arm carries a response; an error reply is forwarded untouched.
    let (payload, count) = elide.apply(message.payload)?;
    elided.fetch_add(count, Ordering::Relaxed);

    let header =
        BorshServerMessageHeader::new(message.header.id, message.header.kind, message.header.op);
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

/// A node, a proxy eliding one field in front of it, and a bridge connected through the proxy.
struct Harness {
    node: L1Node,
    bridge: L1Bridge,
    sink: RecordingSink,
    proxy: TamperProxy,
    _api: ApiGuard,
}

/// Starts a node, a proxy eliding `elide`, and a bridge behind the proxy that anchors its fresh
/// chain as `seed_depth` says.
async fn spawn(elide: Elide, seed_depth: Option<u64>) -> Harness {
    record_panics();

    let node = L1Node::new(NetworkId::new(NetworkType::Simnet), None).await;
    let proxy = TamperProxy::start(node.wrpc_borsh_url(), elide).await;

    let config = L1BridgeConfig::default()
        .with_url(Some(proxy.url()))
        .with_network_type(NetworkType::Simnet)
        .with_connect_strategy(ConnectStrategy::Fallback)
        .with_seed_depth(seed_depth);

    let sink = RecordingSink::new();
    let (api_tx, api_rx) = mpsc::channel(1);
    let bridge = L1Bridge::new(config, sink.clone(), api_rx);

    Harness { node, bridge, sink, proxy, _api: api_tx }
}

/// Spawns a harness anchored at the pruning point and waits for it to connect, leaving the sync
/// path as the only place the elided field is read.
async fn setup(elide: Elide) -> Harness {
    let h = spawn(elide, None).await;

    // Drain `Connected` so a later `wait_and_pop` parks on the queue, as a consumer would.
    let events = h.bridge.wait_for(TIMEOUT, |e| matches!(e, L1Event::Connected)).await;
    assert_eq!(events.len(), 1, "the bridge queued {events:?} before connecting");

    h
}

/// Mines until a tampered response reaches the worker, then asserts the bridge reports the
/// malformed response as `Fatal`.
///
/// The bridge owes every consumer either progress or a terminal event. A peer that elides a
/// required field can never yield progress, so `Fatal` is the only outcome left. Panicking on the
/// worker thread delivers neither.
async fn assert_fatal_on_elided_field(elide: Elide) {
    let h = setup(elide).await;

    // Kaspa accepts a block's transactions on the next chain block, so the second block puts the
    // first's header and transactions into a virtual-chain response the proxy tampers.
    h.node.mine_blocks(2).await;
    h.proxy.wait_for_elision(TIMEOUT).await;

    let event = tokio::time::timeout(FATAL_TIMEOUT, h.bridge.wait_and_pop()).await;
    assert!(
        matches!(event, Ok(L1Event::Fatal { .. })),
        "{elide:?}: expected Fatal on the elided field, got {event:?}. The worker scheduled {} \
         blocks, and panicked at: {:?}",
        h.sink.scheduled(),
        bridge_panics(),
    );

    h.node.shutdown().await;
}

// ============================================================================
// One case per elidable field
// ============================================================================

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn elided_header_hash_reports_fatal() {
    assert_fatal_on_elided_field(Elide::Chain(ChainField::HeaderHash)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn elided_header_daa_score_reports_fatal() {
    assert_fatal_on_elided_field(Elide::Chain(ChainField::HeaderDaaScore)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn elided_transaction_storage_mass_reports_fatal() {
    assert_fatal_on_elided_field(Elide::Chain(ChainField::TxStorageMass)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn elided_header_timestamp_reports_fatal() {
    assert_fatal_on_elided_field(Elide::Chain(ChainField::HeaderTimestamp)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn elided_header_blue_score_reports_fatal() {
    assert_fatal_on_elided_field(Elide::Chain(ChainField::HeaderBlueScore)).await;
}

/// The walk in `seed_from_recent` reads `verbose_data` before the bridge announces `Connected`, so
/// a peer eliding it there kills the worker at startup, before any of the sync-path reads above
/// runs. The walk has no parent to follow and can never yield progress, so the consumer is still
/// owed `Fatal`, and this is the only case where nothing at all precedes it in the queue.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn elided_block_verbose_data_reports_fatal() {
    // One step below the sink is enough: the walk's first `get_block` reply carries the field.
    let h = spawn(Elide::BlockVerboseData, Some(1)).await;
    h.proxy.wait_for_elision(TIMEOUT).await;

    let event = tokio::time::timeout(FATAL_TIMEOUT, h.bridge.wait_and_pop()).await;
    assert!(
        matches!(event, Ok(L1Event::Fatal { .. })),
        "expected Fatal while seeding below the sink, got {event:?}. The worker panicked at: {:?}",
        bridge_panics(),
    );

    h.node.shutdown().await;
}

// ============================================================================
// Observability: progress or a terminal event, never silence
// ============================================================================

/// Verifies the bridge stays observable after a malformed response: it either keeps scheduling or
/// says why it stopped.
///
/// A worker that panicked instead would be gone: no further block scheduled, no event ever queued,
/// nothing observable to distinguish it from a quiet chain, and a consumer in `wait_and_pop`
/// parked forever.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn malformed_response_leaves_the_bridge_observable() {
    let h = setup(Elide::Chain(ChainField::HeaderDaaScore)).await;

    h.node.mine_blocks(2).await;
    h.proxy.wait_for_elision(TIMEOUT).await;

    // Let the worker reach the tampered response.
    tokio::time::sleep(Duration::from_secs(5)).await;
    let scheduled_before = h.sink.scheduled();

    // Keep mining: a live worker schedules these.
    h.node.mine_blocks(5).await;
    tokio::time::sleep(Duration::from_secs(5)).await;

    let progressed = h.sink.scheduled() > scheduled_before;
    let events = h.bridge.drain();
    assert!(
        progressed || !events.is_empty(),
        "after the malformed response the bridge neither scheduled a block (stuck at \
         {scheduled_before}) nor queued an event to say why; its worker panicked at: {:?}",
        bridge_panics(),
    );

    h.node.shutdown().await;
}
