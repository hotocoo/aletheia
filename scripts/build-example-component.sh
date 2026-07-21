#!/usr/bin/env bash
# Build the example Aletheia component (examples/hello-component) with the component SDK to a wasm32
# guest, and refresh the committed test fixture the hosted acceptance suite runs.
#
#   examples/hello-component (+ component-sdk)  ->  wasm32-unknown-unknown  ->  hello_component.wasm
#   -> aletheia/tests/fixtures/hello_component.wasm  (include_bytes! by tests/sdk_component.rs)
#
# The fixture is committed so the hosted `cargo test` stays green with NO wasm toolchain dependency;
# regenerate it (this script) whenever the SDK or the example changes. Needs: rustup target
# wasm32-unknown-unknown (`rustup target add wasm32-unknown-unknown`).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
EXAMPLE="$ROOT/examples/hello-component"
TARGET="wasm32-unknown-unknown"
ARTIFACT="$EXAMPLE/target/$TARGET/release/hello_component.wasm"
FIXTURE_DIR="$ROOT/aletheia/tests/fixtures"
FIXTURE="$FIXTURE_DIR/hello_component.wasm"

command -v rustup >/dev/null 2>&1 || { echo "error: rustup not on PATH"; exit 1; }
rustup target list --installed | grep -q "$TARGET" || {
  echo "error: target $TARGET not installed — run: rustup target add $TARGET"; exit 1; }

echo "==> building examples/hello-component for $TARGET (release)"
( cd "$EXAMPLE" && cargo build --release --target "$TARGET" )
[ -f "$ARTIFACT" ] || { echo "error: expected artifact missing: $ARTIFACT"; exit 1; }

echo "==> refreshing fixture: $FIXTURE"
mkdir -p "$FIXTURE_DIR"
cp "$ARTIFACT" "$FIXTURE"
echo "    $(wc -c < "$FIXTURE") bytes"
echo "done. Run: cargo test --manifest-path aletheia/Cargo.toml --test sdk_component"
