# mimalloc Rust Rewrite Plan

This is the living implementation and learning plan for rewriting mimalloc v3
in Rust.

The companion document
[`mimalloc-v3-architecture.md`](mimalloc-v3-architecture.md) describes the
existing C allocator. This document describes how we will learn and rebuild it
one small concept at a time.

## Current Status

| Property | Value |
|---|---|
| Date | September 19, 2026 |
| C reference branch | `main3` |
| C reference commit | `31d034d94cdb8e22f7d7ed55967f581a2d6e831d` |
| Rust directory | `mimalloc-rust/` |
| Cargo package | `mimalloc-rust` |
| Initial platform | Linux x86-64 |
| Working branch | `rust-rewrite-phase0` |
| Base feature branch | `main3-rust-rewrite` |
| Current phase | Phase 0 complete |
| Next phase | Divide one OS page into fixed-size blocks |

## Working Method

This rewrite is intentionally bottom-up.

We will not begin by creating the final crate hierarchy, public API, arena
manager, or threading model. Those structures will be introduced only after
the lower-level operation they organize is understood.

Every implementation batch follows this sequence:

1. Explain one allocator concept in plain language.
2. Identify the corresponding C source and invariant.
3. Propose one Rust experiment or implementation.
4. Obtain approval before editing.
5. Keep the code change below 200 lines.
6. Run normal tests and AddressSanitizer tests.
7. Stop for review.
8. Commit only after approval.

Code changes are made on dedicated working branches. Nothing is pushed without
explicit permission.

## Project Goals

### Performance

The Rust implementation should eventually match mimalloc v3 in:

- local allocation and free throughput;
- cross-thread free throughput;
- multithreaded scalability;
- tail latency;
- peak resident memory;
- committed memory;
- fragmentation;
- metadata overhead.

The final local fast paths should preserve these properties:

```text
local allocation:
  no lock
  no atomic read-modify-write

local free:
  no lock
  no atomic read-modify-write

remote free:
  normally one page-local compare-and-exchange
```

### Safety

Unsafe Rust is expected for:

- OS virtual-memory operations;
- raw pointer arithmetic;
- intrusive free lists;
- uninitialized memory;
- address-derived page metadata;
- tagged atomic pointers;
- global allocator entry points.

Unsafe code should remain small and explain:

- why a pointer is valid;
- how many bytes are accessible;
- why alignment is sufficient;
- why aliasing rules are satisfied;
- when memory becomes initialized;
- which thread may mutate the state;
- which atomic operation publishes data.

Higher-level allocation policy should be safe Rust where possible.

### Rust API

The first public goal is use as Rust's global allocator.

Later Rust-native features should include:

- shared heaps;
- thread-local heaps;
- custom arenas;
- caller-provided memory;
- isolated allocation domains;
- NUMA selection;
- huge pages;
- statistics and profiling.

C and C++ API compatibility is not required initially.

## Current Crate

The rewrite starts as one library crate:

```text
mimalloc-rust/
  Cargo.toml
  rust-toolchain.toml
  README.md
  scripts/
    test-asan.sh
  src/
    lib.rs
```

We will split modules or crates only when a demonstrated ownership or safety
boundary makes the split useful.

The normal toolchain is stable Rust 1.98.0.

AddressSanitizer tests use the pinned nightly toolchain
`nightly-2026-09-18`, because Rust sanitizer instrumentation currently
requires an unstable compiler flag.

## Phase 0: OS Backing Memory

Status: **Complete**

Phase 0 establishes the layer below the allocator.

The demonstration test:

1. reserves one anonymous virtual-memory page with `mmap`;
2. initially gives it no access permissions;
3. makes it readable and writable with `mprotect`;
4. writes and reads every byte using a raw pointer;
5. releases it with `munmap`.

This corresponds to the Linux primitives used by mimalloc:

| Rust demonstration | mimalloc C |
|---|---|
| `mmap_anonymous` with no permissions | `_mi_prim_alloc` with `PROT_NONE` |
| `mprotect` with read/write | `_mi_prim_commit` |
| Raw pointer writes | Page and free-list initialization |
| `munmap` | `_mi_prim_free` |

Relevant C source:

- `src/prim/unix/prim.c:284-287`
- `src/prim/unix/prim.c:383-499`
- `src/prim/unix/prim.c:523-533`
- `src/prim/unix/prim.c:548-599`

Run the normal test:

```text
cargo test --manifest-path mimalloc-rust/Cargo.toml
```

Run the AddressSanitizer test:

```text
bash mimalloc-rust/scripts/test-asan.sh
```

Phase 0 does not yet implement an allocator. It only proves that Rust can
request and manipulate memory without calling another allocator.

## Phase 1: Fixed-Size Blocks in One Page

Status: **Next**

Start with one 4 KiB OS page and one fixed block size, such as 64 bytes.

The page contains:

