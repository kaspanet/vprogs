---
name: code-hygiene
description: Final-pass code hygiene on Rust files in this repo. Applies vprogs's comment and small-scale code conventions (contract-first docs, semantic-block inline notes, no em-dashes, no implementation leaks, redundant-link / derive-substitution checks). Use after finishing a feature, before opening a PR, or whenever a reviewer asks for a cleanup pass.
---

# Code hygiene pass

Sweep the named files (or, if none given, the files touched in the current
branch via `git diff --name-only main...HEAD`) for vprogs's comment and
small-scale code-shape conventions. The goal is a clean, contract-first
surface that reviewers can scan without effort.

The skill skews heavily toward comment hygiene (that's where most of the
rules are), but also covers trivial code-shape wins that surface during the
same read-through (e.g. a manual `Default` impl that could be `#[derive]`).
Structural surgery (splitting files, renaming types across the codebase,
moving items between modules) is OUT of scope; flag it, don't do it.

## Core principles

### 1. Doc comments are contracts, not implementation walkthroughs

Doc comments tell the **caller** what the item does: what it takes, what it
returns, what non-obvious side-effects or edge-case return values exist.
They do NOT walk through how the function achieves the contract — that's
implementation detail.

A doc comment that narrates implementation steps ("first X, then Y, then Z")
is a smell. State the contract; if the body needs narration, push it inline.

### 2. Inline comments for complex bodies, organized as semantic blocks

If a function body is complex enough to need explanation, annotate it with
short inline comments — each labeling a semantic block of logic. Separate
blocks with blank lines so structure is visible at a glance.

Ideal inline comment: one line. Two or three for something genuinely
subtle is fine. If you need a paragraph, either the code is too complex
(refactor it) or the comment is restating the doc contract (drop it).

A wall of comment text on a function indicates a problem in the code,
not a documentation gap.

### 3. Don't describe how the code is used elsewhere

Comments describe what the code IS or DOES, not which callers consume it.
For a type, that's the role it plays in the system; for a function, its
effects, returns, and important edge cases / invariants. Caller
relationships change during refactors, leaving comments out of sync with
their context. The same applies to:

- Naming sibling tests in doc comments ("`X_test` verifies this")
- Listing specific upstream callers
- Naming consumer crates that depend on this item

**Exception**: file-level (module) comments documenting the file's role in
the broader architecture. Those help readers orient quickly from a
search/jump-to-definition landing.

### 4. Files should be understandable in isolation

A reader landing on a file should be able to understand what the file is
for from its module-level doc plus per-item docs, without reading the
whole crate. Important architectural decisions live at the **file level**,
not scattered across function docs.

