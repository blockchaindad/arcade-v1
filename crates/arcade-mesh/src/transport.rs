//! Transports for a mesh link: plain TCP (u32 BE length + body) and WebSocket over TLS
//! (wss://host/mesh, one binary message per frame body, client-masked), as mesh/wsock.py.

use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

pub const MAX_FRAME: usize = 64 * 1024;
/// Largest frame taken in. We send at most MAX_FRAME (the protocol's limit), but old arcades
/// send their chunk words on link-up in 500-row "dx dn" frames of ~73 KB (the rows grew a
/// notes digest after the row count was chosen); refusing those dropped the link.
pub const MAX_INBOUND: usize = 256 * 1024;
pub const MAX_WS_MESSAGE: usize = 128 * 1024;

pub trait Transport: Send {
    fn send(&mut self, body: &[u8]) -> io::Result<()>;
    fn recv(&mut self) -> io::Result<Vec<u8>>;
    fn set_read_timeout(&mut self, d: Duration) -> io::Result<()>;
    fn describe(&self) -> String;
    /// Wait up to `d` for something to read, without consuming it (so a later recv never
    /// starts mid-frame). The read timeout is left at `d`; the caller sets it back.
    fn wait_readable(&mut self, d: Duration) -> io::Result<bool>;
}

fn peek_tcp(s: &TcpStream, d: Duration) -> io::Result<bool> {
    s.set_read_timeout(Some(d.max(Duration::from_millis(1))))?;
    match s.peek(&mut [0u8; 1]) {
        Ok(0) => Ok(true), // closed: let recv report it
        Ok(_) => Ok(true),
        Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => Ok(false),
        Err(e) => Err(e),
    }
}

// ---------------- TCP ----------------

pub struct Tcp {
    pub stream: TcpStream,
}

impl Transport for Tcp {
    fn send(&mut self, body: &[u8]) -> io::Result<()> {
        if body.len() > MAX_FRAME {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
        }
        let mut b = Vec::with_capacity(4 + body.len());
        b.extend_from_slice(&(body.len() as u32).to_be_bytes());
        b.extend_from_slice(body);
        self.stream.write_all(&b)
    }
    fn recv(&mut self) -> io::Result<Vec<u8>> {
        let mut l = [0u8; 4];
        self.stream.read_exact(&mut l)?;
        let n = u32::from_be_bytes(l) as usize;
        if n > MAX_INBOUND {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
        }
        let mut b = vec![0u8; n];
        self.stream.read_exact(&mut b)?;
        Ok(b)
    }
    fn set_read_timeout(&mut self, d: Duration) -> io::Result<()> {
        self.stream.set_read_timeout(Some(d))
    }
    fn wait_readable(&mut self, d: Duration) -> io::Result<bool> {
        peek_tcp(&self.stream, d)
    }
    fn describe(&self) -> String {
        self.stream.peer_addr().map(|a| a.to_string()).unwrap_or_default()
    }
}

// ---------------- WebSocket over TLS ----------------

type TlsStream = rustls::StreamOwned<rustls::ClientConnection, TcpStream>;

pub struct Wss {
    s: TlsStream,
    url: String,
    /// Bytes of a WebSocket message being assembled across continuation frames.
    partial: Vec<u8>,
    /// The link's byte stream carried in WebSocket messages, as on TCP
    /// (u32 BE length + body); frames may span or share messages.
    inbox: Vec<u8>,
}

fn tls_config() -> Arc<rustls::ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    Arc::new(
        rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .expect("TLS versions")
            .with_root_certificates(roots)
            .with_no_client_auth(),
    )
}


fn bad(m: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, m.to_string())
}

impl Wss {
    /// Connect to `wss://host[:port]/path`.
    pub fn connect(url: &str, timeout: Duration) -> io::Result<Wss> {
        Self::connect_checked(url, timeout, &|_| true)
    }

