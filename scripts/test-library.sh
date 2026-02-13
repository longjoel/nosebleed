#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
CORE_PATH="${CORE_PATH:-${ROOT_DIR}/test-core.so}"
ROM_PATH="${ROM_PATH:-${ROOT_DIR}/test-rom.nes}"
LISTEN_ADDR="${LISTEN_ADDR:-127.0.0.1:8080}"
HEALTH_URL="http://${LISTEN_ADDR}/healthz"
LOG_FILE="${LOG_FILE:-${ROOT_DIR}/target/test-library.log}"
MODE="${1:-smoke}"
ENABLE_WEBRTC_VP8="${ENABLE_WEBRTC_VP8:-0}"
WEBRTC_VIDEO_ENCODER="${NOSEBLEED_WEBRTC_VIDEO_ENCODER:-}"
WEBRTC_VIDEO_ENCODER_ARGS="${NOSEBLEED_WEBRTC_VIDEO_ENCODER_ARGS:-}"
WEBRTC_VIDEO_BITRATE_KBPS="${NOSEBLEED_WEBRTC_VIDEO_BITRATE_KBPS:-}"
FFMPEG_BIN="${NOSEBLEED_FFMPEG_BIN:-}"

if [[ "${ENABLE_WEBRTC_VP8}" == "1" ]]; then
  if [[ -z "${WEBRTC_VIDEO_ENCODER}" ]]; then
    WEBRTC_VIDEO_ENCODER="vp8_vaapi"
  fi
  if [[ -z "${WEBRTC_VIDEO_ENCODER_ARGS}" ]]; then
    WEBRTC_VIDEO_ENCODER_ARGS="-vaapi_device /dev/dri/renderD128 -vf format=nv12,hwupload"
  fi
fi

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
ENV_ARGS=()
if [[ -n "${FFMPEG_BIN}" ]]; then
  ENV_ARGS+=("NOSEBLEED_FFMPEG_BIN=${FFMPEG_BIN}")
fi
if [[ -n "${WEBRTC_VIDEO_ENCODER}" ]]; then
  ENV_ARGS+=("NOSEBLEED_WEBRTC_VIDEO_ENCODER=${WEBRTC_VIDEO_ENCODER}")
fi
if [[ -n "${WEBRTC_VIDEO_ENCODER_ARGS}" ]]; then
  ENV_ARGS+=("NOSEBLEED_WEBRTC_VIDEO_ENCODER_ARGS=${WEBRTC_VIDEO_ENCODER_ARGS}")
fi
if [[ -n "${WEBRTC_VIDEO_BITRATE_KBPS}" ]]; then
  ENV_ARGS+=("NOSEBLEED_WEBRTC_VIDEO_BITRATE_KBPS=${WEBRTC_VIDEO_BITRATE_KBPS}")
fi

if [[ ${#ENV_ARGS[@]} -gt 0 ]]; then
  printf 'webrtc video env:'
  for arg in "${ENV_ARGS[@]}"; do
    printf ' %q' "${arg}"
  done
  printf '\n'
fi

env "${ENV_ARGS[@]}" cargo run -p nosebleed -- --listen "${LISTEN_ADDR}" --core "${CORE_PATH}" --content "${ROM_PATH}" >"${LOG_FILE}" 2>&1 &
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
  echo "for WebRTC VP8, open http://${LISTEN_ADDR}/?transport=webrtc"
  wait "${SERVER_PID}"
  exit $?
fi

# Default mode (smoke): exit successfully after startup verification.
