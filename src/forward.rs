//! Forwarded ports: a server in the session, reachable from the host.
//!
//! A session's network namespace has nothing the engine could publish a
//! port from, and servers in it usually listen on loopback only. So podbay
//! listens on the host's loopback instead, and passes each connection
//! through a Unix socket to the relay container, which shares the
//! session's network namespace and connects to the port there (see
//! `relay/`).
//!
//! Connections only go one way. The session can't open anything through a
//! forward, and the relay connects only to the port it was started with.

use std::fmt;
use std::io::{self, ErrorKind};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// Connections beyond this, per port, are closed straight away.
const MAX_CONNECTIONS: usize = 256;

/// `3080` (the same port on both sides) or `"8080:3080"` (host port, then
/// the port in the session).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(try_from = "PortEntry")]
pub struct Port {
    pub host: u16,
    pub container: u16,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum PortEntry {
    Number(i64),
    Text(String),
}

impl TryFrom<PortEntry> for Port {
    type Error = anyhow::Error;
    fn try_from(entry: PortEntry) -> Result<Self> {
        match entry {
            PortEntry::Number(n) => Self::parse(&n.to_string()),
            PortEntry::Text(text) => Self::parse(&text),
        }
    }
}

impl Port {
    pub fn parse(spec: &str) -> Result<Self> {
        let number = |s: &str| {
            s.parse::<u16>()
                .ok()
                .filter(|p| *p != 0)
                .with_context(|| format!("invalid port {spec:?}: use 3080 or \"8080:3080\""))
        };
        Ok(match spec.split_once(':') {
            Some((host, container)) => Self {
                host: number(host)?,
                container: number(container)?,
            },
            None => {
                let port = number(spec)?;
                Self {
                    host: port,
                    container: port,
                }
            }
        })
    }
}

impl fmt::Display for Port {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.host == self.container {
            true => write!(f, "{}", self.host),
            false => write!(f, "{}:{}", self.host, self.container),
        }
    }
}

/// Adds `port` to `ports`, refusing a host port already taken by a
/// different forward. An identical one is kept once.
pub fn add(ports: &mut Vec<Port>, port: Port) -> Result<()> {
    match ports.iter().find(|p| p.host == port.host) {
        Some(existing) if *existing == port => Ok(()),
        Some(existing) => bail!(
            "host port {} is forwarded twice, to session ports {} and {}",
            port.host,
            existing.container,
            port.container
        ),
        None => {
            ports.push(port);
            Ok(())
        }
    }
}

/// The name of the relay's socket for a session port.
pub fn socket_name(container_port: u16) -> String {
    format!("{container_port}.sock")
}

struct Listener {
    port: Port,
    addr: SocketAddr,
    connections: Arc<AtomicUsize>,
}

/// The host side of the forwarded ports, listening from creation until
/// dropped.
pub struct Forwarder {
    dir: tempfile::TempDir,
    listeners: Vec<Listener>,
    stop: Arc<AtomicBool>,
}

impl Forwarder {
    /// Listens on the host's loopback for every port. Fails if a port is
    /// taken, before anything has started.
    pub fn start(ports: &[Port]) -> Result<Self> {
        let dir = crate::egress::socket_dir()?;
        let stop = Arc::new(AtomicBool::new(false));
        let mut listeners = Vec::new();
        for &port in ports {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port.host)).map_err(|e| {
                let why = match e.kind() {
                    ErrorKind::AddrInUse => "it is in use".to_owned(),
                    _ => e.to_string(),
                };
                anyhow::anyhow!(
                    "can't forward host port {}: {why}. Pick another with --port HOST:{}",
                    port.host,
                    port.container
                )
            })?;
            let addr = listener.local_addr()?;
            let connections = Arc::new(AtomicUsize::new(0));
            let socket = dir.path().join(socket_name(port.container));
            let (stop, count) = (Arc::clone(&stop), Arc::clone(&connections));
            thread::Builder::new()
                .spawn(move || accept(listener, &socket, &stop, &count))
                .context("starting the port forwarder")?;
            listeners.push(Listener {
                port,
                addr,
                connections,
            });
        }
        Ok(Self {
            dir,
            listeners,
            stop,
        })
    }

    /// The directory the relay creates its sockets in.
    pub fn dir(&self) -> &Path {
        self.dir.path()
    }

    /// Session ports, for the relay's arguments.
    pub fn container_ports(&self) -> Vec<u16> {
        self.listeners.iter().map(|l| l.port.container).collect()
    }

    /// Waits for the relay to create every socket.
    pub fn wait_ready(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self
                .listeners
                .iter()
                .all(|l| self.dir.path().join(socket_name(l.port.container)).exists())
            {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// One line per port, for when the session starts.
    pub fn describe(&self) -> String {
        self.listeners
            .iter()
            .map(|l| {
                format!(
                    "podbay: forwarding http://{} to session port {}\n",
                    l.addr, l.port.container
                )
            })
            .collect()
    }

    /// One line per port with its connection count, for after the session.
    pub fn summary(&self) -> String {
        let mut out = String::from("podbay: forwarded ports:\n");
        for l in &self.listeners {
            let n = l.connections.load(Ordering::SeqCst);
            let s = if n == 1 { "" } else { "s" };
            out.push_str(&format!(
                "  {} -> session port {}: {n} connection{s}\n",
                l.addr, l.port.container
            ));
        }
        out
    }
}

impl Drop for Forwarder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake each accept loop so it sees the flag and closes its port.
        for l in &self.listeners {
            let _ = TcpStream::connect_timeout(&l.addr, Duration::from_millis(200));
        }
    }
}