    /// Connect as `connect` does, only if every address the host resolves to passes `ok`.
    pub fn connect_checked(url: &str, timeout: Duration, ok: &dyn Fn(&std::net::IpAddr) -> bool) -> io::Result<Wss> {
        let rest = url.strip_prefix("wss://").ok_or_else(|| bad("not a wss:// URL"))?;
        let (hostport, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/mesh"),
        };
        let (host, port) = match hostport.rsplit_once(':') {
            Some((h, p)) if p.parse::<u16>().is_ok() => (h, p.parse().unwrap()),
            _ => (hostport, 443u16),
        };
        let all: Vec<std::net::SocketAddr> = (host, port).to_socket_addrs()?.collect();
        if all.iter().any(|a| !ok(&a.ip())) {
            return Err(bad(&format!("{host} resolves to an address not to dial")));
        }
        let sa = all.first().copied().ok_or_else(|| bad("cannot resolve host"))?;
        let tcp = TcpStream::connect_timeout(&sa, timeout)?;
        tcp.set_read_timeout(Some(timeout))?;
        tcp.set_write_timeout(Some(timeout))?;
        tcp.set_nodelay(true)?;
        let name = rustls::pki_types::ServerName::try_from(host.to_string()).map_err(|_| bad("bad host name"))?;
        let conn = rustls::ClientConnection::new(tls_config(), name).map_err(|e| bad(&e.to_string()))?;
        let mut s = rustls::StreamOwned::new(conn, tcp);

        use base64::Engine as _;
        let key = base64::engine::general_purpose::STANDARD.encode(crate::random::bytes::<16>());
        let req = format!(
            "GET {path} HTTP/1.1\r\nHost: {host}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\nUser-Agent: arcade/1.0\r\n\r\n"
        );
        s.write_all(req.as_bytes())?;
        // Read the HTTP response head byte by byte (it is short) so no frame bytes are lost.
        let mut head = Vec::new();
        let mut b = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            s.read_exact(&mut b)?;
            head.push(b[0]);
            if head.len() > 8192 {
                return Err(bad("oversized HTTP response"));
            }
        }
        let text = String::from_utf8_lossy(&head);
        if !text.starts_with("HTTP/1.1 101") {
            return Err(bad(&format!("WebSocket refused: {}", text.lines().next().unwrap_or(""))));
        }
        Ok(Wss { s, url: url.into(), partial: Vec::new(), inbox: Vec::new() })
    }

    fn write_frame(&mut self, opcode: u8, payload: &[u8]) -> io::Result<()> {
        let mut f = Vec::with_capacity(14 + payload.len());
        f.push(0x80 | opcode); // FIN
        let n = payload.len();
        if n < 126 {
            f.push(0x80 | n as u8);
        } else if n <= 0xffff {
            f.push(0x80 | 126);
            f.extend_from_slice(&(n as u16).to_be_bytes());
        } else {
            f.push(0x80 | 127);
            f.extend_from_slice(&(n as u64).to_be_bytes());
        }
        let mask: [u8; 4] = crate::random::bytes();
        f.extend_from_slice(&mask);
        f.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
        self.s.write_all(&f)?;
        self.s.flush()
    }
}

impl Wss {
    /// Next whole WebSocket message.
    fn message(&mut self) -> io::Result<Vec<u8>> {
        loop {
            let mut h = [0u8; 2];
            self.s.read_exact(&mut h)?;
            let fin = h[0] & 0x80 != 0;
            let opcode = h[0] & 0x0f;
            let masked = h[1] & 0x80 != 0;
            let mut n = (h[1] & 0x7f) as usize;
            if n == 126 {
                let mut e = [0u8; 2];
                self.s.read_exact(&mut e)?;
                n = u16::from_be_bytes(e) as usize;
            } else if n == 127 {
                let mut e = [0u8; 8];
                self.s.read_exact(&mut e)?;
                n = u64::from_be_bytes(e) as usize;
            }
            if n > MAX_WS_MESSAGE {
                return Err(bad("message too large"));
            }
            let mut mask = [0u8; 4];
            if masked {
                self.s.read_exact(&mut mask)?;
            }
            let mut p = vec![0u8; n];
            self.s.read_exact(&mut p)?;
            if masked {
                for (i, b) in p.iter_mut().enumerate() {
                    *b ^= mask[i % 4];
                }
            }
            match opcode {
                0x9 => self.write_frame(0xA, &p)?, // ping -> pong
                0xA => {}
                0x8 => return Err(io::Error::new(io::ErrorKind::ConnectionAborted, "WebSocket closed")),
                0x0 | 0x1 | 0x2 => {
                    self.partial.extend_from_slice(&p);
                    if self.partial.len() > MAX_WS_MESSAGE {
                        return Err(bad("message too large"));
                    }
                    if fin {
                        return Ok(std::mem::take(&mut self.partial));
                    }
                }
                _ => return Err(bad("unknown WebSocket opcode")),
            }
        }
    }

}

