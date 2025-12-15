#!/bin/bash
# Script to run the simple_test example under Ferros via the launch command

set -e

# Get the script directory
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
# Build the example binary (debug profile)
echo "Building simple_test example..."
cd "$PROJECT_ROOT"
cargo build --example simple_test --package ferros

# Path to the built example binary
EXAMPLE_BIN="$PROJECT_ROOT/target/debug/examples/simple_test"

# Launch the example under Ferros (launch starts it suspended, Ferros owns its lifetime)
echo "Launching simple_test under Ferros..."
sudo FERROS_CONFIG="$PROJECT_ROOT/.config/ferros/config.toml" cargo run --package ferros "$EXAMPLE_BIN"
