//! Connections: TCP with timeouts, optionally wrapped in TLS.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use rustls::pki_types::ServerName;
use rustls::{ClientConfig, ClientConnection, RootCertStore, StreamOwned};

use crate::{NetError, Result};

pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// How long an address is given while there are others to try (one family's route can be broken
/// while the other's works: a whole connect timeout on each request made everything crawl).
const FIRST_TRIES: Duration = Duration::from_secs(4);

/// An IPv6 connection failed during this run: IPv4 addresses go first from then on.
static PREFER_IPV4: AtomicBool = AtomicBool::new(false);

/// The addresses to try, in turn: the two families alternating (as RFC 8305 does), starting with
/// the resolver's first, or with IPv4 once IPv6 has failed.
fn in_turn(addrs: Vec<SocketAddr>, ipv4_first: bool) -> Vec<SocketAddr> {
    let first_v6 = !ipv4_first && addrs.first().is_some_and(SocketAddr::is_ipv6);
    let (mut a, mut b): (Vec<SocketAddr>, Vec<SocketAddr>) = addrs.into_iter().partition(|x| x.is_ipv6() == first_v6);
    let mut out = Vec::with_capacity(a.len() + b.len());
    a.reverse();
    b.reverse();
    loop {
        match (a.pop(), b.pop()) {
            (None, None) => return out,
            (x, y) => out.extend(x.into_iter().chain(y)),
        }
    }
}
pub const IO_TIMEOUT: Duration = Duration::from_secs(60);

/// A stream whose read timeout can change (IDLE waits in short slices).
pub trait Timeouts {
    fn set_read_timeout(&mut self, t: Option<Duration>) -> io::Result<()>;
}

pub enum Stream {
    Plain(TcpStream),
    Tls(Box<StreamOwned<ClientConnection, TcpStream>>),
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Stream::Plain(s) => s.read(buf),
            Stream::Tls(s) => s.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Stream::Plain(s) => s.write(buf),
            Stream::Tls(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Stream::Plain(s) => s.flush(),
            Stream::Tls(s) => s.flush(),
        }
    }
}

impl Timeouts for Stream {
    fn set_read_timeout(&mut self, t: Option<Duration>) -> io::Result<()> {
        match self {
            Stream::Plain(s) => s.set_read_timeout(t),
            Stream::Tls(s) => s.sock.set_read_timeout(t),
        }
    }
}

impl Timeouts for TcpStream {
    fn set_read_timeout(&mut self, t: Option<Duration>) -> io::Result<()> {
        TcpStream::set_read_timeout(self, t)
    }
}

fn tls_config() -> Result<Arc<ClientConfig>> {
    static CONFIG: OnceLock<std::result::Result<Arc<ClientConfig>, String>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let mut roots = RootCertStore::empty();
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .map(|b| Arc::new(b.with_root_certificates(roots).with_no_client_auth()))
                .map_err(|e| e.to_string())
        })
        .clone()
        .map_err(NetError::Tls)
}

/// Open a TCP connection, trying each resolved address in turn.
pub fn tcp(host: &str, port: u16) -> Result<TcpStream> {
    let began = Instant::now();
    // A name that doesn't resolve is almost always a mistyped or guessed server name.
    let addrs = (host, port).to_socket_addrs().map_err(|e| {
        NetError::Io(io::Error::new(
            e.kind(),
            format!("no server called {host} could be found: check the server name in Server settings, and that this computer is online ({e})"),
        ))
    })?;
    let resolved = began.elapsed();
    let addrs = in_turn(addrs.collect(), PREFER_IPV4.load(Ordering::Relaxed));
    let mut last = io::Error::new(io::ErrorKind::NotFound, format!("{host}: no address"));
    for (i, a) in addrs.iter().enumerate() {
        let wait = if i + 1 < addrs.len() { FIRST_TRIES } else { CONNECT_TIMEOUT };
        let at = Instant::now();
        match TcpStream::connect_timeout(a, wait) {
            Ok(s) => {
                s.set_read_timeout(Some(IO_TIMEOUT))?;
                s.set_write_timeout(Some(IO_TIMEOUT))?;
                let _ = s.set_nodelay(true);
                log::debug!("{host}: name found in {} ms, connected to {a} in {} ms", resolved.as_millis(), at.elapsed().as_millis());
                return Ok(s);
            }
            Err(e) => {
                log::info!("{host}: {a}: no connection after {} ms: {e}", at.elapsed().as_millis());
                if a.is_ipv6() && !PREFER_IPV4.swap(true, Ordering::Relaxed) {
                    log::warn!("IPv6 doesn't work from here ({host}: {e}): IPv4 first from now on");
                }
                last = e;
            }
        }
    }
    Err(NetError::Io(io::Error::new(last.kind(), format!("{host}:{port}: {last}"))))
}

/// Wrap a TCP connection in TLS, verifying the certificate against `host`.
pub fn tls(sock: TcpStream, host: &str) -> Result<Stream> {
    let name = ServerName::try_from(host.to_string()).map_err(|e| NetError::Tls(format!("{host}: {e}")))?;
    let conn = ClientConnection::new(tls_config()?, name).map_err(|e| NetError::Tls(e.to_string()))?;
    let mut s = StreamOwned::new(conn, sock);
    let began = Instant::now();
    // Finish the handshake now so certificate problems surface as TLS errors, not later I/O.
    while s.conn.is_handshaking() {
        s.conn.complete_io(&mut s.sock).map_err(|e| NetError::Tls(e.to_string()))?;
    }
    log::debug!("{host}: secure connection in {} ms", began.elapsed().as_millis());
    Ok(Stream::Tls(Box::new(s)))
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;

    /// IPv6 and IPv4 take turns, the resolver's first family first, or IPv4 once IPv6 failed.
    #[test]
    fn address_families_take_turns() {
        let a = |s: &str| s.parse::<SocketAddr>().unwrap();
        let (v6a, v6b, v4a, v4b) = (a("[2001:db8::1]:443"), a("[2001:db8::2]:443"), a("192.0.2.1:443"), a("192.0.2.2:443"));
        assert_eq!(super::in_turn(vec![v6a, v6b, v4a, v4b], false), vec![v6a, v4a, v6b, v4b]);
        assert_eq!(super::in_turn(vec![v6a, v6b, v4a, v4b], true), vec![v4a, v6a, v4b, v6b]);
        assert_eq!(super::in_turn(vec![v4a, v6a, v6b], false), vec![v4a, v6a, v6b]);
        assert_eq!(super::in_turn(vec![v6a], true), vec![v6a]);
        assert!(super::in_turn(Vec::new(), false).is_empty());
    }
    #[test]
    fn a_server_name_that_does_not_exist_says_what_to_check() {
        // `.invalid` never resolves (RFC 2606).
        let e = super::tcp("mail.no-such-host.invalid", 995).err().map(|e| e.to_string()).unwrap_or_default();
        assert!(e.contains("no server called mail.no-such-host.invalid") && e.contains("Server settings"), "{e}");
    }
}
