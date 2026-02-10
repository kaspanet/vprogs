# scheduling/

Defines **how we provide access** to state. This layer orchestrates transaction execution, manages resource dependencies, and coordinates parallel processing.

## Crates

### scheduler/
`vprogs-scheduling-scheduler`

The main orchestrator for transaction execution:

- **Scheduler** - Entry point for batch processing
- **RuntimeBatch** - Groups transactions for atomic execution
- **RuntimeTx** - Individual transaction runtime state
- **Resource** - Tracks dependency chains per resource
- **ResourceAccess** - Manages read/write access to resources
- **StateDiff** - Captures state changes per resource per batch
- **Rollback** - Reverts state changes during chain reorganization
- **WorkerLoop** - Background processing of batch lifecycle stages

Key flows:
1. `scheduler.schedule(txs)` - Submit transactions for execution
2. Transactions are linked into resource dependency chains
3. Execution workers process transactions in parallel
4. State diffs are persisted, then committed
5. `scheduler.rollback_to(index)` - Revert to previous state if needed

### execution-workers/
`vprogs-scheduling-execution-workers`

Thread pool for parallel transaction execution:

- Work-stealing deque for load balancing
- Configurable worker count
- Integrates with RuntimeBatch for task distribution

### test-suite/
`vprogs-scheduling-test-suite`

Integration tests in `tests/e2e.rs`:

- Batch execution and lifecycle
- Rollback scenarios
- Concurrent access patterns
- Cancellation handling

## Layer Position

```
┌─────────────────────────────────────────┐
│  Layer 4: transaction-runtime           │
├─────────────────────────────────────────┤
│  Layer 3: scheduling  ◄── You are here  │
├─────────────────────────────────────────┤
│  Layer 2: state                         │
├─────────────────────────────────────────┤
│  Layer 1: storage                       │
├─────────────────────────────────────────┤
│  Layer 0: core                          │
└─────────────────────────────────────────┘
```

The scheduling layer coordinates execution. It uses state and storage for persistence and is used by the transaction-runtime and node layers above.

## Key Abstractions

### VmInterface Trait

```rust
pub trait VmInterface: Clone + Send + Sync + 'static {
    type Transaction: Transaction<Self::ResourceId, Self::AccessMetadata>;
    type ResourceId: ResourceId;
    type AccessMetadata: AccessMetadata<Self::ResourceId>;
    // ...

    fn process_transaction<S: Store>(
        &self,
        tx: &Self::Transaction,
        resources: &mut [AccessHandle<S, Self>],
    ) -> Result<Self::TransactionEffects, Self::Error>;
}
```

This trait is implemented by the node layer to define transaction processing semantics.

## Design Philosophy

Scheduling is separated from transaction-runtime to:
1. Keep orchestration logic independent of execution semantics
2. Allow different VM implementations with the same scheduling infrastructure
3. Enable testing scheduling without full transaction execution
