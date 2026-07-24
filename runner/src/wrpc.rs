//! wRPC client construction, mirroring the bridge's own client setup.

use std::time::Duration;

use kaspa_consensus_core::network::NetworkId;
use kaspa_wrpc_client::prelude::*;

/// Connects a Borsh wRPC client to `url`, mirroring the bridge's own client construction.
pub async fn connect_wrpc(url: &str, network_id: NetworkId) -> KaspaRpcClient {
    let client =
        KaspaRpcClient::new_with_args(WrpcEncoding::Borsh, Some(url), None, Some(network_id), None)
            .expect("create wRPC client");
    client
        .connect(Some(ConnectOptions {
            block_async_connect: true,
            connect_timeout: Some(Duration::from_millis(10_000)),
            ..Default::default()
        }))
        .await
        .expect("connect to node wRPC");
    client
}
