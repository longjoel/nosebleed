#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
CORE_PATH="${CORE_PATH:-${ROOT_DIR}/test-core.so}"
ROM_PATH="${ROM_PATH:-${ROOT_DIR}/test-rom.nes}"
LISTEN_ADDR="${LISTEN_ADDR:-127.0.0.1:8080}"
HEALTH_URL="http://${LISTEN_ADDR}/healthz"
LOG_FILE="${LOG_FILE:-${ROOT_DIR}/target/test-library.log}"
MODE="${1:-smoke}"

if [[ ! -f "${CORE_PATH}" ]]; then
  echo "missing core: ${CORE_PATH}" >&2
  exit 1
fi

if [[ ! -f "${ROM_PATH}" ]]; then
  echo "missing rom: ${ROM_PATH}" >&2
  exit 1
fi

mkdir -p "$(dirname -- "${LOG_FILE}")"

echo "starting nosebleed with core=${CORE_PATH} rom=${ROM_PATH} listen=${LISTEN_ADDR}"
cargo run -- --listen "${LISTEN_ADDR}" --core "${CORE_PATH}" --content "${ROM_PATH}" >"${LOG_FILE}" 2>&1 &
SERVER_PID=$!

cleanup() {
  if kill -0 "${SERVER_PID}" >/dev/null 2>&1; then
    kill "${SERVER_PID}" >/dev/null 2>&1 || true
    wait "${SERVER_PID}" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT INT TERM

for _ in $(seq 1 100); do
  if curl -fsS "${HEALTH_URL}" >/dev/null 2>&1; then
    break
  fi
  sleep 0.1
done

if ! curl -fsS "${HEALTH_URL}" >/dev/null 2>&1; then
  echo "health check failed: ${HEALTH_URL}" >&2
  tail -n 40 "${LOG_FILE}" >&2 || true
  exit 1
fi

echo "health check passed: ${HEALTH_URL}"
echo "log file: ${LOG_FILE}"

if [[ "${MODE}" == "run" ]]; then
  echo "server running. open http://${LISTEN_ADDR}/ and press Ctrl+C to stop."
  wait "${SERVER_PID}"
  exit $?
fi

# Default mode (smoke): exit successfully after startup verification.
