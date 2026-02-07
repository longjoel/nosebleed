#!/usr/bin/env bash
set -euo pipefail

# Start nosebleed demo: X11 on 127.0.0.1:6000 and HTTP UI on 127.0.0.1:8080.
NOSEBLEED_DEMO=1 RUST_LOG=info cargo run --quiet -- \
  >/tmp/nosebleed-server.log 2>&1 &
SERVER_PID=$!

echo "nosebleed server pid: $SERVER_PID"

echo "Waiting 1s for server startup..."
sleep 1

echo "Starting bounce example client..."
cargo run --quiet --example bounce >/tmp/nosebleed-bounce.log 2>&1 &
CLIENT_PID=$!

echo "bounce client pid: $CLIENT_PID"

echo "Open http://127.0.0.1:8080 to view the framebuffer (polling)"
echo "Press Ctrl+C to stop."
trap 'kill $CLIENT_PID 2>/dev/null || true; kill $SERVER_PID 2>/dev/null || true' INT TERM
wait $CLIENT_PID
kill $SERVER_PID 2>/dev/null || true