impl Transport for Wss {
    fn send(&mut self, body: &[u8]) -> io::Result<()> {
        if body.len() > MAX_FRAME {
            return Err(bad("frame too large"));
        }
        let mut b = Vec::with_capacity(4 + body.len());
        b.extend_from_slice(&(body.len() as u32).to_be_bytes());
        b.extend_from_slice(body);
        self.write_frame(0x2, &b)
    }

    fn recv(&mut self) -> io::Result<Vec<u8>> {
        loop {
            if self.inbox.len() >= 4 {
                let n = u32::from_be_bytes(self.inbox[..4].try_into().unwrap()) as usize;
                if n > MAX_INBOUND {
                    return Err(bad("frame too large"));
                }
                if self.inbox.len() >= 4 + n {
                    let body = self.inbox[4..4 + n].to_vec();
                    self.inbox.drain(..4 + n);
                    return Ok(body);
                }
            }
            let m = self.message()?;
            self.inbox.extend_from_slice(&m);
        }
    }

    fn set_read_timeout(&mut self, d: Duration) -> io::Result<()> {
        self.s.sock.set_read_timeout(Some(d))
    }
    fn wait_readable(&mut self, d: Duration) -> io::Result<bool> {
        if !self.inbox.is_empty() || !self.partial.is_empty() {
            return Ok(true);
        }
        if let Ok(st) = self.s.conn.process_new_packets() {
            if st.plaintext_bytes_to_read() > 0 {
                return Ok(true);
            }
        }
        peek_tcp(&self.s.sock, d)
    }

    fn describe(&self) -> String {
        self.url.clone()
    }
}

// ---------------- WebSocket, accepted (wss://host/mesh with TLS ended by a proxy in front) ----------------