If a fact applies to the whole module ("this file is the permission tree's
on-chain commitment wiring"), put it once at the top — don't repeat it on
every item.

### 5. Every function and field gets a doc comment

This includes private fns, `pub(crate)` items, and trivial accessors —
not just the public API surface. The cost of a one-line `///` is
trivial; the cost of inconsistency ("why does this private fn have a
doc but its sibling doesn't?") is a real review-time tax.

The only exception: trait `impl` methods. The trait definition documents the
contract; impls inherit it. Implementations only need their own doc when
they add behavior beyond the trait contract.

Watch for the inverse smell too: when impl-specific commentary has migrated
up onto the type's doc and back-references a method by name
(`/// method_name prepends X...`), pull it down onto the impl method. The
type doc is for what the type IS; behavior specific to one method on it
belongs on the method. See example 8.

Trivial items still get docs, but the docs can be one line
(`/// Creates an empty accumulator.`, `/// Returns the number of leaves
added so far.`, `/// Bounds-checked access to `keys[idx]`.`).

When sweeping a file, watch for the *partial-coverage* smell: most items
documented, one or two skipped because they "felt obvious." That's the
inconsistency tax above — either document them in the same one-line voice
as the siblings, or remove the existing private-fn docs from the rest of
the file. Mixed coverage is the wrong answer.

## Specific rules

### No em-dashes

Detect `—` (U+2014) in `.rs` files. Replace contextually, never via bulk
substitution — different em-dashes want different replacements:

| Use case | Replacement |
|---|---|
| Definitional (`X — Y describes X`) | `:` or `(...)` |
| Two independent clauses | `;` or two sentences |
| Parenthetical aside | `(...)` |
| Comma-equivalent (rare) | `,` |

For multi-line em-dashes (em-dash at end of line continuing on the next),
read both lines to choose the right repair.

### Don't restate algorithms across levels

If a function `compute_X` has the algorithm in its doc, a const `X` (which
is just the precomputed result) doesn't need to restate the algorithm —
link to `compute_X` and let the doc-link do the work.

### Drop redundant reference-style link definitions

Rustdoc resolves `[`Name`]` in a doc comment from the surrounding scope.
Anything brought in by a `use` statement at the top of the file is already
in scope, so `[`Name`]` auto-resolves to it via the intra-doc shortcut.

Adding `//! [`Name`]: full::path::Name` at the bottom of the file for such
an item is redundant: there are now two declarations of the same target.
That's both noise and a small ambiguity hazard — a reader sees the explicit
definition and wonders whether it's pointing somewhere different from the
`use`, or why the file felt the need to override the in-scope resolution.

Keep reference-style definitions only for items that are NOT in scope and
would be ugly inline (deep paths, paths used multiple times in the prose).
For an out-of-scope item mentioned once, prefer the inline form
`[`Name`](full::path::Name)`.

**Before**:
```rust
//! Uses [`StandardSpk`] and [`ExitAccumulator`] and references
//! [`StateTransition::permission_spk_hash`].

use vprogs_zk_abi::{batch_processor::ExitAccumulator, transaction_processor::StandardSpk};

// ... file body ...

//! [`StandardSpk`]: vprogs_zk_abi::transaction_processor::StandardSpk
//! [`ExitAccumulator`]: vprogs_zk_abi::batch_processor::ExitAccumulator
//! [`StateTransition::permission_spk_hash`]: vprogs_zk_abi::batch_processor::StateTransition::permission_spk_hash
```

**After**:
```rust
//! Uses [`StandardSpk`] and [`ExitAccumulator`] and references
//! [`StateTransition::permission_spk_hash`].

use vprogs_zk_abi::{batch_processor::ExitAccumulator, transaction_processor::StandardSpk};

// ... file body ...

//! [`StateTransition::permission_spk_hash`]: vprogs_zk_abi::batch_processor::StateTransition::permission_spk_hash
```

Only the out-of-scope path survives; the two in-scope ones are dropped
because the `use` already does the job.

### Don't over-specify when the abstraction is generic

If a function delegates to a generic `Hasher`, the doc shouldn't claim a
specific hash function (`SHA-256`). Use abstract `hash`. The specific
choice can be named at the higher level where it's actually made (e.g. the
`mod config` block that pins the hasher).

### Don't put implementation details in doc comments

The contract is what the function returns / does, not how it gets there.
Whether a value is computed on-demand or pulled from a precomputed table
is internal.

### Don't name tests in doc comments

`/// X_test verifies this` is implementation, not API. Inline comments in
test code MAY cross-reference sibling tests for maintainer convenience,
but doc comments should not.

### Move rationale to its decision site

If a `const`'s `/// Sized for X reason` rationale is really about how the
const is chosen by upstream config, move it to the config site (e.g. the
type alias / config struct), not on the const.

### Combine related return-value clauses

When a function has a primary return value plus a special-case return, write them as one
sentence joined with `, or ... if ...` rather than two consecutive "Returns" sentences.

**Before**:
```
/// Returns the padded tree root. Returns `empty_hashes[0]` if no leaves were added.
```

**After**:
```
/// Returns the padded tree root, or `empty_hashes[0]` if no leaves were added.
```

The same applies to multiple edge cases — chain them with commas when each is brief:
`/// Returns X, or Y if A, or Z if B.`

### Punctuation fragments

`Default [TraitName], does X.` reads as a fragment — the comma between a
noun phrase and a predicate creates an awkward break.

- Colon for elaboration: `Default [TraitName]: does X.`
- Or split: `Default [TraitName] for ABC. Does X.`

### Don't manually wrap doc-comment lines

Write continuation text as a single long line. `./cargo-fmt-all.sh` (rustfmt
on nightly) handles wrapping to the project's 100-char limit. Manually
breaking `///` lines mid-sentence makes future edits clunky (the wrap point
drifts) and produces diffs that fight the formatter.

If you're already editing a doc paragraph (changing its wording, adding a
link, moving it across items), unwrap it to a single long line as part of
that edit and let the formatter re-flow. Don't gratuitously reflow doc
paragraphs you aren't otherwise touching — keep the diff to what actually
changed. The rule is: edits trigger the unwrap; passive readthroughs
don't.

