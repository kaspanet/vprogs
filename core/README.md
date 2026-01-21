# core/

Foundation layer used throughout the vprogs codebase. This layer has zero dependencies on other vprogs domains.

## Crates

### types/
`vprogs-core-types`

Foundational trait definitions that enable the scheduling system's genericity:

- **ResourceId** - Trait for resource identifiers (serialization to/from bytes)
- **Transaction** - Trait for transactions (provides accessed resources)
- **AccessMetadata** - Trait for access metadata (id + access type)
- **AccessType** - Enum for read/write access classification

These traits allow the scheduling layer to work with any concrete types that implement them.

### atomics/
`vprogs-core-atomics`

Concurrent atomic wrappers for lock-free programming:

- **AtomicAsyncLatch** - Async-aware latch for signaling completion
- **AtomicEnum** - Atomic enum wrapper using discriminant mapping

### macros/
`vprogs-core-macros`

Procedural macros for code generation:

- **#[smart_pointer]** - Generates a wrapper struct with `Deref` implementation and a `Ref` type alias for `Arc<Data>` patterns

## Layer Position

```
┌─────────────────────────────────────────┐
│  Layer 3: scheduling                    │
├─────────────────────────────────────────┤
│  Layer 2: state                         │
├─────────────────────────────────────────┤
│  Layer 1: storage                       │
├─────────────────────────────────────────┤
│  Layer 0: core  ◄── You are here        │
└─────────────────────────────────────────┘
```

The core layer is the foundation. All other layers may depend on core, but core depends on no other vprogs domains.
