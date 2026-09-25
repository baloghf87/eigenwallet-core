# swap-market-data

A headless service that autonomously connects to the eigenwallet XMR/BTC
atomic-swap network, discovers makers, continuously polls their quotes, and
serves the **current liquidity snapshot** (a synthetic orderbook) over a small
REST API. It is designed to run as a container in Kubernetes.

It is **stateless**: it keeps only the latest snapshot in memory and does no
persistence. Downstream components build any history / candlesticks by polling
this API over time.

## What this data is — and what it is not

This section is the short version of the feasibility study that produced this
service; see [`API.md`](./API.md) for the consumer-facing detail.

- There is exactly **one market: `XMR/BTC`**, and it is **one-directional**:
  makers sell XMR for BTC (takers bring BTC). There is no other pair and no bid
  side in the usual sense.
- Each maker advertises **one quote** — a single price plus a `[min, max]` BTC
  size band, **not** a book of discrete price levels. The "orderbook" is the
  aggregation of every maker's single quote.
- Quotes are **indicative**: refreshed on the maker's poll interval (~45s) and
  expired from the cache after ~180s. They are **not** resting limit orders and
  **not** proof that any trade happened.
- `max_quantity` is bounded by the maker's available Monero inventory, so it is
  the main depth signal; an optional Monero `reserve_proof` cryptographically
  backs the quote.
- **There is no executed-trades / historical-fills data available from the
  network.** Swaps are negotiated privately peer-to-peer, and their on-chain legs
  (a generic 2-of-2 P2WSH on Bitcoin, stealth/RingCT on Monero) are unlinkable and
  unpriced to an outside observer. A real trades tape therefore cannot be derived
  from the network. Anything candlestick-like a downstream component builds by
  polling this API is a history **of quotes**, not of trades.

## How it works

```
rendezvous nodes ──discover──▶ makers ──poll BidQuote──▶ in-memory snapshot ──▶ REST API
```

1. Connects to the network's public rendezvous nodes (or an override) and
   discovers every registered maker.
2. Continuously polls each maker for its `BidQuote` and caches the latest one per
   maker, expiring stale entries.
3. Publishes each updated snapshot to the HTTP handlers via a `watch` channel, so
   reads are always current and lock-free.

It reuses the network stack that the GUI/CLI already use, rather than
reimplementing it:

- `swap_p2p::protocols::rendezvous::discovery` — maker discovery.
- `swap_p2p::protocols::quotes_cached` — polling + per-maker quote cache +
  snapshot events.
- A `wss` / `tcp` / `dns` (+ optional Tor) transport, adapted from the production
  CLI transport (`swap/src/cli/transport.rs`), so it can actually reach the
  clearnet `wss` rendezvous addresses.

Source layout:

- `src/main.rs` — CLI/env config, swarm setup, event loop → `watch` channel, HTTP server.
- `src/transport.rs` — the libp2p transport.
- `src/api.rs` — response types, handlers, router, and unit tests.

## API

| Endpoint | Description |
|---|---|
| `GET /orderbook` | Current snapshot with summary (best price, total depth), sorted best-first. |
| `GET /quotes` | Raw per-maker rows, unsorted. |
| `GET /healthz` | Liveness. |
| `GET /readyz` | Readiness — 200 once the first quote is received, else 503. |

Full request/response schemas, field semantics, and examples are in
[`API.md`](./API.md) (written to be handed to a consuming service or agent).

## Configuration

Flags or environment variables:

| Flag | Env var | Default | Description |
|---|---|---|---|
| `--host` | `MARKET_DATA_HOST` | `0.0.0.0` | HTTP bind address. |
| `--port` | `MARKET_DATA_PORT` | `8080` | HTTP bind port. |
| `--testnet` | `MARKET_DATA_TESTNET` | `false` | Use the testnet rendezvous namespace. |
| `--tor` | `MARKET_DATA_TOR` | `false` | Route over Tor (uses onion rendezvous addresses). |
| `--socks-proxy` | `MARKET_DATA_SOCKS_PROXY` | unset | Route **every** connection (clearnet and onion) through an external SOCKS5 proxy `host:port`, e.g. a shared tor daemon, instead of the embedded Tor. Excludes `--tor`. |
| `--rendezvous` | `MARKET_DATA_RENDEZVOUS` | built-in public nodes | Comma-separated multiaddr override. |