**What's still fine** (and is project convention): blank `///` lines that
separate paragraphs. The first sentence is the summary; if you need more,
add a blank `///` line and write the rest as a fresh paragraph. The rule is
against wrapping *one sentence* across multiple lines, not against having
multiple paragraphs.

```rust
/// One-line summary contract sentence.
///
/// Optional second paragraph with additional context, edge cases, or
/// references. This paragraph is also written as one long line per
/// sentence; the formatter wraps it.
pub fn foo() { ... }
```

### Avoid widow words at wrap points

After the formatter runs, scan multi-line doc/inline comments for *widows*:
wraps where the last line holds only one or two short words ("`were added.`",
"`runtime.`", "`as input.`"). They look untidy and can often be eliminated
with a small rephrase — either tightening the sentence to fit on one line,
or restructuring so the wrap lands at a more balanced point — **but only
when the rephrase is natural**. If reworking the sentence to dodge the
widow makes the prose awkward, loses precision, or drops a meaningful
qualifier, leave the widow. Readability is the goal, not a strict
line-count rule.

**Before** (widow `were added.` on line 2):
```
/// Returns the permission tree's P2SH script-hash, or `[0u8; 32]` when no exits
/// were added.
```

**After** (fits on one line, no meaning lost):
```
/// Returns the permission tree's P2SH script-hash, or `[0u8; 32]` if empty.
```

This is NOT a contradiction with "don't manually wrap doc-comment lines".
You still write each sentence as one long source line and let the formatter
wrap it; you just treat the formatter's output as feedback. If it wraps
into a widow AND a natural rephrase exists, take the rephrase. Otherwise
the formatter's wrap stands.

### Manual impls vs derives

A `Default` impl that just calls `Self::new()` can become `#[derive(Default)]`
when:
- All fields satisfy `Default`, AND
- Adding `Default` to the type's generic bounds doesn't constrain
  legitimate use cases.

Otherwise keep the manual impl — explicit bounds beat derive convenience
when the derive would over-constrain.

### Same-typed positional arguments / returns need a struct

When a function or closure has two or more positional arguments (or
returns) of the **same type** (or types that differ only by reference /
mutability over the same payload, like `[u8; 32]` and `&[u8; 32]`),
introduce a named struct instead. With only position to disambiguate,
every call site has to remember which slot goes where, and a swap typo
compiles silently.

Tuples / positional arguments with **distinct** types are fine:
`(String, u32)` or `fn foo(name: String, age: u32)` need no struct because
the types themselves disambiguate. Same for `(Foo, Bar, Baz)`. The smell
is *positional repetition over the same payload type*, where only the
slot index carries meaning.

This applies in three places:

- **Function returns**: `fn foo() -> (u64, u64)` → introduce a struct with
  named fields.
- **Function parameters**: `fn foo(a: Hash, b: Hash, c: Hash)` → group into
  a struct (often a `*Input` / `*Step` / `*Pins` type).
- **Closure / `Fn` trait bounds**: `Fn(&[u8; 32], &[u8; 32]) -> T` → pass a
  small struct argument instead. The struct lifts the parameter names out
  to the type level, so the trait bound becomes `Fn(MyArgs<'_>) -> T` and
  every implementor's call site reads `MyArgs { foo, bar }`.

