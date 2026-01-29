use kaspa_hashes::Hash as BlockHash;
use kaspa_rpc_core::{RpcBlock, api::rpc::RpcApi};
use kaspa_wrpc_client::KaspaRpcClient;

use crate::{L1BridgeError, Result};

/// Extension trait for KaspaRpcClient with convenience methods.
pub trait KaspaRpcClientExt {
    /// Fetches a block by hash with transactions included.
    fn fetch_block(
        &self,
        hash: BlockHash,
    ) -> impl std::future::Future<Output = Result<RpcBlock>> + Send;

    /// Fetches the parent hashes of a block.
    fn fetch_block_parents(
        &self,
        hash: BlockHash,
    ) -> impl std::future::Future<Output = Result<Vec<BlockHash>>> + Send;
}

impl KaspaRpcClientExt for KaspaRpcClient {
    async fn fetch_block(&self, hash: BlockHash) -> Result<RpcBlock> {
        self.get_block(hash, true)
            .await
            .map_err(|e| L1BridgeError::Rpc(format!("get_block failed: {}", e)))
    }

    async fn fetch_block_parents(&self, hash: BlockHash) -> Result<Vec<BlockHash>> {
        let block = self.fetch_block(hash).await?;
        Ok(block.header.parents_by_level.first().cloned().unwrap_or_default())
    }
}
