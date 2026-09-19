# mimalloc-rs

This directory contains the Rust rewrite of mimalloc v3.

The implementation roadmap is in
[`../doc/mimalloc-rust-rewrite-plan.md`](../doc/mimalloc-rust-rewrite-plan.md).
The C architecture reference is in
[`../doc/mimalloc-v3-architecture.md`](../doc/mimalloc-v3-architecture.md).

## Current State

The rewrite starts as one minimal library crate. Allocator structures and
behavior will be introduced one reviewable concept at a time.

The crate currently contains no allocator implementation and no unsafe code.

## Build

```text
cargo test --manifest-path mimalloc-rust/Cargo.toml
```

## AddressSanitizer

Install the pinned nightly toolchain once:

```text
rustup toolchain install nightly-2026-09-18 \
  --profile minimal \
  --component rust-src
```

Run all tests with AddressSanitizer instrumentation:

```text
bash mimalloc-rust/scripts/test-asan.sh
```

The normal crate toolchain remains stable. Nightly is used only for sanitizer
testing because Rust's sanitizer compiler flag is currently unstable.
