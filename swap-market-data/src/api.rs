//! REST API: serves the current network orderbook snapshot.
//!
//! The state is a `tokio::sync::watch` receiver holding the latest [`Snapshot`],
//! updated by the swarm task in `main.rs`. Handlers read the current value
//! without locking.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json};
use axum::routing::get;
use axum::Router;
use chrono::{DateTime, Utc};
use libp2p::{Multiaddr, PeerId};
use serde::Serialize;
use tokio::sync::watch;
use tower_http::cors::CorsLayer;

use swap_p2p::protocols::quote::{BidQuote, RefundPolicyWire, ReserveProofWithAddress};

const MARKET: &str = "XMR/BTC";
const DISCLAIMER: &str = "Values are indicative maker quotes refreshed on a poll interval, \
     not executed trades or resting limit orders. `price_sat` is the amount of BTC (in \
     satoshis) a maker asks for 1 XMR. `max_quantity_sat` is bounded by the maker's \
     available Monero inventory.";

/// One maker's current advertised quote.
#[derive(Debug, Clone, Serialize)]
pub struct MakerQuote {
    /// libp2p peer id of the maker.
    pub peer_id: String,
    /// Multiaddr the maker was last reached at.
    pub multiaddr: String,
    /// Maker software version reported via libp2p identify, if known.
    pub version: Option<String>,
    /// Price for 1 XMR, in BTC satoshis (the maker's ask).
    pub price_sat: u64,
    /// Minimum BTC amount (satoshis) the maker accepts in a swap.
    pub min_quantity_sat: u64,
    /// Maximum BTC amount (satoshis) the maker accepts, bounded by its inventory.
    pub max_quantity_sat: u64,
    /// Terms applied if the swap is cancelled.
    pub refund_policy: RefundPolicyWire,
    /// Optional Monero reserve proof backing the quote.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reserve_proof: Option<ReserveProofWithAddress>,
    /// True when the maker currently advertises usable liquidity (price and max > 0).
    pub has_liquidity: bool,
}

impl MakerQuote {
    fn from_parts(
        peer_id: PeerId,
        multiaddr: Multiaddr,
        quote: BidQuote,
        version: Option<semver::Version>,
    ) -> Self {
        let price_sat = quote.price.to_sat();
        let max_quantity_sat = quote.max_quantity.to_sat();

        Self {
            peer_id: peer_id.to_string(),
            multiaddr: multiaddr.to_string(),
            version: version.map(|v| v.to_string()),
            price_sat,
            min_quantity_sat: quote.min_quantity.to_sat(),
            max_quantity_sat,
            refund_policy: quote.refund_policy,
            reserve_proof: quote.reserve_proof,
            has_liquidity: price_sat > 0 && max_quantity_sat > 0,
        }
    }
}

/// The shared, always-current market view produced by the swarm task.
#[derive(Debug, Clone)]
pub struct Snapshot {
    /// When this snapshot was produced.
    pub generated_at: DateTime<Utc>,
    /// True once the collector has received at least one quote from the network.
    pub ready: bool,
    /// One entry per maker currently in the cache.
    pub makers: Vec<MakerQuote>,
}

impl Snapshot {
    /// The initial, empty snapshot served before the first quote arrives.
    pub fn empty() -> Self {
        Self {
            generated_at: Utc::now(),
            ready: false,
            makers: Vec::new(),
        }
    }

    /// Builds a snapshot from a `CachedQuotes` event payload.
    pub fn from_cached_quotes(
        quotes: Vec<(PeerId, Multiaddr, BidQuote, Option<semver::Version>)>,
    ) -> Self {
        let makers = quotes
            .into_iter()
            .map(|(peer_id, multiaddr, quote, version)| {
                MakerQuote::from_parts(peer_id, multiaddr, quote, version)
            })
            .collect();

        Self {
            generated_at: Utc::now(),
            ready: true,
            makers,
        }
    }
}

/// The `/orderbook` response: the snapshot plus derived summary fields.
#[derive(Debug, Clone, Serialize)]
pub struct OrderbookResponse {
    pub market: &'static str,
    pub generated_at: DateTime<Utc>,
    pub maker_count: usize,
    pub makers_with_liquidity: usize,
    /// Lowest ask across makers that currently have liquidity, in satoshis.
    pub best_price_sat: Option<u64>,
    /// Sum of all makers' advertised max quantities, in satoshis.
    pub total_max_quantity_sat: u64,
    /// Makers sorted with liquidity first, then ascending price.
    pub makers: Vec<MakerQuote>,
    pub disclaimer: &'static str,
}