Mixed value/reference of the same type counts as the smell: `fn foo(owner:
[u8; 32], borrowed: &[u8; 32])` is just as easy to swap by accident as
`fn foo(a: [u8; 32], b: [u8; 32])`, since the slot order is still the only
guide. A struct dodges that.

**Before** (closure `Fn` bound with two `&[u8; 32]`):
```rust
struct Config<BuildPins>
where
    BuildPins: for<'a> Fn(&'a [u8; 32], &'a [u8; 32]) -> RedeemPins<'a>,
{
    build_pins: BuildPins,
}

// every call site has to remember program_id is first, tx_image_id second:
(config.build_pins)(&program_id, &tx_image_id)
```

**After**:
```rust
struct ImageIds<'a> {
    program_id: &'a [u8; 32],
    tx_image_id: &'a [u8; 32],
}

struct Config<BuildPins>
where
    BuildPins: for<'a> Fn(ImageIds<'a>) -> RedeemPins<'a>,
{
    build_pins: BuildPins,
}

(config.build_pins)(ImageIds { program_id: &program_id, tx_image_id: &tx_image_id })
```

The struct stays at the trait-bound boundary, so the closure body
destructures it (`|ImageIds { program_id, tx_image_id }| ...`) and the
call site can't swap the two `[u8; 32]`s.

### Match surrounding field-doc voice

Inside a struct, sibling fields establish a voice — typically one short
sentence naming what the field IS, optionally with a one-clause qualifier.
When adding a new field, scan the siblings and match their brevity. A
multi-line doc dropped next to one-line siblings reads as noise even when
each individual sentence is defensible.

The rule is voice-matching, not a hard length cap: if the surrounding
fields already carry usage details, match that. If they're terse, be terse.

**Before** (5 lines next to 1-line siblings):
```rust
/// Lane tip after applying this block's accepted txs.
pub lane_tip: Hash,
/// True when the lane was silent past the finality window at this block.
pub lane_expired: bool,
/// Most-recent settlement of the bridge's configured covenant observed at or before this
/// block, or `None` if none has been observed since the bridge started. Carried forward from
/// the parent on blocks that contain no new settlement; replaced when a new one lands in this
/// block. Compare `last_settlement.containing_block` against this block's `hash` to tell the
/// two cases apart.
pub last_settlement: Option<SettlementInfo>,
```

**After**:
```rust
/// Lane tip after applying this block's accepted txs.
pub lane_tip: Hash,
/// True when the lane was silent past the finality window at this block.
pub lane_expired: bool,
/// Most-recent settlement of the configured covenant, or `None` until one lands.
pub last_settlement: Option<SettlementInfo>,
```

Why: the carry-forward behavior, the bridge-context, and the "compare
against block hash" usage hint all migrate to where they actually belong —
the field's type doc covers the data shape; callers handle their own
comparisons. The field itself just names what it carries.

### Field docs name what the field IS, not what happens when set

For config / option fields, the doc should describe the field's *identity* —
what value it carries and what that value means — not the behavior triggered
when the value is set. The "When set, X is scanned for Y" or "If `Some`, Z
is invoked" phrasing is a smell: that behavior lives in the consumer that
READS the field, and its docs are the right place for it.

Naming the role (often by linking to the field that consumes it) is
shorter, truer, and stays correct when the consumer's algorithm changes.

**Before**:
```rust
/// Covenant id this bridge tracks settlements for, or `None` to ignore covenant
/// activity. When set, accepted transactions in each chain block are scanned
/// for an output bound to this id; on a match, the settlement is decoded into
/// the block's [`ChainBlockMetadata::last_settlement`].
pub covenant_id: Option<Hash>,
```

**After**:
```rust
/// Covenant id surfaced through [`ChainBlockMetadata::last_settlement`], or
/// `None` to disable.
pub covenant_id: Option<Hash>,
```

Why: the WHAT is "the covenant id whose settlements the bridge surfaces
into the metadata field". The "When set, accepted txs are scanned..." prose
is implementation of how the field is used, lives in the worker, and would
need updating if the worker's loop shape changed. The link to
`last_settlement` does the work.

