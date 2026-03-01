# vprogs

[![Ask DeepWiki](https://deepwiki.com/badge.svg)](https://deepwiki.com/kaspanet/vprogs)

> **Note:** This repository is in early development / prototype phase. APIs and architecture may change significantly.

A Rust-based framework for based computation on the Kaspa network, featuring a transaction scheduler, execution runtime, and storage management system.

## Architecture

vprogs is organized as a layered monorepo inspired by the ISO/OSI model. Each layer has a single responsibility and communicates with adjacent layers through well-defined trait boundaries.

```
┌─────────────────────────────────────────────────────────────────────────────┐
│  Layer 5: node/                                                             │
│  ─────────────────────────────────────────────────────────────────────────  │
│  VM Reference Implementation                                                │
│  Connects the framework to the real world                                   │
├─────────────────────────────────────────────────────────────────────────────┤
│  Layer 4: transaction-runtime/                                              │
│  ─────────────────────────────────────────────────────────────────────────  │
│  Execution Semantics                                                        │
│  Defines programs, contexts, and what happens when transactions execute     │
├─────────────────────────────────────────────────────────────────────────────┤
│  Layer 3: scheduling/                                                       │
│  ─────────────────────────────────────────────────────────────────────────  │
│  Execution Orchestration                                                    │
│  Batch processing, resource tracking, parallel execution, rollback          │
├─────────────────────────────────────────────────────────────────────────────┤
│  Layer 2: state/                                                            │
│  ─────────────────────────────────────────────────────────────────────────  │
│  State Semantics                                                            │
│  Defines what we store: versioned data, pointers, rollback information      │
├─────────────────────────────────────────────────────────────────────────────┤
│  Layer 1: storage/                                                          │
│  ─────────────────────────────────────────────────────────────────────────  │
│  Persistence Layer                                                          │
│  Implements how we store: read/write coordination, backend abstraction      │
├─────────────────────────────────────────────────────────────────────────────┤
│  Layer 0: core/                                                             │
│  ─────────────────────────────────────────────────────────────────────────  │
│  Foundation                                                                 │
│  Foundational types, atomics, macros - zero domain dependencies             │
└─────────────────────────────────────────────────────────────────────────────┘
```

## Domains

| Layer | Domain | Purpose                                        | Documentation |
|-------|--------|------------------------------------------------|---------------|
| 0 | [core/](core/) | Foundational types, atomics, macros            | [README](core/README.md) |
| 1 | [storage/](storage/) | Persistence layer: how we access the disk      | [README](storage/README.md) |
| 2 | [state/](state/) | State semantics: what we store                 | [README](state/README.md) |
| 3 | [scheduling/](scheduling/) | Execution orchestration: how we access the CPU | [README](scheduling/README.md) |
| 4 | [transaction-runtime/](transaction-runtime/) | Execution semantics                            | [README](transaction-runtime/README.md) |
| 5 | [node/](node/) | VM implementation                              | [README](node/README.md) |

## Design Principles

### Layered Architecture

Each layer builds on the layers below it. Dependencies flow downward only:

- **Layer 0 (core)** - Zero dependencies on other domains. Provides foundational types (`ResourceId`, `Transaction`, `AccessMetadata`), atomics, and macros.
- **Layer 1 (storage)** - Implements persistence abstractions. Depends on core.
- **Layer 2 (state)** - Defines state spaces and versioning semantics. Depends on core and storage traits.
- **Layer 3 (scheduling)** - Orchestrates execution using state and storage. Depends on layers below.
- **Layer 4 (transaction-runtime)** - Defines execution semantics. Depends on core types.
- **Layer 5 (node)** - Integrates everything into a concrete VM. Depends on all layers.

### Trait-Driven Extensibility

Core abstractions are defined as traits, enabling modularity and different implementations:

- `VmInterface` - Abstract transaction processor
- `Store` / `ReadStore` / `WriteBatch` - Abstract state persistence
- `ResourceId` / `Transaction` / `AccessMetadata` - Abstract scheduling types

### Batch-Oriented Execution

Transactions are grouped into batches for atomic processing:

- `ScheduledBatch` - Groups transactions for execution
- `StateDiff` - Captures state changes per resource per batch
- `Rollback` - Reverts state changes when needed

## Build Commands

```bash
cargo build                 # Debug build
cargo build --release       # Release build
cargo test                  # Run all tests
cargo test --test e2e       # Run integration tests only
cargo +nightly fmt          # Format code
cargo clippy --tests        # Lint code
```

## Package Naming Convention

All packages follow the pattern: `vprogs-{domain}-{crate}`

Examples:
- `vprogs-core-types`
- `vprogs-scheduling-scheduler`
- `vprogs-storage-manager`
- `vprogs-state-version`

## Serialization

Uses [Borsh](https://borsh.io/) for state serialization throughout the codebase.

## License

See [LICENSE](LICENSE) for details.