```text
4096 bytes / 64 bytes = 64 blocks

+---------+---------+---------+-----+---------+
| block 0 | block 1 | block 2 | ... | block63 |
+---------+---------+---------+-----+---------+
```

The first experiment will:

- map one page;
- calculate a block address from a block index;
- verify block alignment;
- write different data into separate blocks;
- prove that block ranges do not overlap;
- unmap the page.

No free list is introduced yet.

The important invariant is:

```text
block_address = page_start + block_index * block_size

0 <= block_index < block_count
```

## Phase 2: Intrusive Free List

Status: **Pending**

A free block will store the index or pointer of the next free block inside its
own unused bytes.

```text
free block:
+----------------+---------------------------+
| next free link | unused block bytes        |
+----------------+---------------------------+

allocated block:
+--------------------------------------------+
| user data                                  |
+--------------------------------------------+
```

The experiment will support:

- initializing a free list;
- popping one block;
- pushing one block back;
- detecting the empty-list sentinel;
- verifying that each block appears exactly once.

This phase is single-threaded.

## Phase 3: Single-Threaded Page

Status: **Pending**

Combine the mapped memory and intrusive free list into one page abstraction.

The page will track:

- block size;
- total block count;
- free-list head;
- allocated block count.

The page will support:

```text
allocate one block
free one block
detect full
detect empty
```

The accounting invariant is:

```text
allocated blocks + free blocks = total blocks
```

## Phase 4: Mimalloc Size Classes

Status: **Pending**

Replace the single 64-byte block size with mimalloc's size-class calculation.

The Rust implementation will be compared directly with:

- `include/mimalloc/types.h`
- `src/page-queue.c`
- the C bin and block-size calculations.

Every supported request size must select the expected block size and page
kind.

## Phase 5: Multiple Pages and Page Queues

Status: **Pending**

Introduce multiple pages for one size class.

Learn:

- how a thread selects an active page;
- how full pages leave the active path;
- why mimalloc prefers fuller pages;
- how an empty page becomes releasable.

No cross-thread free is introduced yet.

## Phase 6: Arena Slices

Status: **Pending**

Replace one-off page mappings with large OS reservations divided into 64 KiB
slices.

Introduce:

- arena reservation;
- slice indexes;
- free-slice bitmaps;
- page construction from slices;
- page return to an arena.

## Phase 7: Thread-Local Heap

Status: **Pending**

Give each thread its own page queues.

The local path should become:

```text
thread-local heap
  -> size-class queue
    -> active page
      -> free block
```

## Phase 8: Cross-Thread Free

Status: **Pending**

Add the three-list page model:

```text
free
local_free
xthread_free
```

The owner uses non-atomic local state. A remote thread pushes onto the atomic
remote list.

## Phase 9: Abandonment and Reclamation

Status: **Pending**

Handle pages whose owning thread exits while blocks remain live.

Introduce:

- abandoned page state;
- the page ownership bit;
- abandoned-page indexes;
- reclaim on allocation;
- reclaim on remote free.

## Phase 10: Rust Global Allocator

Status: **Pending**

Only after page ownership and reclamation work will the crate implement
Rust's global allocator interface.

The public implementation should be a thin adapter over the allocator engine.

## Phase 11: Performance and Memory Policy

Status: **Pending**

Add and tune:

- incremental commitment;
- page retirement;
- delayed purging;
- full-page retention;
- page candidate limits;
- large and singleton pages;
- alignment handling;
- benchmark comparisons with C mimalloc.

## Later Features

After the global allocator reaches parity:

- first-class heaps;
- thread-local heap handles;
- public arenas;
- isolated domains;
- NUMA;
- huge pages;
- statistics;
- profiling;
- guarded and security modes;
- Windows and macOS platform layers;
- optional nightly allocator-aware container support.

## Testing Rules

Every unsafe memory operation needs:

- a focused normal test;
- an AddressSanitizer run;
- boundary cases;
- a written safety explanation.

Later concurrency protocols also need:

- deterministic state-machine tests;
- stress tests;
- thread sanitizer or concurrency-model tests where practical.

The sanitizer does not prove correctness. It supplements invariants, code
review, and targeted tests.

## Review and Git Rules

- Use a separate working branch for code changes.
- Keep every code batch below 200 lines.
- Explain the concept before editing.
- Wait for approval before each code batch.
- Stop after each batch for review.
- Commit only approved changes.
- Do not push without explicit permission.

## Decision Log

### September 19, 2026

- Renamed the Rust directory and package to `mimalloc-rust`.
- Replaced the multi-crate workspace with one minimal crate.
- Changed the implementation order from top-down to bottom-up.
- Chose explanation-first reviews.
- Added a direct OS-memory mapping demonstration.
- Added AddressSanitizer testing on a pinned nightly toolchain.
- Kept stable Rust as the normal development toolchain.
- Selected `main3-rust-rewrite` as the base feature branch.
