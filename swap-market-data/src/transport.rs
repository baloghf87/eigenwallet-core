//! libp2p transport for the market-data collector.
//!
//! Adapted from `swap/src/cli/transport.rs`. The collector needs to reach the
//! network's rendezvous nodes, whose default addresses are WebSocket-Secure
//! (`/dns4/.../tcp/443/wss`) on clearnet and `/onion3/...` over Tor. A plain TCP
//! transport cannot dial `wss`, so we build a websocket transport (which handles
//! `/ws` and `/wss`) layered over a Tor-or-TCP+DNS chain, plus a plain
//! Tor-or-TCP+DNS transport for non-websocket (e.g. onion) addresses.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use arti_client::TorClient;
use futures::{AsyncRead, AsyncWrite};
use libp2p::core::muxing::StreamMuxerBox;
use libp2p::core::transport::{Boxed, OptionalTransport};
use libp2p::core::upgrade::Version;
use libp2p::{PeerId, Transport, dns, identity, noise, tcp, websocket, yamux};
use libp2p_tor::{AddressConversion, TorTransport};
use tor_rtcompat::tokio::TokioRustlsRuntime;

use crate::socks::Socks5Transport;

const AUTH_AND_MULTIPLEX_TIMEOUT: Duration = Duration::from_secs(15);
// The quote/discovery path only uses a handful of protocols concurrently.
const MAX_NUM_STREAMS: usize = 5;

/// Creates the libp2p transport for the collector.
///
/// When `maybe_tor_client` is `Some`, connections are routed over Tor (required
/// to reach the `/onion3` rendezvous addresses); otherwise plain TCP+DNS is used
/// to reach the clearnet `wss` addresses.
pub fn new(
    identity: &identity::Keypair,
    maybe_tor_client: Option<Arc<TorClient<TokioRustlsRuntime>>>,
) -> Result<Boxed<(PeerId, StreamMuxerBox)>> {
    // Websocket transport over a Tor-or-TCP+DNS chain. `WsConfig` strips the
    // `/ws`/`/wss` suffix and delegates to its inner transport.
    let ws_inner_tcp = tcp::tokio::Transport::new(tcp::Config::new().nodelay(true));
    let ws_inner_tcp_dns = dns::tokio::Transport::system(ws_inner_tcp)
        .context("Failed to create DNS transport for websocket transport")?;
    let ws_inner_tor: OptionalTransport<TorTransport> = match &maybe_tor_client {
        Some(client) => OptionalTransport::some(TorTransport::from_client(
            Arc::clone(client),
            AddressConversion::IpAndDns,
        )),
        None => OptionalTransport::none(),
    };
    let ws_inner = ws_inner_tor.or_transport(ws_inner_tcp_dns);
    let ws_transport = websocket::WsConfig::new(ws_inner);

    // Plain Tor-or-TCP+DNS transport for non-websocket (e.g. onion) addresses.
    let tcp = tcp::tokio::Transport::new(tcp::Config::new().nodelay(true));
    let tcp_with_dns =
        dns::tokio::Transport::system(tcp).context("Failed to create DNS transport for TCP")?;
    let maybe_tor_transport: OptionalTransport<TorTransport> = match maybe_tor_client {
        Some(client) => OptionalTransport::some(TorTransport::from_client(
            client,
            AddressConversion::IpAndDns,
        )),
        None => OptionalTransport::none(),
    };
    let plain_transport = maybe_tor_transport.or_transport(tcp_with_dns);

    // `WsConfig` must come first — otherwise the plain transport would eagerly
    // claim the address (ignoring the `/ws` suffix) and skip the WebSocket
    // handshake.
    let transport = ws_transport.or_transport(plain_transport).boxed();

    authenticate_and_multiplex(transport, identity)
}

/// Creates the libp2p transport that routes every connection (websocket and
/// plain, clearnet and onion) through the SOCKS5 proxy at `proxy` (`host:port`),
/// e.g. a shared `tor` daemon, instead of the embedded arti client.
pub fn new_socks(
    identity: &identity::Keypair,
    proxy: &str,
) -> Result<Boxed<(PeerId, StreamMuxerBox)>> {
    // As in `new`: `WsConfig` must come first so `/ws`/`/wss` addresses get the
    // WebSocket handshake before the plain transport can claim them.
    let ws_transport = websocket::WsConfig::new(Socks5Transport::new(proxy));
    let transport = ws_transport
        .or_transport(Socks5Transport::new(proxy))
        .boxed();

    authenticate_and_multiplex(transport, identity)
}

/// Applies the noise authentication and yamux multiplexing upgrades that all
/// libp2p peers in this network share. Copied from `swap/src/network/transport.rs`.
fn authenticate_and_multiplex<T>(
    transport: Boxed<T>,
    identity: &identity::Keypair,
) -> Result<Boxed<(PeerId, StreamMuxerBox)>>
where
    T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let auth_upgrade = noise::Config::new(identity)?;
    let mut multiplex_upgrade = yamux::Config::default();
    multiplex_upgrade.set_max_num_streams(MAX_NUM_STREAMS);

    let transport = transport
        .upgrade(Version::V1)
        .authenticate(auth_upgrade)
        .multiplex(multiplex_upgrade)
        .timeout(AUTH_AND_MULTIPLEX_TIMEOUT)
        .map(|(peer, muxer), _| (peer, StreamMuxerBox::new(muxer)))
        .boxed();

    Ok(transport)
}
