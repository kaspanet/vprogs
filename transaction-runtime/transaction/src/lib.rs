use vprogs_core_types::AccessMetadata;
use vprogs_transaction_runtime_instruction::Instruction;

pub struct Transaction {
    accessed_objects: Vec<AccessMetadata>,
    instructions: Vec<Instruction>,
}

impl Transaction {
    pub fn new(accessed_objects: Vec<AccessMetadata>, instructions: Vec<Instruction>) -> Self {
        Transaction { accessed_objects, instructions }
    }

    pub fn accessed_objects(&self) -> &[AccessMetadata] {
        &self.accessed_objects
    }

    pub fn instructions(&self) -> &[Instruction] {
        &self.instructions
    }
}