### Extend existing iterations; don't split filtered loops into two passes

When adding a new derived value alongside an existing iteration (filtering,
collecting, mapping), prefer a captured accumulator updated inside the
existing closure over a "collect everything first, iterate again" rewrite.
Side-effects in closures are idiomatic when the closure is already doing
similar work, and a single-pass loop preserves the original control flow
reviewers already understand.

This applies especially to **filtered** collections: building an unfiltered
intermediate `Vec` just to satisfy a second scan doubles allocation and
obscures the filter shape. Push the second concern into the same
`filter_map` closure via a mutable accumulator captured from the enclosing
scope.

When the second concern is "carry the latest value forward across
iterations, defaulting to a prior state if none observed", seed the
accumulator from the prior state and update via `.or(accumulator)` inside
the loop — that one expression handles both "overwrite when something new
is found" and "preserve when nothing matches", and the variable holds the
final answer without an extra `.or(prior_state)` at the call site.

**Before** (single-pass `filter_map` ballooned into two passes to add
settlement detection + a separate `.or(parent.last_settlement)` at the
struct site):
```rust
let l1_txs: Vec<(u32, L1Transaction)> = chain_block
    .accepted_transactions
    .iter()
    .enumerate()
    .map(|(idx, tx)| (idx as u32, L1Transaction::try_from(tx.clone()).expect("missing tx fields")))
    .collect();

let new_settlement = self.covenant_id.as_ref().and_then(|cid| {
    l1_txs.iter().find_map(|(_, tx)| detect_settlement(cid, block_hash, tx))
});

let accepted_transactions: Vec<(u32, L1Transaction)> = l1_txs
    .into_iter()
    .filter(|(_, tx)| match self.subnetwork_filter.as_ref() {
        Some(want) => tx.subnetwork_id == *want,
        None => true,
    })
    .collect();

// ...later, at the struct construction:
let checkpoint = self.virtual_chain.advance_tip(ChainBlockMetadata {
    last_settlement: new_settlement.or(parent_meta.last_settlement),
    // ...
});
```

**After** (single pass; accumulator seeded from parent state and updated in place):
```rust
let mut last_settlement = parent_meta.last_settlement;
let accepted_transactions: Vec<(u32, L1Transaction)> = chain_block
    .accepted_transactions
    .iter()
    .enumerate()
    .filter_map(|(idx, tx)| {
        let tx = L1Transaction::try_from(tx.clone()).expect("missing tx fields");
        if let Some(id) = &self.covenant_id {
            last_settlement = tx.settlement_info(id, block_hash).or(last_settlement);
        }
        match self.subnetwork_filter.as_ref() {
            Some(want) if tx.subnetwork_id != *want => None,
            _ => Some((idx as u32, tx)),
        }
    })
    .collect();

// ...later, at the struct construction:
let checkpoint = self.virtual_chain.advance_tip(ChainBlockMetadata {
    last_settlement,
    // ...
});
```

Why: the After version preserves the original `filter_map` shape, allocates
one Vec instead of two, and folds the carry-forward semantics into the
accumulator's seed + the `.or(last_settlement)` accumulation. The struct
site just uses the variable — no extra coalesce step needed.

**Exception**: when the second concern needs a *complete* collection up
front (e.g., sort then process, check uniqueness then dedupe), two passes
are the right answer. The rule targets cases where the second concern can
be folded into the existing closure without changing semantics.

## Concrete examples from prior cleanups

### Example 1 — algorithm restated at two levels

The `compute_empty_hashes` function in `core/merkle-tree` documents the
recurrence. The `PERM_EMPTY_HASHES` const in `permission_tree.rs` used to
restate it:

**Before** (on `PERM_EMPTY_HASHES`):
```
/// `[0]` is `sha256("PermEmpty")` — the empty leaf; `[L]` is
/// `sha256("PermBranch" || [L-1] || [L-1])` — the root of an
/// all-empty subtree of height `L`. Computing these in the guest
/// costs one SHA-256 per level; the table makes it an array read.
/// `empty_hashes_table_matches_runtime` revalidates it.
```

