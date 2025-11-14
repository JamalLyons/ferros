#!/bin/bash
# Test script for the debugger attachment

echo "🧪 Testing Ferros Debugger Attachment"
echo "======================================"
echo ""

# Step 1: Start hello_target in background
echo "Step 1: Starting hello_target..."
cargo run --example hello_target > /tmp/hello_target.log 2>&1 &
TARGET_PID=$!
echo "✅ Started hello_target with PID: $TARGET_PID"
echo ""

# Step 2: Wait a moment for it to start
sleep 2

# Step 3: Verify it's running
if ! ps -p $TARGET_PID > /dev/null 2>&1; then
    echo "❌ Error: hello_target process exited immediately"
    echo "Log output:"
    cat /tmp/hello_target.log
    exit 1
fi

echo "Step 2: Verifying process exists..."
ps -p $TARGET_PID
echo ""

# Step 4: Run the debugger
echo "Step 4: Attempting attachment (with sudo)..."
echo "Note: You'll need to enter your password"
sudo cargo run --example hello_ptrace $TARGET_PID 2>&1 | head -30
echo ""

# Step 5: Cleanup
echo "Step 5: Cleaning up..."
kill $TARGET_PID 2>/dev/null || true
echo "✅ Done"

