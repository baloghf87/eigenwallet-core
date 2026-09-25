# swap-market-data

A headless service that autonomously connects to the eigenwallet XMR/BTC
atomic-swap network, discovers makers, continuously polls their quotes, and
serves the **current liquidity snapshot** (a synthetic orderbook) over a small
REST API.

It is **stateless**: it holds only the latest snapshot in memory and does no
persistence. Historical series / candlesticks are expected to be built by
downstream components that poll this API over time. Note that only *quote*
history can be derived this way — the network exposes no executed-trades data
(see [`API.md`](./API.md) for the full rationale).

## API

See [`API.md`](./API.md). In short:

- `GET /orderbook` — current snapshot with summary (best price, total depth), sorted best-first.
- `GET /quotes` — raw per-maker rows.
- `GET /healthz` — liveness.
- `GET /readyz` — readiness (ready once the first quote is received).

## Running

```bash
# Clearnet (default): connects to the network's public rendezvous nodes over wss.
cargo run -p swap-market-data --bin market-data

# Then:
curl http://localhost:8080/orderbook
```

Configuration (flags or env vars):

| Flag | Env var | Default |
|---|---|---|
| `--host` | `MARKET_DATA_HOST` | `0.0.0.0` |
| `--port` | `MARKET_DATA_PORT` | `8080` |
| `--testnet` | `MARKET_DATA_TESTNET` | `false` |
| `--tor` | `MARKET_DATA_TOR` | `false` |
| `--rendezvous` | `MARKET_DATA_RENDEZVOUS` | built-in public nodes |

Log level via `RUST_LOG` (e.g. `RUST_LOG=info,swap_p2p=debug`).

## Docker

The build pulls in `monero-sys` (a C++ dependency) transitively via `swap-p2p`,
so the image mirrors `swap-asb/Dockerfile` (Ubuntu + cargo-chef). Build from the
repository root with submodules checked out:

```bash
git submodule update --init --recursive
docker build -f swap-market-data/Dockerfile -t swap-market-data .
docker run -p 8080:8080 swap-market-data
```

An example Kubernetes Deployment + Service is in [`deploy/k8s.yaml`](./deploy/k8s.yaml).
