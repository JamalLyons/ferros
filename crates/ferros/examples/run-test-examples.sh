#!/bin/bash
# Test script for Ferros debugger examples
# This script runs multiple test examples against a target process
#
# Usage: ./run-test-examples.sh
#
# This script will:
# 1. Start hello_target in the background
# 2. Extract the test variable address from its output
# 3. Run hello_ptrace example (register reading)
# 4. Run memory_inspector example (memory read/write)
# 5. Clean up the target process

echo "🧪 Testing Ferros Debugger Examples"
echo "===================================="
echo ""

# Step 1: Start hello_target in background
echo "Step 1: Starting hello_target..."
cargo run --example hello_target > /tmp/hello_target.log 2>&1 &
TARGET_PID=$!
echo "✅ Started hello_target with PID: $TARGET_PID"
echo ""

# Step 2: Wait a moment for it to start
echo "Step 2: Waiting for target to initialize..."
sleep 2

# Step 3: Verify it's running
if ! ps -p $TARGET_PID > /dev/null 2>&1; then
    echo "❌ Error: hello_target process exited immediately"
    echo "Log output:"
    cat /tmp/hello_target.log
    exit 1
fi

echo "Step 3: Verifying process exists..."
ps -p $TARGET_PID
echo ""

# Step 4: Extract variable address from log
echo "Step 4: Extracting test variable address..."
VARIABLE_ADDRESS=$(grep "TEST_VARIABLE address:" /tmp/hello_target.log | grep -oE '0x[0-9a-fA-F]+' | head -1)

if [ -z "$VARIABLE_ADDRESS" ]; then
    echo "⚠️  Warning: Could not extract variable address from log"
    echo "Log output:"
    cat /tmp/hello_target.log
    VARIABLE_ADDRESS=""
else
    echo "✅ Found variable address: $VARIABLE_ADDRESS"
fi
echo ""

# Step 5: Run hello_ptrace example
echo "Step 5: Testing hello_ptrace (register reading)..."
echo "Note: You'll need to enter your password for sudo"
echo "----------------------------------------"
sudo cargo run --example hello_ptrace $TARGET_PID 2>&1 | head -40
echo ""

# Step 6: Run memory_inspector example
echo "Step 6: Testing memory_inspector (memory read/write)..."
echo "Note: You'll need to enter your password for sudo"
echo "----------------------------------------"
if [ -n "$VARIABLE_ADDRESS" ]; then
    sudo cargo run --example memory_inspector $TARGET_PID $VARIABLE_ADDRESS 2>&1 | head -50
else
    echo "⚠️  Skipping memory read/write test (no variable address)"
    sudo cargo run --example memory_inspector $TARGET_PID 2>&1 | head -30
fi
echo ""

# Step 7: Cleanup
echo "Step 7: Cleaning up..."
kill $TARGET_PID 2>/dev/null || true
sleep 1

# Verify cleanup
if ps -p $TARGET_PID > /dev/null 2>&1; then
    echo "⚠️  Process still running, force killing..."
    kill -9 $TARGET_PID 2>/dev/null || true
fi

echo "✅ All tests completed!"
echo ""
echo "📝 Summary:"
echo "  - hello_ptrace: Register reading test"
if [ -n "$VARIABLE_ADDRESS" ]; then
    echo "  - memory_inspector: Memory read/write test with address $VARIABLE_ADDRESS"
else
    echo "  - memory_inspector: Memory region enumeration only (no address provided)"
fi

