use std::{
    sync::Arc,
    thread::{self, JoinHandle},
    time::Duration,
};

use futures::{FutureExt, select_biased};
use kaspa_notify::scope::{
    BlockAddedScope, FinalityConflictResolvedScope, FinalityConflictScope, Scope,
    VirtualChainChangedScope, VirtualDaaScoreChangedScope,
};
use kaspa_rpc_core::{Notification, api::ctl::RpcState};
use kaspa_wrpc_client::prelude::*;
use tokio::{runtime::Builder, sync::Notify};
use workflow_core::channel::Channel;

use crate::{
    BridgeConfig, ConnectionState, EventQueue, L1BridgeError, L1Event, Result,
    event::{BlockAdded, DaaScoreChanged, FinalityConflict, FinalityResolved, VirtualChainChanged},
};

/// The main L1 bridge that manages connection to Kaspa and emits events.
pub struct L1Bridge {
    /// Configuration for the bridge.
    config: BridgeConfig,
    /// Lock-free event queue for consumers.
    event_queue: EventQueue,
    /// Connection state tracking.
    state: Arc<ConnectionState>,
    /// Notification signal to shut down the worker.
    notify_shutdown: Arc<Notify>,
    /// Handle to the background worker thread.
    handle: Option<JoinHandle<()>>,
}

impl L1Bridge {
    /// Creates a new L1 bridge with the given configuration.
    ///
    /// The bridge does not connect automatically; call `start()` to initiate connection.
    pub fn new(config: BridgeConfig) -> Self {
        Self {
            config,
            event_queue: EventQueue::new(),
            state: Arc::new(ConnectionState::new()),
            notify_shutdown: Arc::new(Notify::new()),
            handle: None,
        }
    }

    /// Starts the bridge, initiating connection to the L1 node.
    ///
    /// This spawns a background worker thread that manages the RPC connection
    /// and forwards notifications to the event queue.
    pub fn start(&mut self) -> Result<()> {
        if self.handle.is_some() {
            return Err(L1BridgeError::AlreadyStarted);
        }

        let config = self.config.clone();
        let event_queue = self.event_queue.clone();
        let state = self.state.clone();
        let notify_shutdown = self.notify_shutdown.clone();

        let handle = thread::spawn(move || {
            Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to build tokio runtime")
                .block_on(run_event_loop(config, event_queue, state, notify_shutdown))
        });

        self.handle = Some(handle);
        Ok(())
    }

    /// Stops the bridge and disconnects from the L1 node.
    ///
    /// This signals the background worker to shut down and waits for it to complete.
    pub fn stop(&mut self) -> Result<()> {
        self.notify_shutdown.notify_one();
        if let Some(handle) = self.handle.take() {
            handle.join().expect("bridge worker panicked");
        }
        Ok(())
    }

    /// Returns the event queue for consuming L1 events.
    ///
    /// The queue is lock-free. Use `pop()` to poll for events, or `wait_and_pop()`
    /// to block until an event is available.
    pub fn event_queue(&self) -> &EventQueue {
        &self.event_queue
    }

    /// Pops an event from the queue, if available.
    pub fn pop_event(&self) -> Option<L1Event> {
        self.event_queue.pop()
    }

    /// Drains all available events from the queue.
    pub fn drain_events(&self) -> Vec<L1Event> {
        self.event_queue.drain()
    }

    /// Returns whether the bridge is currently connected.
    pub fn is_connected(&self) -> bool {
        self.state.is_connected()
    }

    /// Returns the last known DAA score.
    pub fn last_daa_score(&self) -> u64 {
        self.state.last_daa_score()
    }

    /// Returns a reference to the connection state.
    pub fn state(&self) -> &Arc<ConnectionState> {
        &self.state
    }

    /// Returns whether the bridge worker is running.
    pub fn is_running(&self) -> bool {
        self.handle.is_some()
    }
}

