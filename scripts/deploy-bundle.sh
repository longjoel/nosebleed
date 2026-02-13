#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
if [[ "${1:-}" == "--" ]]; then
  shift
fi
DEPLOY_DIR="${1:-${DEPLOY_DIR:-${ROOT_DIR}/dist/nosebleed}}"
BIN_SRC="${ROOT_DIR}/target/release/nosebleed"
CONFIG_SRC="${ROOT_DIR}/apps/nosebleed/nosebleed.config.json.example"
STATIC_SRC="${ROOT_DIR}/apps/nosebleed/static"
SDK_DIST_SRC="${ROOT_DIR}/packages/player-sdk/dist"

if [[ ! -x "${BIN_SRC}" ]]; then
  echo "missing release binary: ${BIN_SRC}" >&2
  echo "run: pnpm build:app" >&2
  exit 1
fi

mkdir -p "${DEPLOY_DIR}/bin" \
  "${DEPLOY_DIR}/config" \
  "${DEPLOY_DIR}/static" \
  "${DEPLOY_DIR}/metadata" \
  "${DEPLOY_DIR}/sessions"

install -m 755 "${BIN_SRC}" "${DEPLOY_DIR}/bin/nosebleed"
install -m 644 "${CONFIG_SRC}" "${DEPLOY_DIR}/config/nosebleed.config.example.json"
cp -a "${STATIC_SRC}/." "${DEPLOY_DIR}/static/"

if [[ -d "${SDK_DIST_SRC}" ]]; then
  mkdir -p "${DEPLOY_DIR}/web/player-sdk"
  cp -a "${SDK_DIST_SRC}/." "${DEPLOY_DIR}/web/player-sdk/"
fi

cat > "${DEPLOY_DIR}/run.sh" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
LISTEN_ADDR="${LISTEN_ADDR:-0.0.0.0:8080}"
CONFIG_PATH="${NOSEBLEED_CONFIG:-${SCRIPT_DIR}/config/nosebleed.config.json}"

exec "${SCRIPT_DIR}/bin/nosebleed" --listen "${LISTEN_ADDR}" --config "${CONFIG_PATH}"
EOF
chmod +x "${DEPLOY_DIR}/run.sh"

BUILD_TIME_UTC="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"
GIT_COMMIT="unknown"
if git -C "${ROOT_DIR}" rev-parse --verify HEAD >/dev/null 2>&1; then
  GIT_COMMIT="$(git -C "${ROOT_DIR}" rev-parse --short HEAD)"
fi

cat > "${DEPLOY_DIR}/config/nosebleed.config.json" <<'EOF'
{
  "listen": "0.0.0.0:8080",
  "fps": 60,
  "width": 320,
  "height": 240,
  "require_auth": false,
  "reconnect_window_ms": 15000,
  "session": {
    "root_dir": "./sessions",
    "id": "match-123",
    "copy_core": false,
    "copy_content": false
  }
}
EOF

cat > "${DEPLOY_DIR}/metadata/manifest.txt" <<EOF
built_at_utc=${BUILD_TIME_UTC}
git_commit=${GIT_COMMIT}
binary=bin/nosebleed
config=config/nosebleed.config.json
config_example=config/nosebleed.config.example.json
static=static/
sdk=web/player-sdk/
EOF

echo "bundle ready: ${DEPLOY_DIR}"
