#!/usr/bin/env bash
#
# Smoke-test the market-data service against the live network.
#
# Builds the Docker image, runs it (against MAINNET by default), waits until it
# reports ready, prints the current orderbook, then cleans up. This is read-only:
# it only connects and reads maker quotes — it never runs a swap and needs no funds.
#
# Requires: docker, curl (jq optional, for pretty output). Run from anywhere in
# the repo; it builds from the repository root.
#
# Environment variables (all optional):
#   PORT             Host port to expose (default 8080)
#   IMAGE            Docker image tag (default swap-market-data:smoke)
#   CONTAINER        Container name (default market-data-smoke)
#   READY_TIMEOUT    Seconds to wait for readiness (default 120)
#   RUST_LOG         Log level inside the container (default info)
#   MARKET_DATA_ARGS Extra args passed to the binary, e.g. "--testnet" or "--tor"
#   SKIP_BUILD=1     Reuse an existing image instead of rebuilding
#   SKIP_SUBMODULES=1  Do not run `git submodule update` first
#
# Examples:
#   ./swap-market-data/scripts/smoke-test.sh            # test mainnet
#   MARKET_DATA_ARGS=--testnet ./swap-market-data/scripts/smoke-test.sh

set -euo pipefail

PORT="${PORT:-8080}"
IMAGE="${IMAGE:-swap-market-data:smoke}"
CONTAINER="${CONTAINER:-market-data-smoke}"
READY_TIMEOUT="${READY_TIMEOUT:-120}"
RUST_LOG="${RUST_LOG:-info}"
MARKET_DATA_ARGS="${MARKET_DATA_ARGS:-}"

# Resolve the repository root (this script lives in swap-market-data/scripts/).
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

log() { printf '\033[1;34m[smoke]\033[0m %s\n' "$*"; }
err() { printf '\033[1;31m[smoke]\033[0m %s\n' "$*" >&2; }

command -v docker >/dev/null || { err "docker is required"; exit 1; }
command -v curl >/dev/null || { err "curl is required"; exit 1; }

cleanup() {
    docker stop "$CONTAINER" >/dev/null 2>&1 || true
}
trap cleanup EXIT

if [[ "${SKIP_SUBMODULES:-}" != "1" ]]; then
    log "Ensuring monero-sys submodules are present..."
    git submodule update --init --recursive
fi

if [[ "${SKIP_BUILD:-}" != "1" ]]; then
    log "Building image '$IMAGE' (first build compiles monero-sys and is slow)..."
    docker build -f swap-market-data/Dockerfile -t "$IMAGE" .
fi

log "Starting container '$CONTAINER' on port $PORT (args: ${MARKET_DATA_ARGS:-<none>})..."
docker stop "$CONTAINER" >/dev/null 2>&1 || true
# shellcheck disable=SC2086  # intentional word-splitting of MARKET_DATA_ARGS
docker run -d --rm \
    --name "$CONTAINER" \
    -p "$PORT:8080" \
    -e "RUST_LOG=$RUST_LOG" \
    "$IMAGE" $MARKET_DATA_ARGS >/dev/null

base_url="http://localhost:$PORT"

log "Waiting up to ${READY_TIMEOUT}s for the service to connect and receive a quote..."
ready=""
for ((i = 0; i < READY_TIMEOUT; i++)); do
    if curl -sf -o /dev/null "$base_url/readyz"; then
        ready="yes"
        break
    fi
    # Fail fast if the container died.
    if ! docker ps --format '{{.Names}}' | grep -qx "$CONTAINER"; then
        err "Container exited early. Logs:"
        docker logs "$CONTAINER" 2>&1 | tail -n 50 >&2 || true
        exit 1
    fi
    sleep 1
done

if [[ -z "$ready" ]]; then
    err "Service did not become ready within ${READY_TIMEOUT}s. Recent logs:"
    docker logs "$CONTAINER" 2>&1 | tail -n 50 >&2 || true
    err "If the orderbook stays empty, check outbound access to the wss rendezvous hosts."
    exit 1
fi

log "Service is ready. Fetching /orderbook ..."
orderbook="$(curl -s "$base_url/orderbook")"

if command -v jq >/dev/null; then
    echo "$orderbook" | jq .
    maker_count="$(echo "$orderbook" | jq -r '.maker_count')"
    with_liq="$(echo "$orderbook" | jq -r '.makers_with_liquidity')"
    best="$(echo "$orderbook" | jq -r '.best_price_sat')"
    log "Summary: maker_count=$maker_count makers_with_liquidity=$with_liq best_price_sat=$best"
else
    echo "$orderbook"
    log "(install jq for a summary and pretty output)"
fi

log "Smoke test finished. Stopping container."