Log level via `RUST_LOG` (e.g. `RUST_LOG=info,swap_p2p=debug`).

When `--tor` is off, only clearnet (`wss`) rendezvous addresses are used; when
on, only `/onion3` addresses are used. With `--socks-proxy` all of them are used:
host names and onion addresses are resolved by the proxy (SOCKS5 domain
requests, no local DNS), see `src/socks.rs`.

## Building

The crate depends on `swap-p2p`, which transitively pulls in `monero-sys` (a C++
build). So a build needs the Monero submodules and the monero build toolchain:

```bash
git submodule update --init --recursive   # or: just update_submodules
cargo build -p swap-market-data --bin market-data
```

The first build compiles `monero-sys` and is slow; subsequent builds are cached.
Never run `cargo clean` (it forces a full monero-sys rebuild).

The Docker image (below) bundles the toolchain, so building the image needs no
local Rust/Monero setup — just Docker.

## Testing against the main network

This is **read-only and needs no funds**: the service only connects and reads
maker quotes; it never negotiates or executes a swap.

### One command (recommended)

From a machine with Docker and normal internet access:

```bash
just market-data-smoke
```

This builds the image, runs it against **mainnet**, waits for it to connect and
receive its first quote, prints the current orderbook, and cleans up. Target
testnet with `just market-data-smoke --testnet`, or route over Tor with
`just market-data-smoke --tor`. The underlying script is
[`scripts/smoke-test.sh`](./scripts/smoke-test.sh) (configurable via env vars —
`PORT`, `READY_TIMEOUT`, `SKIP_BUILD`, etc.).

### Manually

```bash
# Build + run against mainnet (the default):
cargo run -p swap-market-data --bin market-data
# (or: RUST_LOG=info,swap_p2p=debug cargo run -p swap-market-data --bin market-data)

# In another shell:
curl -sf localhost:8080/readyz          # 200 once the first quote arrives (can take tens of seconds)
curl -s  localhost:8080/orderbook | jq  # expect maker_count > 0, prices, best_price_sat, ...
curl -s  localhost:8080/quotes | jq
```

Cross-check the maker count against the in-repo scraper:

```bash
cargo run -p swap-p2p --example fetch_quotes
```

Notes:
- **Privacy**: clearnet connections reveal the host's IP to the rendezvous nodes
  and makers. Use `--tor` to route over Tor.
- **Empty orderbook** usually means outbound access to the `wss` rendezvous hosts
  is blocked, or discovery has not completed yet — give it time and check
  `RUST_LOG` output.

## Docker & Kubernetes

```bash
git submodule update --init --recursive
docker build -f swap-market-data/Dockerfile -t swap-market-data .
docker run -p 8080:8080 swap-market-data
```

The Dockerfile mirrors `swap-asb/Dockerfile` (Ubuntu + cargo-chef) because of the
`monero-sys` dependency. An example Kubernetes Deployment + Service (with liveness
and readiness probes wired to `/healthz` and `/readyz`) is in
[`deploy/k8s.yaml`](./deploy/k8s.yaml).

## Verification status

- Built (`docker build -f swap-market-data/Dockerfile .`, and `cargo build` in the
  Dockerfile's toolchain image) and unit-tested (`cargo test -p swap-market-data`).
- **Verified against mainnet (2026-09-25)** through an external tor via
  `--socks-proxy`: 24 makers (13 with liquidity), reached over clearnet `wss`
  and `/onion3`, best ask ≈ 0.0067 BTC/XMR.
- **Fixed: maker discovery never worked on mainnet** (in clearnet, Tor and SOCKS
  mode alike): discover requests had no limit, so the rendezvous nodes answered
  with every registration of the namespace in one response, which exceeds
  libp2p-rendezvous' 1 MiB message cap. The failure surfaces only as
  `DiscoverFailed { error: Unavailable }`. `swap-p2p`'s discovery now requests
  pages of 25 and continues with the returned cookie (which also makes the
  periodic refresh incremental). This affects the CLI/GUI discovery too.

## Known limitations & possible follow-ups

- Only a single price + size band per maker is available (no per-maker depth
  curve), because that is all the network's quote protocol exposes.
- To produce a much lighter image, `swap-machine`/`monero-sys` could be made an
  optional dependency of `swap-p2p` (the quote/discovery path does not use it),
  letting the collector build without the C++ toolchain. This is an invasive
  change to a shared crate and is intentionally left out of the initial version.