impl Drop for L1Bridge {
    fn drop(&mut self) {
        // Signal shutdown and wait for the worker to complete.
        self.notify_shutdown.notify_one();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Runs the main event loop for the bridge worker.
async fn run_event_loop(
    config: BridgeConfig,
    event_queue: EventQueue,
    state: Arc<ConnectionState>,
    notify_shutdown: Arc<Notify>,
) {
    // Create the RPC client.
    let resolver = if config.url.is_none() { Some(Resolver::default()) } else { None };
    let client = match KaspaRpcClient::new_with_args(
        WrpcEncoding::Borsh,
        config.url.as_deref(),
        resolver,
        Some(config.network_id),
        None,
    ) {
        Ok(client) => Arc::new(client),
        Err(e) => {
            log::error!("Failed to create RPC client: {}", e);
            return;
        }
    };

    // Create notification channel for receiving node notifications.
    let notification_channel: Channel<Notification> = Channel::unbounded();

    // Get RPC control channel for connection events.
    let rpc_ctl_channel = client.rpc_ctl().multiplexer().channel();

    // Connect with configured options.
    let connect_options = ConnectOptions {
        block_async_connect: config.block_async_connect,
        connect_timeout: Some(Duration::from_millis(config.connect_timeout_ms)),
        strategy: config.connect_strategy,
        ..Default::default()
    };

    if let Err(e) = client.connect(Some(connect_options)).await {
        log::error!("Failed to initiate connection: {}", e);
        return;
    }

    let mut listener_id: Option<ListenerId> = None;

    loop {
        select_biased! {
            // Handle shutdown signal (lowest priority to drain other channels first).
            _ = notify_shutdown.notified().fuse() => {
                log::info!("L1 bridge shutdown requested");
                break;
            }

            // Handle RPC connection state changes.
            msg = rpc_ctl_channel.receiver.recv().fuse() => {
                match msg {
                    Ok(RpcState::Connected) => {
                        log::info!("L1 bridge connected to {}", client.url().unwrap_or_default());
                        state.set_connected(true);
                        state.increment_reconnect_count();

                        // Register notification listener.
                        let id = client.rpc_api().register_new_listener(
                            ChannelConnection::new(
                                "vprogs-l1-bridge",
                                notification_channel.sender.clone(),
                                ChannelType::Persistent,
                            )
                        );

                        // Subscribe to relevant scopes.
                        let scopes: Vec<Scope> = vec![
                            Scope::BlockAdded(BlockAddedScope {}),
                            Scope::VirtualChainChanged(VirtualChainChangedScope {
                                include_accepted_transaction_ids: config.include_accepted_transaction_ids,
                            }),
                            Scope::FinalityConflict(FinalityConflictScope {}),
                            Scope::FinalityConflictResolved(FinalityConflictResolvedScope {}),
                            Scope::VirtualDaaScoreChanged(VirtualDaaScoreChangedScope {}),
                        ];

                        for scope in scopes {
                            if let Err(e) = client.rpc_api().start_notify(id, scope).await {
                                log::error!("Failed to subscribe to scope: {}", e);
                            }
                        }

                        listener_id = Some(id);

                        // Emit connected event.
                        event_queue.push(L1Event::Connected);
                    }
                    Ok(RpcState::Disconnected) => {
                        log::info!("L1 bridge disconnected");
                        state.set_connected(false);

                        // Unregister listener.
                        if let Some(id) = listener_id.take() {
                            let _ = client.rpc_api().unregister_listener(id).await;
                        }

                        // Emit disconnected event.
                        event_queue.push(L1Event::Disconnected);
                    }
                    Err(e) => {
                        log::error!("RPC control channel error: {}", e);
                        break;
                    }
                }
            }

            // Handle notifications from the node.
            notification = notification_channel.receiver.recv().fuse() => {
                match notification {
                    Ok(notification) => {
                        if let Some(event) = convert_notification(notification, &state) {
                            event_queue.push(event);
                        }
                    }
                    Err(e) => {
                        log::error!("Notification channel error: {}", e);
                        break;
                    }
                }
            }
        }
    }

    // Cleanup: unregister listener and disconnect.
    if let Some(id) = listener_id {
        let _ = client.rpc_api().unregister_listener(id).await;
    }
    let _ = client.disconnect().await;
    state.set_connected(false);
    log::info!("L1 bridge worker stopped");
}

/// Converts a Kaspa RPC notification into an L1Event.
fn convert_notification(notification: Notification, state: &ConnectionState) -> Option<L1Event> {
    match notification {
        Notification::BlockAdded(n) => {
            let header = &n.block.header;
            Some(L1Event::BlockAdded(BlockAdded {
                block_hash: header.hash,
                daa_score: header.daa_score,
                blue_score: header.blue_score,
                timestamp: header.timestamp,
            }))
        }
        Notification::VirtualChainChanged(n) => {
            Some(L1Event::VirtualChainChanged(VirtualChainChanged {
                removed_block_hashes: n.removed_chain_block_hashes.to_vec(),
                added_block_hashes: n.added_chain_block_hashes.to_vec(),
            }))
        }
        Notification::FinalityConflict(n) => Some(L1Event::FinalityConflict(FinalityConflict {
            violating_block_hash: n.violating_block_hash,
        })),
        Notification::FinalityConflictResolved(n) => {
            Some(L1Event::FinalityResolved(FinalityResolved {
                finality_block_hash: n.finality_block_hash,
            }))
        }
        Notification::VirtualDaaScoreChanged(n) => {
            state.set_daa_score(n.virtual_daa_score);
            Some(L1Event::DaaScoreChanged(DaaScoreChanged { daa_score: n.virtual_daa_score }))
        }
        // Ignore other notification types.
        _ => None,
    }
}
