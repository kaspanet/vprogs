//! wRPC client construction, mirroring the bridge's own client setup.

use std::time::Duration;

use kaspa_consensus_core::network::NetworkId;
use kaspa_wrpc_client::prelude::*;

/// Connects a Borsh wRPC client to `url`, mirroring the bridge's own client construction.
/// Panics on failure; use [`try_connect_wrpc`] from paths that must surface errors.
pub async fn connect_wrpc(url: &str, network_id: NetworkId) -> KaspaRpcClient {
    try_connect_wrpc(url, network_id).await.expect("connect to node wRPC")
}

/// Fallible [`connect_wrpc`]: reports client construction and connection failures as messages
/// instead of panicking, for callers that surface errors to an operator.
pub async fn try_connect_wrpc(url: &str, network_id: NetworkId) -> Result<KaspaRpcClient, String> {
    let client =
        KaspaRpcClient::new_with_args(WrpcEncoding::Borsh, Some(url), None, Some(network_id), None)
            .map_err(|e| format!("create wRPC client: {e}"))?;
    client
        .connect(Some(ConnectOptions {
            block_async_connect: true,
            connect_timeout: Some(Duration::from_millis(10_000)),
            ..Default::default()
        }))
        .await
        .map_err(|e| format!("connect to node wRPC: {e}"))?;
    Ok(client)
}
