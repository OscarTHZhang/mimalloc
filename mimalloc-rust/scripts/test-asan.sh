#!/usr/bin/env bash

set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
crate_root="$(cd -- "${script_dir}/.." && pwd)"

toolchain="nightly-2026-09-18"
target="x86_64-unknown-linux-gnu"

export RUSTFLAGS="-Zsanitizer=address -Cforce-frame-pointers=yes"
export RUSTDOCFLAGS="${RUSTFLAGS}"
export ASAN_OPTIONS="${ASAN_OPTIONS:-detect_leaks=1:halt_on_error=1}"

cargo "+${toolchain}" test \
  -Zbuild-std \
  --target "${target}" \
  --manifest-path "${crate_root}/Cargo.toml"
