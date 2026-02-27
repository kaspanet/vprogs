use vprogs_scheduling_scheduler::Processor;
use vprogs_storage_types::Store;
use vprogs_transaction_runtime_auth_context::AuthContext;
use vprogs_transaction_runtime_object_id::ObjectId;
use vprogs_transaction_runtime_pubkey::PubKey;

use crate::TransactionRuntime;

impl<'a, 'b, S, P> AuthContext for TransactionRuntime<'a, 'b, S, P>
where
    S: Store,
    P: Processor<ResourceId = ObjectId>,
{
    fn has_signer(&self, pub_key: &PubKey) -> bool {
        self.signers.contains(pub_key)
    }
}
