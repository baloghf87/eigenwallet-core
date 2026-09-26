# Market Data API

REST API exposing the **current liquidity snapshot** of the eigenwallet XMR/BTC
atomic-swap network. The service (`market-data`) connects to the network's
rendezvous nodes, discovers makers, continuously polls each maker for its quote,
and serves the aggregated view.

This document is self-contained and intended to be handed to another agent or
service that consumes the API.

## What this data is (and is not)

- There is exactly **one market: `XMR/BTC`**, and it is **one-directional**:
  makers sell XMR for BTC (takers bring BTC). There is no bid side and no other
  trading pair.
- Each maker advertises **one quote**: a single price plus a `[min, max]` BTC
  size band — **not** a book of discrete price levels. The "orderbook" is the
  aggregation of all makers' single quotes.
- Quotes are **indicative**, refreshed on the maker's poll interval (~45s) and
  expired from the cache after ~180s. They are **not** resting limit orders and
  **not** proof that any trade occurred.
- `max_quantity_sat` is bounded by the maker's available Monero inventory, so it
  is the main depth signal. `reserve_proof`, when present, is a Monero reserve
  proof cryptographically backing the quote.
- **There is no executed-trades / historical-fills data.** Swaps are negotiated
  privately peer-to-peer and are unlinkable on-chain, so a real trades tape is
  not derivable from the network. Any candlestick/history must be built by a
  downstream component that polls this API over time — and it will be a history
  of *quotes*, not of trades.

## Conventions

- All monetary values are integers in **BTC satoshis** (1 BTC = 100,000,000 sat).
  Field names carry the `_sat` suffix.
- `price_sat` is the amount of BTC (in satoshis) a maker asks for **1 XMR**.
- Timestamps are RFC 3339 / ISO 8601 UTC strings (e.g. `2026-09-25T12:34:56Z`).
- Content type is `application/json`. CORS is permissive (all origins).
- The API is unauthenticated and read-only.

## Endpoints

### `GET /orderbook`

The current snapshot as a synthetic orderbook, with derived summary fields.
Makers are sorted with liquidity first, then ascending `price_sat` (best ask
first).

Response body:

| Field | Type | Description |
|---|---|---|
| `market` | string | Always `"XMR/BTC"`. |
| `generated_at` | string (RFC 3339) | When this snapshot was produced. |
| `maker_count` | integer | Number of makers currently in the cache. |
| `makers_with_liquidity` | integer | Makers with `has_liquidity = true`. |
| `best_price_sat` | integer \| null | Lowest ask among makers with liquidity; `null` if none. |
| `total_max_quantity_sat` | integer | Sum of all makers' `max_quantity_sat`. |
| `makers` | array of [MakerQuote](#makerquote) | Sorted best-first. |
| `disclaimer` | string | Human-readable reminder that values are indicative quotes. |

Example:

```json
{
  "market": "XMR/BTC",
  "generated_at": "2026-09-25T12:34:56.789Z",
  "maker_count": 3,
  "makers_with_liquidity": 2,
  "best_price_sat": 3820000,
  "total_max_quantity_sat": 51000000,
  "makers": [
    {
      "peer_id": "12D3KooW...abc",
      "multiaddr": "/dns4/maker.example.org/tcp/443/wss",
      "version": "2.1.0",
      "price_sat": 3820000,
      "min_quantity_sat": 100000,
      "max_quantity_sat": 30000000,
      "refund_policy": { "type": "FullRefund" },
      "reserve_proof": {
        "address": "4A...",
        "proof": "ReserveProofV2...",
        "message": "12D3KooW...abc"
      },
      "has_liquidity": true
    },
    {
      "peer_id": "12D3KooW...def",
      "multiaddr": "/dns4/maker2.example.org/tcp/443/wss",
      "version": "2.0.1",
      "price_sat": 3850000,
      "min_quantity_sat": 50000,
      "max_quantity_sat": 21000000,
      "refund_policy": {
        "type": "PartialRefund",
        "content": { "anti_spam_deposit_ratio": 0.05 }
      },
      "has_liquidity": true
    },
    {
      "peer_id": "12D3KooW...ghi",
      "multiaddr": "/dns4/maker3.example.org/tcp/443/wss",
      "version": null,
      "price_sat": 0,
      "min_quantity_sat": 0,
      "max_quantity_sat": 0,
      "refund_policy": { "type": "FullRefund" },
      "has_liquidity": false
    }
  ],
  "disclaimer": "Values are indicative maker quotes ..."
}
```

### `GET /quotes`

The same per-maker rows as `/orderbook`, but **unsorted** and without summary
fields — for consumers that want the raw list.

| Field | Type | Description |
|---|---|---|
| `generated_at` | string (RFC 3339) | When this snapshot was produced. |
| `maker_count` | integer | Number of makers currently in the cache. |
| `makers` | array of [MakerQuote](#makerquote) | Unsorted. |
| `disclaimer` | string | As above. |

### `GET /status`

Is this build still in step with the network, and are we connected?

| Field | Type | Description |
|---|---|---|
| `generated_at` | string (RFC 3339) | When this status was produced. |
| `ready` | boolean | At least one quote has been received. |
| `our_version` | string \| null | The `swap` release this collector is built from (makers/takers run `asb`/`swap` of the same versioning). |
| `connected_peers` | integer | Peers with an open connection. |
| `rendezvous_total` / `rendezvous_connected` | integer | Configured rendezvous nodes, and how many are connected. |
| `discovered_peers` | integer | Makers known to the quote poller. |
| `quotes_received` / `quotes_not_supported` / `quotes_failed` | integer | The last quote poll's outcomes per maker (`not_supported`: the peer lacks our bid-quote protocol). |
| `maker_count` / `makers_with_liquidity` | integer | As in `/orderbook`. |
| `maker_versions` | object | Maker count per advertised version (`unknown` when not advertised). |
| `max_maker_version` | string \| null | The newest advertised maker version. |
| `makers_newer_than_ours` | integer | Makers advertising a newer version than `our_version`. |

### `GET /healthz`

Liveness. Always returns `200 OK` with body `ok` while the process is running.

### `GET /readyz`

Readiness. Returns `200 OK` (`ready`) once the collector has received at least
one quote from the network; `503 Service Unavailable` (`not ready`) before that.
Use as a Kubernetes readiness probe.

## Types

### MakerQuote

| Field | Type | Description |
|---|---|---|
| `peer_id` | string | libp2p peer id of the maker. |
| `multiaddr` | string | Multiaddr the maker was last reached at. |
| `version` | string \| null | Maker software version (via libp2p identify), if known. |
| `price_sat` | integer | BTC satoshis asked for 1 XMR (the maker's ask). |
| `min_quantity_sat` | integer | Minimum BTC (sat) the maker accepts in a swap. |
| `max_quantity_sat` | integer | Maximum BTC (sat), bounded by the maker's inventory. |
| `refund_policy` | [RefundPolicy](#refundpolicy) | Terms if the swap is cancelled. |
| `reserve_proof` | [ReserveProof](#reserveproof) \| omitted | Monero proof of funds, when provided. |
| `has_liquidity` | boolean | `true` when `price_sat > 0` and `max_quantity_sat > 0`. |

A maker with `has_liquidity = false` (e.g. all-zero amounts) is connected but
currently advertises no usable liquidity — keep it out of pricing logic.

### RefundPolicy

A tagged enum. Either:

```json
{ "type": "FullRefund" }
```

or:

```json
{ "type": "PartialRefund", "content": { "anti_spam_deposit_ratio": 0.05 } }
```

- `FullRefund`: the taker receives 100% of their BTC back on refund.
- `PartialRefund`: `anti_spam_deposit_ratio` (0.0–1.0) is the fraction of BTC
  placed into an anti-spam deposit that the maker may withhold on refund.

### ReserveProof

```json
{ "address": "4A...", "proof": "ReserveProofV2...", "message": "<maker peer id>" }
```

- `address`: the maker's Monero address the proof is against.
- `proof`: a Monero `ReserveProofV2` string proving the maker holds funds.
- `message`: the message signed in the proof (by convention, the maker's peer id).

## Configuration (service operator reference)

The service is configured via flags or environment variables:

| Flag | Env var | Default | Description |
|---|---|---|---|
| `--host` | `MARKET_DATA_HOST` | `0.0.0.0` | HTTP bind address. |
| `--port` | `MARKET_DATA_PORT` | `8080` | HTTP bind port. |
| `--testnet` | `MARKET_DATA_TESTNET` | `false` | Use the testnet rendezvous namespace. |
| `--tor` | `MARKET_DATA_TOR` | `false` | Route connections over Tor (uses onion rendezvous addresses). |
| `--rendezvous` | `MARKET_DATA_RENDEZVOUS` | built-in public nodes | Comma-separated multiaddr override. |

Log level is controlled by `RUST_LOG` (e.g. `RUST_LOG=info,swap_p2p=debug`).

When `--tor` is off, only clearnet (`wss`) rendezvous addresses are used; when
on, only `/onion3` addresses are used.
