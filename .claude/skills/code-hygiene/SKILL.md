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

Comments describe what the code IS, not how callers consume it. The "how
it's used" relationships change during refactors, leaving comments out of
sync with their context. The same applies to:

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

### 5. Every public function and field gets a doc comment

The only exception: trait `impl` methods. The trait definition documents the
contract; impls inherit it. Implementations only need their own doc when
they add behavior beyond the trait contract.

Trivial items still get docs, but the docs can be one line
(`/// Creates an empty accumulator.`, `/// Returns the number of leaves
added so far.`).

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

If a doc paragraph is currently hand-wrapped, unwrap it into one long line
and let the formatter re-flow.

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
6. **Format**: keep the diff tight; don't reflow lines that don't need
   changes.