/// The answer to a WebSocket upgrade's Sec-WebSocket-Key (RFC 6455 §4.2.2).
pub fn ws_accept_key(key: &str) -> String {
    use base64::Engine as _;
    use sha1::{Digest, Sha1};
    let mut h = Sha1::new();
    h.update(key.trim().as_bytes());
    h.update(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
    base64::engine::general_purpose::STANDARD.encode(h.finalize())
}

/// The server side of [`Wss`]: an upgrade answered on a plain TCP connection (a proxy in front holds the TLS), then
/// the link's byte stream (u32 BE length + body, as on TCP) in binary messages. The client's frames must be masked;
/// ours aren't (RFC 6455 §5.1).
pub struct WsServer {
    s: TcpStream,
    /// Bytes read past the HTTP head before the upgrade was answered, read first.
    pre: Vec<u8>,
    desc: String,
    partial: Vec<u8>,
    inbox: Vec<u8>,
}

impl WsServer {
    /// Answer the upgrade (its Sec-WebSocket-Key `key`, the head already read); `pre`: bytes read past the head.
    pub fn accept(mut s: TcpStream, key: &str, pre: Vec<u8>, desc: String) -> io::Result<WsServer> {
        if key.trim().is_empty() || key.len() > 64 {
            return Err(bad("no Sec-WebSocket-Key"));
        }
        s.set_nodelay(true)?;
        s.set_write_timeout(Some(Duration::from_secs(10)))?;
        let head = format!(
            "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {}\r\n\r\n",
            ws_accept_key(key)
        );
        s.write_all(head.as_bytes())?;
        Ok(WsServer { s, pre, desc, partial: Vec::new(), inbox: Vec::new() })
    }

    fn read_exact(&mut self, buf: &mut [u8]) -> io::Result<()> {
        let n = buf.len().min(self.pre.len());
        buf[..n].copy_from_slice(&self.pre[..n]);
        self.pre.drain(..n);
        if n < buf.len() {
            self.s.read_exact(&mut buf[n..])?;
        }
        Ok(())
    }

    fn write_frame(&mut self, opcode: u8, payload: &[u8]) -> io::Result<()> {
        let mut f = Vec::with_capacity(10 + payload.len());
        f.push(0x80 | opcode); // FIN, not masked
        let n = payload.len();
        if n < 126 {
            f.push(n as u8);
        } else if n <= 0xffff {
            f.push(126);
            f.extend_from_slice(&(n as u16).to_be_bytes());
        } else {
            f.push(127);
            f.extend_from_slice(&(n as u64).to_be_bytes());
        }
        f.extend_from_slice(payload);
        self.s.write_all(&f)?;
        self.s.flush()
    }

    /// Next whole WebSocket message (pings answered, a close ends it).
    fn message(&mut self) -> io::Result<Vec<u8>> {
        loop {
            let mut h = [0u8; 2];
            self.read_exact(&mut h)?;
            let fin = h[0] & 0x80 != 0;
            let opcode = h[0] & 0x0f;
            if h[1] & 0x80 == 0 {
                return Err(bad("unmasked frame from a client"));
            }
            let mut n = (h[1] & 0x7f) as usize;
            if n == 126 {
                let mut e = [0u8; 2];
                self.read_exact(&mut e)?;
                n = u16::from_be_bytes(e) as usize;
            } else if n == 127 {
                let mut e = [0u8; 8];
                self.read_exact(&mut e)?;
                n = usize::try_from(u64::from_be_bytes(e)).unwrap_or(usize::MAX);
            }
            if n > MAX_WS_MESSAGE {
                return Err(bad("message too large"));
            }
            let mut mask = [0u8; 4];
            self.read_exact(&mut mask)?;
            let mut p = vec![0u8; n];
            self.read_exact(&mut p)?;
            for (i, b) in p.iter_mut().enumerate() {
                *b ^= mask[i % 4];
            }
            match opcode {
                0x9 => self.write_frame(0xA, &p)?,
                0xA => {}
                0x8 => return Err(io::Error::new(io::ErrorKind::ConnectionAborted, "WebSocket closed")),
                0x0 | 0x1 | 0x2 => {
                    self.partial.extend_from_slice(&p);
                    if self.partial.len() > MAX_WS_MESSAGE {
                        return Err(bad("message too large"));
                    }
                    if fin {
                        return Ok(std::mem::take(&mut self.partial));
                    }
                }
                _ => return Err(bad("unknown WebSocket opcode")),
            }
        }
    }
}

impl Transport for WsServer {
    fn send(&mut self, body: &[u8]) -> io::Result<()> {
        if body.len() > MAX_FRAME {
            return Err(bad("frame too large"));
        }
        let mut b = Vec::with_capacity(4 + body.len());
        b.extend_from_slice(&(body.len() as u32).to_be_bytes());
        b.extend_from_slice(body);
        self.write_frame(0x2, &b)
    }

    fn recv(&mut self) -> io::Result<Vec<u8>> {
        loop {
            if self.inbox.len() >= 4 {
                let n = u32::from_be_bytes(self.inbox[..4].try_into().unwrap()) as usize;
                if n > MAX_INBOUND {
                    return Err(bad("frame too large"));
                }
                if self.inbox.len() >= 4 + n {
                    let body = self.inbox[4..4 + n].to_vec();
                    self.inbox.drain(..4 + n);
                    return Ok(body);
                }
            }
            let m = self.message()?;
            self.inbox.extend_from_slice(&m);
            if self.inbox.len() > MAX_INBOUND + 4 {
                return Err(bad("frame too large"));
            }
        }
    }

    fn set_read_timeout(&mut self, d: Duration) -> io::Result<()> {
        self.s.set_read_timeout(Some(d))
    }

    fn wait_readable(&mut self, d: Duration) -> io::Result<bool> {
        if !self.inbox.is_empty() || !self.partial.is_empty() || !self.pre.is_empty() {
            return Ok(true);
        }
        peek_tcp(&self.s, d)
    }

    fn describe(&self) -> String {
        self.desc.clone()
    }
}

// ---------------- plain HTTPS GET (for public old-arcade routes) ----------------

/// GET `https://host/path` and return (status, body). HTTP/1.0 so the body is never chunked.
/// Bodies are capped at `max` bytes.
pub fn https_get(url: &str, timeout: Duration, max: usize) -> io::Result<(u16, Vec<u8>)> {
    let rest = url.strip_prefix("https://").ok_or_else(|| bad("not an https:// URL"))?;
    let (host, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let sa = (host, 443u16).to_socket_addrs()?.next().ok_or_else(|| bad("cannot resolve host"))?;
    let tcp = TcpStream::connect_timeout(&sa, timeout)?;
    tcp.set_read_timeout(Some(timeout))?;
    tcp.set_write_timeout(Some(timeout))?;
    let name = rustls::pki_types::ServerName::try_from(host.to_string()).map_err(|_| bad("bad host name"))?;
    let conn = rustls::ClientConnection::new(tls_config(), name).map_err(|e| bad(&e.to_string()))?;
    let mut s = rustls::StreamOwned::new(conn, tcp);
    let req = format!("GET {path} HTTP/1.0\r\nHost: {host}\r\nUser-Agent: arcade/1.0\r\nAccept: application/json\r\nConnection: close\r\n\r\n");
    s.write_all(req.as_bytes())?;
    let mut all = Vec::new();
    let mut buf = [0u8; 16384];
    loop {
        match s.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                all.extend_from_slice(&buf[..n]);
                if all.len() > max + 16384 {
                    return Err(bad("response too large"));
                }
            }
            // Servers often close TLS without a close_notify; what we have is the answer.
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e),
        }
    }
    let split = all.windows(4).position(|w| w == b"\r\n\r\n").ok_or_else(|| bad("no HTTP head"))?;
    let head = String::from_utf8_lossy(&all[..split]);
    let status: u16 = head.split_whitespace().nth(1).and_then(|x| x.parse().ok()).ok_or_else(|| bad("bad status line"))?;
    Ok((status, all[split + 4..].to_vec()))
}

