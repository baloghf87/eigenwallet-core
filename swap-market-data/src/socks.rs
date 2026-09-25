//! Dial-only libp2p transport that tunnels every connection through an
//! external SOCKS5 proxy (e.g. a shared `tor` daemon), as an alternative to the
//! embedded arti client.
//!
//! Supported addresses: `/onion3/<addr>:<port>`, `/dns{,4,6}/<host>/tcp/<port>`
//! and `/ip{4,6}/<ip>/tcp/<port>` (any trailing protocols such as `/ws`, `/wss`
//! or `/p2p` are handled by the wrapping transports). Host names are always
//! resolved by the proxy (SOCKS5 ATYP=domain) so no DNS leaks outside Tor.

use std::io;
use std::net::IpAddr;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures::future::BoxFuture;
use libp2p::core::transport::{ListenerId, TransportEvent};
use libp2p::multiaddr::Protocol;
use libp2p::{Multiaddr, Transport, TransportError};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Target of a SOCKS5 CONNECT request.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Target {
    Domain(String, u16),
    Ip(IpAddr, u16),
}

pub struct Socks5Transport {
    proxy: String,
}

impl Socks5Transport {
    /// `proxy` is `host:port` of the SOCKS5 server, resolved on every dial so a
    /// Kubernetes service name keeps working across proxy restarts.
    pub fn new(proxy: impl Into<String>) -> Self {
        Self {
            proxy: proxy.into(),
        }
    }
}

/// Extracts the SOCKS target from the leading protocols of `addr`.
fn target_of(addr: &Multiaddr) -> Option<Target> {
    let mut iter = addr.iter();
    match iter.next()? {
        Protocol::Onion3(onion) => {
            let host = format!(
                "{}.onion",
                data_encoding::BASE32_NOPAD
                    .encode(onion.hash())
                    .to_lowercase()
            );
            Some(Target::Domain(host, onion.port()))
        }
        Protocol::Dns(host) | Protocol::Dns4(host) | Protocol::Dns6(host) => match iter.next()? {
            Protocol::Tcp(port) => Some(Target::Domain(host.to_string(), port)),
            _ => None,
        },
        Protocol::Ip4(ip) => match iter.next()? {
            Protocol::Tcp(port) => Some(Target::Ip(IpAddr::V4(ip), port)),
            _ => None,
        },
        Protocol::Ip6(ip) => match iter.next()? {
            Protocol::Tcp(port) => Some(Target::Ip(IpAddr::V6(ip), port)),
            _ => None,
        },
        _ => None,
    }
}

/// Encodes a SOCKS5 CONNECT request (RFC 1928 §4).
fn connect_request(target: &Target) -> io::Result<Vec<u8>> {
    let mut req = vec![0x05, 0x01, 0x00];
    let port = match target {
        Target::Domain(host, port) => {
            let len = u8::try_from(host.len())
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "host name too long"))?;
            req.push(0x03);
            req.push(len);
            req.extend_from_slice(host.as_bytes());
            *port
        }
        Target::Ip(IpAddr::V4(ip), port) => {
            req.push(0x01);
            req.extend_from_slice(&ip.octets());
            *port
        }
        Target::Ip(IpAddr::V6(ip), port) => {
            req.push(0x04);
            req.extend_from_slice(&ip.octets());
            *port
        }
    };
    req.extend_from_slice(&port.to_be_bytes());
    Ok(req)
}

