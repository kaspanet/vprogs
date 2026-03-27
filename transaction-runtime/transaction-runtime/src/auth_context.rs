use vprogs_scheduling_scheduler::Processor;
use vprogs_storage_types::Store;
use vprogs_transaction_runtime_auth_context::AuthContext;
use vprogs_transaction_runtime_pubkey::PubKey;

use crate::TransactionRuntime;

impl<'a, 'b, S: Store, P: Processor<S>> AuthContext for TransactionRuntime<'a, 'b, S, P> {
    fn has_signer(&self, pub_key: &PubKey) -> bool {
        self.signers.contains(pub_key)
    }
}
