use super::{
    batch_metadata::BatchMetadata, input::Input, journal::Journal, output::Output,
    resource::Resource,
};
use crate::{Read, Write};

/// Transaction processor API for use inside zkVM guests.
pub struct Abi;

impl Abi {
    /// Reads, decodes, executes, and commits a single transaction inside the guest.
    ///
    /// 1. Reads wire bytes from `host` and decodes into metadata + resource views.
    /// 2. Commits the structured Input segment to `journal` (before the closure).
    /// 3. Calls `f` to let the guest mutate resources in-place.
    /// 4. Streams the execution result back to `host`.
    /// 5. Commits the structured Output segment to `journal` (after the closure).
    pub fn process_transaction(
        host: &mut (impl Read + Write),
        journal: &mut impl Write,
        f: impl for<'a> FnOnce(
            &'a [u8],
            u32,
            &BatchMetadata<'a>,
            &mut [Resource<'a>],
        ) -> crate::Result<()>,
    ) {
        let mut buf = host.read_blob();
        let input = Input::decode(buf.as_mut_slice());

        // Input segment (framework-controlled, BEFORE closure).
        Journal::encode_input(journal, &input);

        let Input { tx, tx_index, batch_metadata, mut resources } = input;

        let result = f(tx, tx_index, &batch_metadata, &mut resources).map(|_| resources.as_slice());

        // Stream execution result to host.
        Output::encode(result, host);

        // Output segment (framework-controlled, AFTER closure).
        Journal::encode_output(journal, result);
    }
}