**After**:
```
/// The permission tree's empty-subtree hash table, precomputed at
/// build time (see `build.rs`) by [`Builder::compute_empty_hashes`]
/// so the guest does no hashing work for empty subtrees at runtime.
```

Why: the recurrence lives on `compute_empty_hashes`. The link does the
work. The test name and SHA-256 specificity also got dropped.

### Example 2 — implementation leak in doc

**Before**:
```
/// Empty-subtree hash: `hash(EMPTY_TAG)`. Returns the precomputed
/// value from [`EMPTY_HASHES`]`[0]` — no runtime hashing.
pub fn hash_empty() -> [u8; 32] { Self::EMPTY_HASHES[0] }
```

**After**:
```
/// Empty-subtree hash: `hash(EMPTY_TAG)`.
pub fn hash_empty() -> [u8; 32] { Self::EMPTY_HASHES[0] }
```

Why: the contract is "returns the empty-subtree hash." Whether it's
looked up or computed is internal.

### Example 3 — doc walking through implementation

**Before**:
```
/// Finalizes the accumulator: builds the padded tree, wraps the root
/// in the permission redeem script, and returns `blake2b(redeem)` —
/// the P2SH script-hash that the settlement exit output's SPK pays
/// to. Returns `[0u8; 32]` when no exits were added.
pub fn finalize(&self) -> [u8; 32] {
    let count = self.builder.leaf_count();
    if count == 0 { return [0u8; 32]; }
    let depth = Builder::required_depth(count as usize);
    // `builder.finalize` already returns the root at exactly
    // `required_depth(count)` levels.
    let root = self.builder.finalize(&Self::EMPTY_HASHES);
    // Wrap the root in the permission redeem script and return its
    // P2SH script-hash: the settlement exit output's SPK is
    // `pay_to_script_hash` of this value.
    let redeem = build_permission_redeem_script(&root, count as u64, depth);
    blake2b_script_hash(&redeem)
}
```

**After**:
```
/// Returns the permission tree's P2SH script-hash (the value the
/// settlement exit output's SPK pays to) or `[0u8; 32]` when no exits
/// were added.
pub fn finalize(&self) -> [u8; 32] {
    // Return early if no exits were added.
    let count = self.builder.leaf_count();
    if count == 0 { return [0u8; 32]; }

    // Padded merkle root.
    let depth = Builder::required_depth(count as usize);
    let root = self.builder.finalize(&Self::EMPTY_HASHES);

    // Wrap in the permission redeem script and return its P2SH script-hash.
    let redeem = build_permission_redeem_script(&root, count as u64, depth);
    blake2b_script_hash(&redeem)
}
```

Why: doc stated implementation steps that the body already shows. Doc
now states the contract (return value, empty-case behavior); body has
brief inline comments per semantic block, separated by blank lines.

### Example 4 — rationale at wrong level

**Before** (on `pub const MAX_DEPTH`):
```
/// Maximum permission-tree depth. Sized to `u32::MAX` leaves.
pub const MAX_DEPTH: usize = Builder::MAX_DEPTH;
```

**After** (split between the const and the config site):

On the const:
```
/// Maximum tree depth.
pub const MAX_DEPTH: usize = Builder::MAX_DEPTH;
```

On the `Builder` type alias inside `mod config`:
```
/// Concrete [`StreamingBuilder`] instantiation for the permission
/// tree: SHA-256 hasher, [`Tags`], 32-level depth bound, 1-byte tags.
///
/// Depth = 32 covers `u32::MAX` leaves (the accumulator's count
/// type), keeping `required_depth`'s clamp non-lossy and the
/// builder's stack non-overflowing for any input.
pub(super) type Builder = StreamingBuilder<Sha256, Tags, 32, 1>;
```

Why: the "Sized to u32::MAX leaves" rationale is a config decision.
It belongs at the config decision site, not on the public const that
just forwards the value.

### Example 5 — em-dash replacements (per category)

