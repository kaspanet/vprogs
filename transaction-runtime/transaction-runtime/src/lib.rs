use std::collections::{HashMap, HashSet};

use vprogs_scheduling_scheduler::{AccessHandle, Processor};
use vprogs_storage_types::Store;
use vprogs_transaction_runtime_address::Address;
use vprogs_transaction_runtime_authenticated_data::AuthenticatedData;
use vprogs_transaction_runtime_error::VmResult;
use vprogs_transaction_runtime_instruction::Instruction;
use vprogs_transaction_runtime_program::Program;
use vprogs_transaction_runtime_pubkey::PubKey;
use vprogs_transaction_runtime_transaction::Transaction;
use vprogs_transaction_runtime_transaction_effects::TransactionEffects;

pub struct TransactionRuntime<'a, 'b, S, P>
where
    S: Store,
    P: Processor,
{
    handles: &'a mut [AccessHandle<'b, S, P>],
    signers: HashSet<PubKey>,
    loaded_data: HashMap<Address, AuthenticatedData>,
    loaded_programs: HashMap<Address, Program>,
}

impl<'a, 'b, S, P> TransactionRuntime<'a, 'b, S, P>
where
    S: Store,
    P: Processor,
{
    pub fn execute(
        tx: &'a Transaction,
        handles: &'a mut [AccessHandle<'b, S, P>],
    ) -> VmResult<TransactionEffects> {
        let signers = HashSet::new();
        let loaded_data = HashMap::new();
        let loaded_programs = HashMap::new();
        let mut this = Self { handles, signers, loaded_data, loaded_programs };

        this.ingest_state()?;

        for instruction in tx.instructions() {
            this.execute_instruction(instruction)?;
        }

        this.finalize()
    }

    fn ingest_state(&mut self) -> VmResult<()> {
        for handle in self.handles.iter() {
            // TODO: recover ObjectId from ResourceId when VM is implemented
            todo!("recover ObjectId from ResourceId when VM is implemented");
            #[allow(unreachable_code)]
            {
                let _ = handle;
            }
        }
        Ok(())
    }

    fn execute_instruction(&mut self, instruction: &Instruction) -> VmResult<()> {
        match instruction {
            Instruction::PublishProgram { program_bytes } => {
                let _ = program_bytes;
                // TODO: CHECK BYTES
                // TODO: STORE PROGRAM
                // TODO: PUSH PROGRAM ID TO RETURN VALUES
            }
            Instruction::CallProgram { program_id, args } => {
                let _ = (program_id, args);
                // TODO: CREATE PROGRAM CONTEXT
                // RESOLVE ARGS
                // EXECUTE PROGRAM WITH INVOCATION CONTEXT
                // TODO: HANDLE RETURN VALUES / TEAR DOWN PROGRAM CONTEXT
            }
        }
        Ok(())
    }

    fn finalize(self) -> VmResult<TransactionEffects> {
        Ok(TransactionEffects {})
    }
}

mod auth_context;
mod data_context;