impl From<Snapshot> for OrderbookResponse {
    fn from(snapshot: Snapshot) -> Self {
        let mut makers = snapshot.makers;
        makers.sort_by(|a, b| {
            b.has_liquidity
                .cmp(&a.has_liquidity)
                .then(a.price_sat.cmp(&b.price_sat))
        });

        let makers_with_liquidity = makers.iter().filter(|m| m.has_liquidity).count();
        let best_price_sat = makers
            .iter()
            .filter(|m| m.has_liquidity)
            .map(|m| m.price_sat)
            .min();
        let total_max_quantity_sat = makers.iter().map(|m| m.max_quantity_sat).sum();

        Self {
            market: MARKET,
            generated_at: snapshot.generated_at,
            maker_count: makers.len(),
            makers_with_liquidity,
            best_price_sat,
            total_max_quantity_sat,
            makers,
            disclaimer: DISCLAIMER,
        }
    }
}

/// The `/quotes` response: the raw per-maker rows, unsorted, with no summary.
#[derive(Debug, Clone, Serialize)]
pub struct QuotesResponse {
    pub generated_at: DateTime<Utc>,
    pub maker_count: usize,
    pub makers: Vec<MakerQuote>,
    pub disclaimer: &'static str,
}

impl From<Snapshot> for QuotesResponse {
    fn from(snapshot: Snapshot) -> Self {
        Self {
            generated_at: snapshot.generated_at,
            maker_count: snapshot.makers.len(),
            makers: snapshot.makers,
            disclaimer: DISCLAIMER,
        }
    }
}

/// Shared handler state: a receiver for the latest snapshot.
#[derive(Clone)]
pub struct AppState {
    pub snapshot: watch::Receiver<Snapshot>,
}

/// Builds the axum router serving the API.
pub fn router(snapshot: watch::Receiver<Snapshot>) -> Router {
    Router::new()
        .route("/orderbook", get(orderbook_handler))
        .route("/quotes", get(quotes_handler))
        .route("/healthz", get(healthz_handler))
        .route("/readyz", get(readyz_handler))
        .layer(CorsLayer::permissive())
        .with_state(AppState { snapshot })
}

async fn orderbook_handler(State(state): State<AppState>) -> Json<OrderbookResponse> {
    let snapshot = state.snapshot.borrow().clone();
    Json(snapshot.into())
}

async fn quotes_handler(State(state): State<AppState>) -> Json<QuotesResponse> {
    let snapshot = state.snapshot.borrow().clone();
    Json(snapshot.into())
}

/// Liveness: always OK while the process is running.
async fn healthz_handler() -> impl IntoResponse {
    (StatusCode::OK, "ok")
}

/// Readiness: OK only once the collector has received a quote from the network.
async fn readyz_handler(State(state): State<AppState>) -> impl IntoResponse {
    let ready = state.snapshot.borrow().ready;
    if ready {
        (StatusCode::OK, "ready")
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "not ready")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn maker(peer: &str, price_sat: u64, max_quantity_sat: u64) -> MakerQuote {
        MakerQuote {
            peer_id: peer.to_string(),
            multiaddr: "/dns4/example.org/tcp/443/wss".to_string(),
            version: Some("1.0.0".to_string()),
            price_sat,
            min_quantity_sat: 1_000,
            max_quantity_sat,
            refund_policy: RefundPolicyWire::FullRefund,
            reserve_proof: None,
            has_liquidity: price_sat > 0 && max_quantity_sat > 0,
        }
    }

    fn snapshot(makers: Vec<MakerQuote>) -> Snapshot {
        Snapshot {
            generated_at: Utc::now(),
            ready: true,
            makers,
        }
    }

    #[test]
    fn orderbook_sorts_liquidity_first_then_by_price_and_summarizes() {
        let snapshot = snapshot(vec![
            maker("high", 3_900, 10),
            maker("zero", 0, 0),
            maker("low", 3_800, 20),
        ]);

        let orderbook = OrderbookResponse::from(snapshot);

        assert_eq!(orderbook.market, "XMR/BTC");
        assert_eq!(orderbook.maker_count, 3);
        assert_eq!(orderbook.makers_with_liquidity, 2);
        assert_eq!(orderbook.best_price_sat, Some(3_800));
        assert_eq!(orderbook.total_max_quantity_sat, 30);

        // Liquidity first, then ascending price; zero-liquidity maker is last.
        assert_eq!(orderbook.makers[0].peer_id, "low");
        assert_eq!(orderbook.makers[1].peer_id, "high");
        assert_eq!(orderbook.makers[2].peer_id, "zero");
    }

    #[test]
    fn best_price_is_none_without_liquidity() {
        let orderbook = OrderbookResponse::from(snapshot(vec![maker("zero", 0, 0)]));

        assert_eq!(orderbook.best_price_sat, None);
        assert_eq!(orderbook.makers_with_liquidity, 0);
    }

    #[test]
    fn serializes_to_expected_json_shape() {
        let orderbook = OrderbookResponse::from(snapshot(vec![maker("m1", 3_800, 20)]));

        let json = serde_json::to_value(&orderbook).expect("serialize orderbook");

        assert_eq!(json["market"], "XMR/BTC");
        assert_eq!(json["makers"][0]["price_sat"], 3_800);
        assert_eq!(json["makers"][0]["refund_policy"]["type"], "FullRefund");
        // reserve_proof is omitted when absent.
        assert!(json["makers"][0].get("reserve_proof").is_none());
    }
}
