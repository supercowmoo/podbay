//! The network allowlist, enforced on the host.
//!
//! A session with an allowlist shares the network namespace of a relay
//! container that has no network either (see `relay/`), so the only thing
//! it can reach is the relay's proxy port on loopback. The relay passes
//! every connection to a Unix socket served here, and this module decides:
//! it reads an HTTP `CONNECT host:port` request, checks the name against
//! the allowlist, resolves it, refuses addresses that aren't on the public
//! internet, then connects and copies bytes both ways.
//!
//! Everything a session sends is untrusted. Requests are bounded in size
//! and time, only names on the allowlist are ever resolved (so the session
//! can't leak data through DNS lookups of names it makes up), and IP
//! literals are refused outright.

use std::collections::BTreeMap;
use std::fmt;
use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, Shutdown, SocketAddr, TcpStream, ToSocketAddrs};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::report::printable;

/// Where the relay listens, inside the session's network namespace.
pub const PROXY_ADDR: &str = "127.0.0.1:3128";
/// The port of [`PROXY_ADDR`].
pub const PROXY_PORT: u16 = 3128;
/// What the relay sends once it is listening; see `relay/src/main.rs`.
const READY: &[u8] = b"podbay-relay ready\n";
/// The longest request head accepted.
const MAX_HEAD: usize = 8 << 10;
/// How long a client gets to send its request.
const HEAD_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Connections beyond this are refused.
const MAX_CONNECTIONS: usize = 256;
/// Distinct destinations remembered for the summary.
const MAX_LOGGED: usize = 1_000;

/// One allowlist entry: `example.com`, or `*.example.com` for every name
/// below it, optionally with `:port` (default 443).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
pub struct HostPattern {
    /// Lowercase, without the `*.`.
    name: String,
    wildcard: bool,
    port: u16,
}

impl HostPattern {
    pub fn parse(entry: &str) -> Result<Self> {
        let (host, port) = match entry.rsplit_once(':') {
            Some((host, port)) => {
                let port = port
                    .parse::<u16>()
                    .ok()
                    .filter(|p| *p != 0)
                    .with_context(|| format!("invalid port in allowed host {entry:?}"))?;
                (host, port)
            }
            None => (entry, 443),
        };
        let (name, wildcard) = match host.strip_prefix("*.") {
            Some(rest) => (rest, true),
            None => (host, false),
        };
        let name = name.to_ascii_lowercase();
        if !is_hostname(&name) {
            bail!(
                "invalid allowed host {entry:?}: use a DNS name such as \"api.example.com\" or \
                 \"*.example.com\", optionally with \":port\"; IP addresses aren't allowed"
            );
        }
        Ok(Self {
            name,
            wildcard,
            port,
        })
    }

    fn matches(&self, host: &str, port: u16) -> bool {
        port == self.port
            && match self.wildcard {
                false => host == self.name,
                true => host
                    .strip_suffix(&self.name)
                    .is_some_and(|sub| sub.len() > 1 && sub.ends_with('.')),
            }
    }
}

impl TryFrom<String> for HostPattern {
    type Error = anyhow::Error;
    fn try_from(entry: String) -> Result<Self> {
        Self::parse(&entry)
    }
}

impl fmt::Display for HostPattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.wildcard {
            f.write_str("*.")?;
        }
        f.write_str(&self.name)?;
        if self.port != 443 {
            write!(f, ":{}", self.port)?;
        }
        Ok(())
    }
}

/// A DNS name, lowercase, with at least one letter in its last label so
/// nothing that a resolver might read as an IPv4 address (`127.1`,
/// `0x7f.1`) passes.
fn is_hostname(name: &str) -> bool {
    let labels: Vec<&str> = name.split('.').collect();
    name.len() <= 253
        && labels.iter().all(|l| {
            !l.is_empty()
                && l.len() <= 63
                && !l.starts_with('-')
                && !l.ends_with('-')
                && l.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
        && labels.last().is_some_and(|l| {
            l.bytes().any(|b| b.is_ascii_lowercase())
                && !(l.starts_with("0x") && l[2..].bytes().all(|b| b.is_ascii_hexdigit()))
        })
}

/// Whether connections to `ip` can leave the machine for the public
/// internet. Everything local, private or reserved is refused: the host's
/// loopback, the LAN, cloud metadata services, and the IPv6 forms that
/// embed such an IPv4 address.
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => is_public_v6(v6),
    }
}

