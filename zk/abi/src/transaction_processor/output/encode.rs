use borsh::{BorshSerialize, io::Write};

use crate::{Result, transaction_processor::input::Resource};

/// Streams the execution result as a borsh-serialized `Result<Vec<Option<StorageOp>>, Error>`
/// to the writer. On success, each resource serializes as the corresponding storage operation based
/// on its dirty/deleted/new flags. On error, writes the error.
pub fn encode<W: Write>(result: Result<&[Resource<'_>]>, w: &mut W) {
    match result {
        Ok(resources) => {
            1u8.serialize(w).expect("write failed"); // Borsh Result::Ok discriminant
            (resources.len() as u32).serialize(w).expect("write failed");
            for resource in resources {
                resource.serialize(w).expect("write failed");
            }
        }
        Err(err) => {
            0u8.serialize(w).expect("write failed"); // Borsh Result::Err discriminant
            err.serialize(w).expect("write failed");
        }
    }
}