async fn socks5_connect(proxy: &str, target: &Target) -> io::Result<TcpStream> {
    let mut stream = TcpStream::connect(proxy).await?;
    stream.set_nodelay(true)?;

    // Greeting: version 5, one method, "no authentication".
    stream.write_all(&[0x05, 0x01, 0x00]).await?;
    let mut choice = [0u8; 2];
    stream.read_exact(&mut choice).await?;
    if choice != [0x05, 0x00] {
        return Err(io::Error::other(format!(
            "SOCKS5 proxy rejected no-auth method: {choice:?}"
        )));
    }

    stream.write_all(&connect_request(target)?).await?;

    // Reply: VER REP RSV ATYP BND.ADDR BND.PORT
    let mut head = [0u8; 4];
    stream.read_exact(&mut head).await?;
    if head[1] != 0x00 {
        return Err(io::Error::new(
            io::ErrorKind::ConnectionRefused,
            format!("SOCKS5 connect to {target:?} failed with reply code {}", head[1]),
        ));
    }
    let addr_len = match head[3] {
        0x01 => 4,
        0x04 => 16,
        0x03 => {
            let mut len = [0u8; 1];
            stream.read_exact(&mut len).await?;
            usize::from(len[0])
        }
        other => {
            return Err(io::Error::other(format!(
                "SOCKS5 reply with unknown address type {other}"
            )));
        }
    };
    let mut bound = vec![0u8; addr_len + 2];
    stream.read_exact(&mut bound).await?;

    Ok(stream)
}

impl Transport for Socks5Transport {
    type Output = libp2p::tcp::tokio::TcpStream;
    type Error = io::Error;
    type Dial = BoxFuture<'static, Result<Self::Output, Self::Error>>;
    type ListenerUpgrade = futures::future::Pending<Result<Self::Output, Self::Error>>;

    fn listen_on(
        &mut self,
        _id: ListenerId,
        addr: Multiaddr,
    ) -> Result<(), TransportError<Self::Error>> {
        Err(TransportError::MultiaddrNotSupported(addr))
    }

    fn remove_listener(&mut self, _id: ListenerId) -> bool {
        false
    }

    fn dial(&mut self, addr: Multiaddr) -> Result<Self::Dial, TransportError<Self::Error>> {
        let target = target_of(&addr).ok_or(TransportError::MultiaddrNotSupported(addr.clone()))?;
        let proxy = self.proxy.clone();
        Ok(Box::pin(async move {
            let stream = socks5_connect(&proxy, &target).await?;
            tracing::debug!(%addr, "Established connection to peer through SOCKS5 proxy");
            Ok(libp2p::tcp::tokio::TcpStream(stream))
        }))
    }

    fn dial_as_listener(
        &mut self,
        addr: Multiaddr,
    ) -> Result<Self::Dial, TransportError<Self::Error>> {
        self.dial(addr)
    }

    fn address_translation(&self, _listen: &Multiaddr, _observed: &Multiaddr) -> Option<Multiaddr> {
        None
    }

    fn poll(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<TransportEvent<Self::ListenerUpgrade, Self::Error>> {
        Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn onion3_target_is_a_domain() {
        let addr: Multiaddr = "/onion3/3xl2zfur4tpebogsrgn3l7l2illzkhwi3755jplmycmn4q77nxsrl6qd:8888/p2p/12D3KooWGRvf7qVQDrNR5nfYD6rKrbgeTi9x8RrbdxbmsPvxL4mw"
            .parse()
            .unwrap();
        assert_eq!(
            target_of(&addr),
            Some(Target::Domain(
                "3xl2zfur4tpebogsrgn3l7l2illzkhwi3755jplmycmn4q77nxsrl6qd.onion".into(),
                8888
            ))
        );
    }

    #[test]
    fn dns_and_ip_targets() {
        let wss: Multiaddr = "/dns4/discovery.eigenwallet.org/tcp/443/wss".parse().unwrap();
        assert_eq!(
            target_of(&wss),
            Some(Target::Domain("discovery.eigenwallet.org".into(), 443))
        );
        let ip: Multiaddr = "/ip4/1.2.3.4/tcp/9939".parse().unwrap();
        assert_eq!(
            target_of(&ip),
            Some(Target::Ip("1.2.3.4".parse().unwrap(), 9939))
        );
        let udp: Multiaddr = "/ip4/1.2.3.4/udp/9939/quic-v1".parse().unwrap();
        assert_eq!(target_of(&udp), None);
    }

    #[test]
    fn domain_connect_request_encoding() {
        let req = connect_request(&Target::Domain("ab.onion".into(), 443)).unwrap();
        assert_eq!(
            req,
            [&[5, 1, 0, 3, 8][..], b"ab.onion", &443u16.to_be_bytes()].concat()
        );
    }
}
