//! Who is on the other end of an RPC.
//!
//! The caching protocol carries no client identity, so the only handle on
//! "which job caused this traffic" is the transport: over a unix socket the
//! kernel hands us the peer's pid (a `swift-frontend` / `clang` process, i.e.
//! one compile job), over TCP the remote address (a `swiftcache proxy`, i.e.
//! one developer machine).

use std::fmt;
use std::net::SocketAddr;

#[cfg(unix)]
use tonic::transport::server::UdsConnectInfo;
use tonic::transport::server::TcpConnectInfo;
use tonic::Request;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Peer {
    /// Client pid on a unix socket, from `SO_PEERCRED`.
    Pid(u32),
    /// Remote address on TCP.
    Addr(SocketAddr),
    Unknown,
}

impl Peer {
    pub fn of<T>(req: &Request<T>) -> Peer {
        #[cfg(unix)]
        if let Some(uds) = req.extensions().get::<UdsConnectInfo>() {
            if let Some(pid) = uds.peer_cred.and_then(|c| c.pid()) {
                return Peer::Pid(pid as u32);
            }
        }
        if let Some(tcp) = req.extensions().get::<TcpConnectInfo>() {
            if let Some(addr) = tcp.remote_addr {
                return Peer::Addr(addr);
            }
        }
        Peer::Unknown
    }
}

impl fmt::Display for Peer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Peer::Pid(pid) => write!(f, "pid:{pid}"),
            Peer::Addr(addr) => write!(f, "{addr}"),
            Peer::Unknown => f.write_str("?"),
        }
    }
}