fn is_public_v4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(a == 0                                   // "this network"
        || a == 10                             // private
        || a == 127                            // loopback
        || (a == 100 && (64..128).contains(&b)) // shared address space (CGNAT)
        || (a == 169 && b == 254)              // link-local, cloud metadata
        || (a == 172 && (16..32).contains(&b)) // private
        || (a == 192 && b == 0 && c == 0)      // IETF protocol assignments
        || (a == 192 && b == 0 && c == 2)      // documentation
        || (a == 192 && b == 88 && c == 99)    // 6to4 relay anycast
        || (a == 192 && b == 168)              // private
        || (a == 198 && (18..20).contains(&b)) // benchmarking
        || (a == 198 && b == 51 && c == 100)   // documentation
        || (a == 203 && b == 0 && c == 113)    // documentation
        || a >= 224) // multicast, reserved, broadcast
}

fn is_public_v6(ip: Ipv6Addr) -> bool {
    let s = ip.segments();
    let embedded =
        |hi: u16, lo: u16| Ipv4Addr::new((hi >> 8) as u8, hi as u8, (lo >> 8) as u8, lo as u8);
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_public_v4(v4);
    }
    match s {
        // NAT64 well-known prefix: the IPv4 address is in the last 32 bits.
        [0x64, 0xff9b, 0, 0, 0, 0, hi, lo] => is_public_v4(embedded(hi, lo)),
        // 6to4: the IPv4 address follows the prefix.
        [0x2002, hi, lo, ..] => is_public_v4(embedded(hi, lo)),
        _ => {
            !(ip.is_unspecified()
                || ip.is_loopback()
                || ip.is_multicast()
                || s[0..6] == [0; 6]                  // IPv4-compatible, deprecated
                || (s[0] == 0x64 && s[1] == 0xff9b)  // NAT64 local use
                || (s[0] == 0x100 && s[1..4] == [0; 3]) // discard
                || (s[0] & 0xfe00) == 0xfc00          // unique local
                || (s[0] & 0xffc0) == 0xfe80          // link-local
                || (s[0] & 0xffc0) == 0xfec0          // site-local, deprecated
                || (s[0] == 0x2001 && s[1] < 0x200)   // Teredo, benchmarking, ORCHID, ...
                || (s[0] == 0x2001 && s[1] == 0xdb8)) // documentation
        }
    }
}

/// What the session asked for, and what happened.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Outcome {
    Allowed,
    Denied(&'static str),
    Failed(&'static str),
}

#[derive(Default)]
struct Log {
    /// Destination (as the session wrote it) and outcome, with a count.
    entries: BTreeMap<(String, Outcome), usize>,
    dropped: usize,
}

impl Log {
    fn record(&mut self, target: &str, outcome: Outcome) {
        let key = (target.to_owned(), outcome);
        if let Some(count) = self.entries.get_mut(&key) {
            *count += 1;
        } else if self.entries.len() < MAX_LOGGED {
            self.entries.insert(key, 1);
        } else {
            self.dropped += 1;
        }
    }
}

struct Shared {
    allow: Vec<HostPattern>,
    /// Tests reach servers on loopback; real sessions never can.
    allow_private: bool,
    log: Mutex<Log>,
    ready: (Mutex<bool>, Condvar),
    active: AtomicUsize,
}

/// The host side of an allowlisted session: a Unix socket in a private
/// directory, served by background threads until this is dropped.
pub struct Server {
    dir: tempfile::TempDir,
    shared: Arc<Shared>,
}

/// A new private directory for Unix sockets shared with the relay.
/// `$XDG_RUNTIME_DIR` is private to the user; the directory itself is
/// created 0700 either way. Socket paths are limited to 108 bytes.
pub fn socket_dir() -> Result<tempfile::TempDir> {
    let parent = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute() && p.is_dir() && p.as_os_str().len() < 64)
        .unwrap_or_else(std::env::temp_dir);
    tempfile::Builder::new()
        .prefix("podbay-")
        .tempdir_in(&parent)
        .with_context(|| format!("creating a directory in {}", parent.display()))
}

