#!/bin/sh
# Source this file to put the workspace-local Rust toolchain on PATH.
# The toolchain is installed under .rustup/.cargo (gitignored) so no
# system-wide Rust installation is required.
SCRIPT="${BASH_SOURCE[0]:-$0}"
ROOT="$(CDPATH= cd -- "$(dirname -- "$SCRIPT")/.." && pwd)"
export RUSTUP_HOME="$ROOT/.rustup"
export CARGO_HOME="$ROOT/.cargo"
export PATH="$CARGO_HOME/bin:$PATH"