#[cfg(test)]
mod ws_server_tests {
    use super::*;

    /// RFC 6455 §1.3's example key and answer.
    #[test]
    fn the_accept_key_is_the_rfcs() {
        assert_eq!(ws_accept_key("dGhlIHNhbXBsZSBub25jZQ=="), "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
    }

    /// A client's masked binary messages carrying the link stream come out as its frames; ours go back unmasked; an
    /// unmasked client frame is refused.
    #[test]
    fn frames_both_ways() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let client = std::thread::spawn(move || {
            let mut c = TcpStream::connect(addr).unwrap();
            let mut head = Vec::new();
            let mut b = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                c.read_exact(&mut b).unwrap();
                head.push(b[0]);
            }
            assert!(String::from_utf8_lossy(&head).contains("Sec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo="));
            // Two link frames in one masked message: "hi", "there".
            let mut stream = Vec::new();
            for body in [&b"hi"[..], b"there"] {
                stream.extend_from_slice(&(body.len() as u32).to_be_bytes());
                stream.extend_from_slice(body);
            }
            let mask = [1u8, 2, 3, 4];
            let mut f = vec![0x82, 0x80 | stream.len() as u8];
            f.extend_from_slice(&mask);
            f.extend(stream.iter().enumerate().map(|(i, x)| x ^ mask[i % 4]));
            c.write_all(&f).unwrap();
            // Our answer: one unmasked binary message, length + "ok".
            let mut h = [0u8; 2];
            c.read_exact(&mut h).unwrap();
            assert_eq!((h[0], h[1]), (0x82, 6));
            let mut p = [0u8; 6];
            c.read_exact(&mut p).unwrap();
            assert_eq!(&p, b"\0\0\0\x02ok");
            // An unmasked frame is refused.
            c.write_all(&[0x82, 0x01, 0x00]).unwrap();
        });
        let (s, _) = l.accept().unwrap();
        let mut w = WsServer::accept(s, "dGhlIHNhbXBsZSBub25jZQ==", Vec::new(), "test".into()).unwrap();
        w.set_read_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(w.recv().unwrap(), b"hi");
        assert_eq!(w.recv().unwrap(), b"there");
        w.send(b"ok").unwrap();
        assert!(w.recv().is_err(), "unmasked client frame");
        client.join().unwrap();
    }
}
