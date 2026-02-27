use vprogs_scheduling_scheduler::Processor;
use vprogs_storage_types::Store;
use vprogs_transaction_runtime_address::Address;
use vprogs_transaction_runtime_authenticated_data::AuthenticatedData;
use vprogs_transaction_runtime_data_context::DataContext;
use vprogs_transaction_runtime_error::{VmError, VmResult};
use vprogs_transaction_runtime_object_id::ObjectId;

use crate::TransactionRuntime;

impl<'a, 'b, S, P> DataContext for TransactionRuntime<'a, 'b, S, P>
where
    S: Store,
    P: Processor<ResourceId = ObjectId>,
{
    fn borrow(&mut self, address: Address) -> VmResult<&AuthenticatedData> {
        self.loaded_data.get(&address).ok_or(VmError::DataNotFound(address))
    }

    fn borrow_mut(&mut self, address: Address) -> VmResult<&mut AuthenticatedData> {
        self.loaded_data.get_mut(&address).ok_or(VmError::DataNotFound(address)).and_then(|data| {
            match data.mut_cap().is_some() {
                true => Ok(data),
                false => Err(VmError::MissingMutCapability(address)),
            }
        })
    }
}
