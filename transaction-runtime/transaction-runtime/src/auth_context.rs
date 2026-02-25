use vprogs_scheduling_scheduler::TransactionProcessor;
use vprogs_state_space::StateSpace;
use vprogs_storage_types::Store;
use vprogs_transaction_runtime_auth_context::AuthContext;
use vprogs_transaction_runtime_object_id::ObjectId;
use vprogs_transaction_runtime_pubkey::PubKey;

use crate::TransactionRuntime;

impl<'a, 'b, S, V> AuthContext for TransactionRuntime<'a, 'b, S, V>
where
    S: Store<StateSpace = StateSpace>,
    V: TransactionProcessor<ResourceId = ObjectId>,
{
    fn has_signer(&self, pub_key: &PubKey) -> bool {
        self.signers.contains(pub_key)
    }
}