/// The socket's name inside [`Server::dir`].
pub const SOCKET_NAME: &str = "egress.sock";

impl Server {
    pub fn start(allow: Vec<HostPattern>) -> Result<Self> {
        Self::start_with(allow, false)
    }

    fn start_with(allow: Vec<HostPattern>, allow_private: bool) -> Result<Self> {
        let dir = socket_dir()?;
        let path = dir.path().join(SOCKET_NAME);
        let listener = UnixListener::bind(&path)
            .with_context(|| format!("listening on {}", path.display()))?;
        let shared = Arc::new(Shared {
            allow,
            allow_private,
            log: Mutex::default(),
            ready: (Mutex::new(false), Condvar::new()),
            active: AtomicUsize::new(0),
        });
        let accept = Arc::clone(&shared);
        thread::Builder::new()
            .spawn(move || {
                for conn in listener.incoming() {
                    let Ok(conn) = conn else { continue };
                    if accept.active.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
                        accept.active.fetch_sub(1, Ordering::SeqCst);
                        let _ = respond(&conn, 503, "too many connections");
                        continue;
                    }
                    let shared = Arc::clone(&accept);
                    let spawned = thread::Builder::new().spawn(move || {
                        handle(conn, &shared);
                        shared.active.fetch_sub(1, Ordering::SeqCst);
                    });
                    if spawned.is_err() {
                        accept.active.fetch_sub(1, Ordering::SeqCst);
                    }
                }
            })
            .context("starting the network policy server")?;
        Ok(Self { dir, shared })
    }

    /// The directory holding the socket, to mount into the relay.
    pub fn dir(&self) -> &Path {
        self.dir.path()
    }

    /// Waits for the relay to say it is listening.
    pub fn wait_ready(&self, timeout: Duration) -> bool {
        let (lock, cond) = &self.shared.ready;
        let ready = lock.lock().unwrap_or_else(|e| e.into_inner());
        let (ready, _) = cond
            .wait_timeout_while(ready, timeout, |ready| !*ready)
            .unwrap_or_else(|e| e.into_inner());
        *ready
    }

    /// One line per destination the session tried, for after the session.
    /// Destinations come from the session, so they are escaped.
    pub fn summary(&self) -> String {
        let log = self.shared.log.lock().unwrap_or_else(|e| e.into_inner());
        if log.entries.is_empty() {
            return "podbay: network: no connections through the allowlist\n".into();
        }
        let mut out = String::from("podbay: network:\n");
        for ((target, outcome), count) in &log.entries {
            let what = match outcome {
                Outcome::Allowed => "allowed".to_owned(),
                Outcome::Denied(why) => format!("denied, {why}"),
                Outcome::Failed(why) => format!("failed, {why}"),
            };
            let times = if *count > 1 {
                format!(" ({count} times)")
            } else {
                String::new()
            };
            out.push_str(&format!("  {}: {what}{times}\n", printable(target)));
        }
        if log.dropped > 0 {
            out.push_str(&format!("  ... and {} more\n", log.dropped));
        }
        out
    }
}

fn handle(mut conn: UnixStream, shared: &Shared) {
    let _ = conn.set_read_timeout(Some(HEAD_TIMEOUT));
    let Ok((head, rest)) = read_head(&mut conn) else {
        return;
    };
    if head.as_bytes() == READY {
        let (lock, cond) = &shared.ready;
        *lock.lock().unwrap_or_else(|e| e.into_inner()) = true;
        cond.notify_all();
        return;
    }
    let log = |target: &str, outcome| {
        let mut log = shared.log.lock().unwrap_or_else(|e| e.into_inner());
        log.record(target, outcome);
    };

    let request = match parse_connect(&head) {
        Ok(request) => request,
        Err(Rejection {
            status,
            reason,
            target,
        }) => {
            log(&target, Outcome::Denied(reason));
            let _ = respond(&conn, status, reason);
            return;
        }
    };
    let target = format!("{}:{}", request.host, request.port);
    let upstream = match connect(&request, shared) {
        Ok(upstream) => upstream,
        Err(Rejection { status, reason, .. }) => {
            let outcome = match status {
                403 => Outcome::Denied(reason),
                _ => Outcome::Failed(reason),
            };
            log(&target, outcome);
            let _ = respond(&conn, status, reason);
            return;
        }
    };
    log(&target, Outcome::Allowed);
    if conn
        .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
        .is_err()
    {
        return;
    }
    let _ = conn.set_read_timeout(None);
    let _ = splice(conn, upstream, &rest);
}

