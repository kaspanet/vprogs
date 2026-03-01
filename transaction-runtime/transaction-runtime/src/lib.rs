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
        // for handle in self.handles.iter() {
        //     match handle.access_metadata().id() {
        //         ObjectId::Program(address) => {
        //             let program = Program::deserialize(&mut handle.data().as_slice())?;
        //             self.loaded_programs.insert(address, program);
        //         }
        //         ObjectId::Data(address) => {
        //             let mut reader = handle.data().as_slice();
        //             let lock = Lock::deserialize(&mut reader)?;
        //             let data = Data::deserialize(&mut reader)?;
        //             let mut_cap = lock.unlock(self);
        //             self.loaded_data.insert(address, AuthenticatedData::new(data, mut_cap));
        //         }
        //         ObjectId::Empty => return Err(VmError::Generic),
        //     }
        // }
        let _ = &self.handles;
        let _ = &self.loaded_programs;
        todo!("recover resource data from AccessHandle when VM is implemented")
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