fn accept(listener: TcpListener, socket: &Path, stop: &AtomicBool, count: &Arc<AtomicUsize>) {
    let active = Arc::new(AtomicUsize::new(0));
    for client in listener.incoming() {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        let Ok(client) = client else { continue };
        if active.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
            active.fetch_sub(1, Ordering::SeqCst);
            continue;
        }
        count.fetch_add(1, Ordering::SeqCst);
        let socket: PathBuf = socket.to_owned();
        let done = Arc::clone(&active);
        let spawned = thread::Builder::new().spawn(move || {
            if let Ok(upstream) = UnixStream::connect(&socket) {
                let _ = pipe(client, upstream);
            }
            done.fetch_sub(1, Ordering::SeqCst);
        });
        if spawned.is_err() {
            active.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

/// Copies both ways until both sides are done, passing on half-closes.
fn pipe(client: TcpStream, upstream: UnixStream) -> io::Result<()> {
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
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;

    #[test]
    fn port_specs() {
        assert_eq!(
            Port::parse("3080").unwrap(),
            Port {
                host: 3080,
                container: 3080
            }
        );
        assert_eq!(
            Port::parse("8080:3080").unwrap(),
            Port {
                host: 8080,
                container: 3080
            }
        );
        assert_eq!(Port::parse("8080:3080").unwrap().to_string(), "8080:3080");
        assert_eq!(Port::parse("3080").unwrap().to_string(), "3080");
        for bad in [
            "",
            "0",
            "65536",
            "x",
            ":3080",
            "8080:",
            "1:2:3",
            "-1",
            "127.0.0.1:80:80",
        ] {
            assert!(Port::parse(bad).is_err(), "{bad:?} accepted");
        }
    }

    #[test]
    fn ports_in_toml() {
        #[derive(Deserialize)]
        struct File {
            ports: Vec<Port>,
        }
        let f: File = toml::from_str("ports = [3080, \"8080:3000\"]").unwrap();
        assert_eq!(
            f.ports,
            [
                Port {
                    host: 3080,
                    container: 3080
                },
                Port {
                    host: 8080,
                    container: 3000
                }
            ]
        );
        assert!(toml::from_str::<File>("ports = [70000]").is_err());
        assert!(toml::from_str::<File>("ports = [true]").is_err());
    }

    #[test]
    fn duplicate_host_ports() {
        let mut ports = Vec::new();
        add(&mut ports, Port::parse("3080").unwrap()).unwrap();
        add(&mut ports, Port::parse("3080").unwrap()).unwrap();
        assert_eq!(ports.len(), 1);
        let err = add(&mut ports, Port::parse("3080:4000").unwrap()).unwrap_err();
        assert!(err.to_string().contains("forwarded twice"), "{err}");
        add(&mut ports, Port::parse("8080:3080").unwrap()).unwrap();
        assert_eq!(ports.len(), 2);
    }

    /// Picks a free host port; another process could take it in between,
    /// which is as good as tests on a shared machine get.
    fn free_port() -> u16 {
        TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    #[test]
    fn forwards_through_the_socket() {
        let port = free_port();
        let forwarder = Forwarder::start(&[Port {
            host: port,
            container: 3080,
        }])
        .unwrap();
        assert!(!forwarder.wait_ready(Duration::from_millis(10)));

        // Stands in for the relay.
        let relay = UnixListener::bind(forwarder.dir().join(socket_name(3080))).unwrap();
        assert!(forwarder.wait_ready(Duration::from_secs(1)));
        thread::spawn(move || {
            let (mut conn, _) = relay.accept().unwrap();
            let mut buf = [0u8; 4];
            conn.read_exact(&mut buf).unwrap();
            assert_eq!(&buf, b"ping");
            conn.write_all(b"pong").unwrap();
        });

        let mut client = TcpStream::connect(("127.0.0.1", port)).unwrap();
        client.write_all(b"ping").unwrap();
        let mut reply = Vec::new();
        client.read_to_end(&mut reply).unwrap();
        assert_eq!(reply, b"pong");

        assert!(
            forwarder.describe().contains(&format!(
                "forwarding http://127.0.0.1:{port} to session port 3080"
            )),
            "{}",
            forwarder.describe()
        );
        assert!(
            forwarder.summary().contains(&format!(
                "127.0.0.1:{port} -> session port 3080: 1 connection\n"
            )),
            "{}",
            forwarder.summary()
        );

        // The port closes with the forwarder.
        drop(forwarder);
        let deadline = Instant::now() + Duration::from_secs(5);
        while TcpStream::connect(("127.0.0.1", port)).is_ok() {
            assert!(Instant::now() < deadline, "port {port} still open");
            thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn taken_port_fails_up_front() {
        let taken = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = taken.local_addr().unwrap().port();
        let err = Forwarder::start(&[Port {
            host: port,
            container: 3080,
        }])
        .err()
        .unwrap();
        let msg = err.to_string();
        assert!(msg.contains("it is in use"), "{msg}");
        assert!(msg.contains("--port HOST:3080"), "{msg}");
    }
}
