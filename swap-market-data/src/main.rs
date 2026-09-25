//! Headless market-data collector for the eigenwallet XMR/BTC swap network.
//!
//! Connects to the network's rendezvous nodes, discovers makers, continuously
//! polls each maker for its `BidQuote`, and serves the aggregated current
//! snapshot (a synthetic orderbook) over a small REST API. It is stateless: no
//! persistence — downstream components derive history/candlesticks by polling
//! this API over time.

mod api;
mod transport;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use arti_client::config::TorClientConfigBuilder;
use arti_client::TorClient;
use clap::Parser;
use futures::StreamExt;
use libp2p::multiaddr::Protocol;
use libp2p::swarm::NetworkBehaviour;
use libp2p::{identify, identity, ping, Multiaddr, SwarmBuilder};
use tokio::sync::watch;
use tor_rtcompat::tokio::TokioRustlsRuntime;
use tracing_subscriber::EnvFilter;

use swap_p2p::libp2p_ext::MultiAddrExt;
use swap_p2p::protocols::{quotes_cached, rendezvous};

use crate::api::Snapshot;

/// How long an otherwise-idle connection to a maker is kept open. Redial keeps
/// reconnecting, so this only bounds resource use.
const IDLE_CONNECTION_TIMEOUT: Duration = Duration::from_secs(60);

const AGENT_VERSION: &str = concat!("market-data/", env!("CARGO_PKG_VERSION"));

#[derive(Parser, Debug)]
#[command(name = "market-data", about, version)]
struct Args {
    /// Host address to bind the HTTP server to.
    #[arg(long, env = "MARKET_DATA_HOST", default_value = "0.0.0.0")]
    host: String,

    /// Port to bind the HTTP server to.
    #[arg(long, env = "MARKET_DATA_PORT", default_value_t = 8080)]
    port: u16,

    /// Use the testnet rendezvous namespace instead of mainnet.
    #[arg(long, env = "MARKET_DATA_TESTNET", default_value_t = false)]
    testnet: bool,

    /// Route all connections over Tor (required to reach `/onion3` rendezvous
    /// addresses). Off by default; clearnet `wss` addresses are used instead.
    #[arg(long, env = "MARKET_DATA_TOR", default_value_t = false)]
    tor: bool,

    /// Override the rendezvous point multiaddrs (comma-separated / repeatable).
    /// Defaults to the network's public rendezvous nodes.
    #[arg(long = "rendezvous", env = "MARKET_DATA_RENDEZVOUS", value_delimiter = ',')]
    rendezvous: Vec<Multiaddr>,
}

/// The collector's libp2p behaviour: discover makers, ping to keep connections
/// healthy, and poll + cache each maker's quote.
#[derive(NetworkBehaviour)]
struct Behaviour {
    rendezvous: rendezvous::discovery::Behaviour,
    ping: ping::Behaviour,
    quote: quotes_cached::Behaviour,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,swap_p2p=info,market_data=info")),
        )
        .init();

    // Required for the rustls-based TLS used by `wss` (clearnet) and Tor.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let args = Args::parse();

    let maybe_tor_client = if args.tor {
        Some(build_tor_client().await?)
    } else {
        None
    };

    let rendezvous_addresses = resolve_rendezvous_addresses(&args);
    if rendezvous_addresses.is_empty() {
        anyhow::bail!(
            "No usable rendezvous addresses. {}",
            if args.tor {
                "Provide rendezvous addresses via --rendezvous."
            } else {
                "Enable --tor to use onion addresses, or provide clearnet addresses via --rendezvous."
            }
        );
    }

    let identity = identity::Keypair::generate_ed25519();
    let namespace = rendezvous::XmrBtcNamespace::from_is_testnet(args.testnet);

    let rendezvous_peer_ids = rendezvous_addresses
        .iter()
        .filter_map(|addr| addr.extract_peer_id())
        .collect();

    let behaviour = Behaviour {
        rendezvous: rendezvous::discovery::Behaviour::new(
            identity.clone(),
            rendezvous_peer_ids,
            namespace.into(),
        ),
        ping: ping::Behaviour::new(ping::Config::new()),
        quote: quotes_cached::Behaviour::new(identify::Config::new(
            AGENT_VERSION.to_string(),
            identity.public(),
        )),
    };

    let transport = transport::new(&identity, maybe_tor_client)?;

    let mut swarm = SwarmBuilder::with_existing_identity(identity)
        .with_tokio()
        .with_other_transport(|_| transport)?
        .with_behaviour(|_| behaviour)?
        .with_swarm_config(|cfg| cfg.with_idle_connection_timeout(IDLE_CONNECTION_TIMEOUT))
        .build();

    for addr in &rendezvous_addresses {
        if let Some(peer_id) = addr.extract_peer_id() {
            swarm.add_peer_address(peer_id, addr.clone());
        }
    }

    tracing::info!(
        network = %namespace,
        rendezvous_nodes = rendezvous_addresses.len(),
        tor = args.tor,
        "Starting market-data collector"
    );

    // The swarm task produces snapshots; the HTTP server serves the latest one.
    let (snapshot_tx, snapshot_rx) = watch::channel(Snapshot::empty());

    tokio::spawn(run_swarm(swarm, snapshot_tx));

    let bind_address = format!("{}:{}", args.host, args.port);
    let listener = tokio::net::TcpListener::bind(&bind_address)
        .await
        .with_context(|| format!("Failed to bind HTTP server to {bind_address}"))?;

    tracing::info!(%bind_address, "Serving REST API");

    axum::serve(listener, api::router(snapshot_rx))
        .await
        .context("HTTP server error")?;

    Ok(())
}

/// Drives the libp2p swarm, forwarding each `CachedQuotes` snapshot to the API.
async fn run_swarm(mut swarm: libp2p::Swarm<Behaviour>, snapshot_tx: watch::Sender<Snapshot>) {
    loop {
        let event = swarm.select_next_some().await;

        let libp2p::swarm::SwarmEvent::Behaviour(BehaviourEvent::Quote(
            quotes_cached::Event::CachedQuotes { quotes },
        )) = event
        else {
            continue;
        };

        let snapshot = Snapshot::from_cached_quotes(quotes);
        tracing::debug!(makers = snapshot.makers.len(), "Updated orderbook snapshot");
        let _ = snapshot_tx.send(snapshot);
    }
}

/// Returns the rendezvous addresses to use, filtered by transport: onion
/// addresses require Tor; clearnet addresses are used otherwise.
fn resolve_rendezvous_addresses(args: &Args) -> Vec<Multiaddr> {
    let configured = if args.rendezvous.is_empty() {
        swap_env::defaults::default_rendezvous_points()
    } else {
        args.rendezvous.clone()
    };

    configured
        .into_iter()
        .filter(|addr| is_onion(addr) == args.tor)
        .collect()
}

fn is_onion(addr: &Multiaddr) -> bool {
    addr.iter().any(|p| matches!(p, Protocol::Onion3(_)))
}

/// Builds and bootstraps a Tor client for routing connections over Tor.
async fn build_tor_client() -> Result<Arc<TorClient<TokioRustlsRuntime>>> {
    let config = TorClientConfigBuilder::default()
        .build()
        .context("Failed to build Tor client config")?;
    let runtime = TokioRustlsRuntime::current().context("Failed to get tokio runtime for Tor")?;

    // `create_bootstrapped` already yields an `Arc<TorClient>`.
    let client = TorClient::with_runtime(runtime)
        .config(config)
        .create_bootstrapped()
        .await
        .context("Failed to bootstrap Tor client")?;

    Ok(client)
}