/// Reads up to the blank line ending the request head. Returns the head and
/// any bytes the client sent after it.
fn read_head(conn: &mut UnixStream) -> io::Result<(String, Vec<u8>)> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        if buf == READY {
            return Ok((String::from_utf8_lossy(READY).into_owned(), Vec::new()));
        }
        if let Some(end) = find_end(&buf) {
            let rest = buf.split_off(end);
            return Ok((String::from_utf8_lossy(&buf).into_owned(), rest));
        }
        if buf.len() >= MAX_HEAD {
            return Err(io::Error::other("request head too long"));
        }
        let n = conn.read(&mut chunk)?;
        if n == 0 {
            if buf.is_empty() {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            return Ok((String::from_utf8_lossy(&buf).into_owned(), Vec::new()));
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

fn find_end(buf: &[u8]) -> Option<usize> {
    let crlf = buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4);
    let lf = buf.windows(2).position(|w| w == b"\n\n").map(|i| i + 2);
    match (crlf, lf) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Connect {
    host: String,
    port: u16,
}

#[derive(Debug, PartialEq, Eq)]
struct Rejection {
    status: u16,
    reason: &'static str,
    /// What the session asked for, for the log.
    target: String,
}

fn reject(status: u16, reason: &'static str, target: &str) -> Rejection {
    Rejection {
        status,
        reason,
        target: target.to_owned(),
    }
}

fn parse_connect(head: &str) -> Result<Connect, Rejection> {
    let line = head.lines().next().unwrap_or_default();
    let mut words = line.split(' ');
    let (Some(method), Some(target), Some(version), None) =
        (words.next(), words.next(), words.next(), words.next())
    else {
        return Err(reject(400, "malformed request", line));
    };
    if !version.starts_with("HTTP/1.") {
        return Err(reject(400, "malformed request", line));
    }
    if method != "CONNECT" {
        // Plain-HTTP proxy requests carry a full URL; log just the host.
        let shown = target
            .split_once("://")
            .map_or(target, |(_, rest)| rest.split('/').next().unwrap_or(rest));
        return Err(reject(405, "only HTTPS (CONNECT) is supported", shown));
    }
    let Some((host, port)) = target.rsplit_once(':') else {
        return Err(reject(400, "the target needs a port", target));
    };
    let Some(port) = port.parse::<u16>().ok().filter(|p| *p != 0) else {
        return Err(reject(400, "invalid port", target));
    };
    let host = host.to_ascii_lowercase();
    let host = host.strip_suffix('.').unwrap_or(&host);
    if host.starts_with('[') || host.parse::<IpAddr>().is_ok() {
        return Err(reject(
            403,
            "IP addresses are not allowed, only names",
            target,
        ));
    }
    if !is_hostname(host) {
        return Err(reject(403, "not a valid host name", target));
    }
    Ok(Connect {
        host: host.to_owned(),
        port,
    })
}

fn connect(request: &Connect, shared: &Shared) -> Result<TcpStream, Rejection> {
    let target = format!("{}:{}", request.host, request.port);
    if !shared
        .allow
        .iter()
        .any(|p| p.matches(&request.host, request.port))
    {
        return Err(reject(403, "not on the allowlist", &target));
    }
    let addrs: Vec<SocketAddr> = (request.host.as_str(), request.port)
        .to_socket_addrs()
        .map_err(|_| reject(502, "the name did not resolve", &target))?
        .collect();
    let usable: Vec<&SocketAddr> = addrs
        .iter()
        .filter(|a| shared.allow_private || is_public(a.ip()))
        .collect();
    if usable.is_empty() {
        return Err(reject(
            403,
            "it resolves only to private or local addresses",
            &target,
        ));
    }
    for addr in usable {
        if let Ok(stream) = TcpStream::connect_timeout(addr, CONNECT_TIMEOUT) {
            return Ok(stream);
        }
    }
    Err(reject(502, "the connection failed", &target))
}

fn respond(mut conn: &UnixStream, status: u16, reason: &str) -> io::Result<()> {
    let text = match status {
        400 => "Bad Request",
        403 => "Forbidden",
        405 => "Method Not Allowed",
        502 => "Bad Gateway",
        _ => "Service Unavailable",
    };
    let body = format!("podbay: {reason}\n");
    write!(
        conn,
        "HTTP/1.1 {status} {text}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    )
}

/// Copies both ways until both sides are done, passing on half-closes.
/// `early` is what the client sent after its request head.
fn splice(client: UnixStream, mut upstream: TcpStream, early: &[u8]) -> io::Result<()> {
    upstream.write_all(early)?;
    let (client_in, upstream_out) = (client.try_clone()?, upstream.try_clone()?);
    let up = thread::Builder::new().spawn(move || {
        let _ = io::copy(&mut &client_in, &mut &upstream_out);
        let _ = upstream_out.shutdown(Shutdown::Write);
    })?;
    let _ = io::copy(&mut &upstream, &mut &client);
    let _ = client.shutdown(Shutdown::Write);
    let _ = up.join();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    fn pattern(s: &str) -> HostPattern {
        HostPattern::parse(s).unwrap()
    }

    #[test]
    fn patterns() {
        let p = pattern("API.Example.com");
        assert!(p.matches("api.example.com", 443));
        assert!(!p.matches("api.example.com", 80));
        assert!(!p.matches("x.api.example.com", 443));
        assert_eq!(p.to_string(), "api.example.com");

        let w = pattern("*.example.com:8443");
        assert!(w.matches("a.example.com", 8443));
        assert!(w.matches("a.b.example.com", 8443));
        assert!(!w.matches("example.com", 8443));
        assert!(!w.matches("badexample.com", 8443));
        assert!(!w.matches("a.example.com", 443));
        assert_eq!(w.to_string(), "*.example.com:8443");

        for bad in [
            "",
            "*",
            "*.com.",
            "1.2.3.4",
            "127.1",
            "0x7f.1",
            "[::1]",
            "::1",
            "a..b",
            "-a.com",
            "a_b.com",
            "a.com:0",
            "a.com:x",
            "a.com:70000",
            "a.*.com",
            "http://a.com",
        ] {
            assert!(HostPattern::parse(bad).is_err(), "{bad:?} accepted");
        }
    }

    #[test]
    fn public_addresses() {
        for ip in [
            "1.1.1.1",
            "8.8.8.8",
            "2606:4700::1111",
            "::ffff:1.1.1.1",
            "64:ff9b::101:101",
            "2002:101:101::1",
        ] {
            assert!(is_public(ip.parse().unwrap()), "{ip} refused");
        }
        for ip in [
            "0.0.0.0",
            "10.1.2.3",
            "100.64.0.1",
            "127.0.0.1",
            "169.254.169.254",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "198.18.0.1",
            "224.0.0.1",
            "255.255.255.255",
            "::",
            "::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "::127.0.0.1",
            "64:ff9b::7f00:1",
            "64:ff9b:1::1",
            "2002:a00:1::1",
            "2001::1",
            "2001:db8::1",
            "fc00::1",
            "fd12::1",
            "fe80::1",
            "fec0::1",
            "ff02::1",
        ] {
            assert!(!is_public(ip.parse().unwrap()), "{ip} allowed");
        }
    }

    #[test]
    fn requests() {
        assert_eq!(
            parse_connect("CONNECT Api.Example.com.:443 HTTP/1.1\r\nHost: x\r\n\r\n"),
            Ok(Connect {
                host: "api.example.com".into(),
                port: 443
            })
        );
        let status = |head: &str| parse_connect(head).unwrap_err().status;
        assert_eq!(status("CONNECT 1.2.3.4:443 HTTP/1.1\r\n\r\n"), 403);
        assert_eq!(status("CONNECT [::1]:443 HTTP/1.1\r\n\r\n"), 403);
        assert_eq!(status("CONNECT 127.1:443 HTTP/1.1\r\n\r\n"), 403);
        assert_eq!(status("CONNECT a.com HTTP/1.1\r\n\r\n"), 400);
        assert_eq!(status("CONNECT a.com:0 HTTP/1.1\r\n\r\n"), 400);
        assert_eq!(status("CONNECT a.com:443 SMTP\r\n\r\n"), 400);
        let get = parse_connect("GET http://a.com/x HTTP/1.1\r\n\r\n").unwrap_err();
        assert_eq!((get.status, get.target.as_str()), (405, "a.com"));
    }

    /// Sends `head` through the server's socket and returns the status line
    /// and the connection.
    fn request(server: &Server, head: &str) -> (String, UnixStream) {
        let mut conn = UnixStream::connect(server.dir().join(SOCKET_NAME)).unwrap();
        conn.write_all(head.as_bytes()).unwrap();
        let mut line = Vec::new();
        let mut byte = [0u8];
        while !line.ends_with(b"\r\n") {
            conn.read_exact(&mut byte).unwrap();
            line.push(byte[0]);
        }
        (String::from_utf8(line).unwrap(), conn)
    }

    #[test]
    fn tunnels_to_allowed_hosts_only() {
        let upstream = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = upstream.local_addr().unwrap().port();
        thread::spawn(move || {
            let (mut conn, _) = upstream.accept().unwrap();
            let mut buf = [0u8; 5];
            conn.read_exact(&mut buf).unwrap();
            conn.write_all(&buf).unwrap();
        });

        let allow = vec![pattern(&format!("localhost:{port}"))];
        let server = Server::start_with(allow.clone(), true).unwrap();

        let (status, mut conn) = request(
            &server,
            &format!("CONNECT localhost:{port} HTTP/1.1\r\n\r\nhello"),
        );
        assert_eq!(status, "HTTP/1.1 200 Connection established\r\n");
        let mut rest = Vec::new();
        conn.read_to_end(&mut rest).unwrap();
        assert_eq!(rest, b"\r\nhello");

        let (status, _) = request(&server, "CONNECT evil.example:443 HTTP/1.1\r\n\r\n");
        assert!(status.starts_with("HTTP/1.1 403"), "{status}");
        let (status, _) = request(&server, "GET http://a.com/ HTTP/1.1\r\n\r\n");
        assert!(status.starts_with("HTTP/1.1 405"), "{status}");

        let summary = server.summary();
        assert!(
            summary.contains(&format!("localhost:{port}: allowed")),
            "{summary}"
        );
        assert!(
            summary.contains("evil.example:443: denied, not on the allowlist"),
            "{summary}"
        );

        // Without the test-only exception, loopback is refused even when
        // the name is allowed.
        let strict = Server::start(allow).unwrap();
        let (status, _) = request(
            &strict,
            &format!("CONNECT localhost:{port} HTTP/1.1\r\n\r\n"),
        );
        assert!(status.starts_with("HTTP/1.1 403"), "{status}");
        assert!(
            strict.summary().contains("private or local addresses"),
            "{}",
            strict.summary()
        );
    }

    #[test]
    fn ready_signal_and_escaping() {
        let server = Server::start(vec![]).unwrap();
        assert!(!server.wait_ready(Duration::from_millis(10)));
        UnixStream::connect(server.dir().join(SOCKET_NAME))
            .unwrap()
            .write_all(READY)
            .unwrap();
        assert!(server.wait_ready(Duration::from_secs(5)));

        let (status, _) = request(&server, "CONNECT \x1b[2Jx.com:443 HTTP/1.1\r\n\r\n");
        assert!(status.starts_with("HTTP/1.1 403"), "{status}");
        assert!(!server.summary().contains('\x1b'), "{}", server.summary());
    }
}