- **Definitional**: `Schnorr P2PK — 32-byte pubkey.` → `Schnorr P2PK: 32-byte pubkey.`
- **Two clauses**: `Streams to EOF — entries are read until exhausted.` → `Streams to EOF; entries are read until exhausted.`
- **Parenthetical**: `// Proves it is genuinely a const fn — usable in const context, the whole point.` → `// Proves it is genuinely a const fn (usable in const context, the whole point).`
- **At line end, multi-line**: read both lines, choose the joiner that makes the combined sentence work. Often `;` or a colon.

### Example 6 — sentence-fragment after structural noun

**Before**:
```
/// Default [`ExitAccumulator`], accumulates the bundle's exits into
/// a permission tree and finalizes to the on-chain commitment.
```

**After**:
```
/// Default [`ExitAccumulator`]: accumulates the bundle's exits into
/// a permission tree and finalizes to the on-chain commitment.
```

Why: the comma between the noun phrase and the predicate creates a
fragment. The colon signals "what follows describes the noun."

### Example 7 — manual `Default` impl → derive

**Before**:
```rust
impl Default for PermissionTreeAccumulator {
    fn default() -> Self { Self::new() }
}
```

**After**:
```rust
#[derive(Default)]
pub struct PermissionTreeAccumulator {
    builder: Builder,  // Builder also impls Default
}
```

Why: derive works when all fields impl Default AND adding the bound to
the type's generics doesn't break legitimate use. Here the struct is
non-generic; trivial win.

**Counter-example** (when NOT to derive): `StreamingBuilder<H, T, ...>` has
a manual `Default` impl because deriving would add `H: Default + T: Default`
bounds, constraining Hasher/NodeTags impls that have no reason to be
`Default`.

### Example 8 — impl-specific commentary parked on the type

**Before**:
```rust
/// SHA-256 implementation of the [`Hasher`] trait.
///
/// `hash_parts_with_domain` prepends the domain bytes to the payload
/// (SHA-256 has no native keyed mode). The hash is equivalent to
/// `sha2::Sha256::new_with_prefix(domain).update(part).finalize()`
/// byte-for-byte.
pub struct Sha256;

impl Hasher for Sha256 {
    fn hash_parts_with_domain<const N: usize>(
        domain: &[u8; N],
        parts: impl IntoIterator<Item = impl AsRef<[u8]>>,
    ) -> [u8; 32] { ... }
}
```

**After**:
```rust
/// SHA-256 implementation of the [`Hasher`] trait.
pub struct Sha256;

impl Hasher for Sha256 {
    /// Prepends the domain bytes to the payload (SHA-256 has no native
    /// keyed mode). Equivalent to
    /// `sha2::Sha256::new_with_prefix(domain).update(part).finalize()`
    /// byte-for-byte.
    fn hash_parts_with_domain<const N: usize>(
        domain: &[u8; N],
        parts: impl IntoIterator<Item = impl AsRef<[u8]>>,
    ) -> [u8; 32] { ... }
}
```

Why: rule #5 says trait impl methods inherit the trait's contract; they
only get their own doc when they add impl-specific behavior. Here the
domain-separation strategy IS impl-specific (SHA-256 has no keyed mode, so
this impl chooses prepending). Putting it on the type forced the prose to
back-reference the method by name; on the impl method it just describes
itself, and the type doc shrinks to its one-line "what this is" role.

## How to operate

1. **Identify scope**: explicit file list from the user, or
   `git diff --name-only main...HEAD` for branch changes.
2. **For each file**: read it, find issues per the rules, propose changes
   in a short written report before applying.
3. **Mechanical changes** (em-dash → punctuation, trivial derive
   substitutions): apply directly without asking.
4. **Substantive rewrites** (restructuring docs, moving rationale across
   sites): ASK before applying.
5. **After edits**: run `./cargo-check-all.sh` and report any failures.
6. **Format**: keep the diff tight. Don't reflow code or doc paragraphs
   you aren't otherwise touching. For doc paragraphs you *are* editing,
   unwrap to a single line as part of the edit (the formatter handles
   re-wrapping). Format via `./cargo-fmt-all.sh`; never invoke
   `cargo +nightly fmt` directly — the project script covers all 8
   workspaces.
