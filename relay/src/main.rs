//! podbay-relay: the only way in or out of a session's network namespace.
//!
//! It runs in a container, and the session shares that container's network
//! namespace. It has two jobs, each optional:
//!
//! - **Egress** (sessions with a network allowlist): the relay has no
//!   network of its own, so both it and the session see nothing but
//!   loopback. The relay listens there, and forwards every connection, byte
//!   for byte, to a Unix socket that podbay serves on the host. podbay
//!   enforces the allowlist; the relay makes no decisions and never touches
//!   a real network.
//! - **Forwarded ports**: the relay listens on a Unix socket per port, in a
//!   directory podbay shares with it, and connects every connection podbay
//!   makes there to that port on loopback, where a server in the session
//!   listens. The port is fixed when the relay starts; nothing that arrives
//!   on the socket can choose another destination.
//!
//! It is built into a minimal image from this one file with plain `rustc`,
//! so it uses the standard library only.
//!
//! Usage: podbay-relay [--egress <listen address> <socket path>]
//!                     [--forward <socket path> <port>]...

use std::env;
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, Ipv6Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::os::unix::net::{UnixListener, UnixStream};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

/// What the relay sends podbay once it is listening.
pub const READY: &[u8] = b"podbay-relay ready\n";

/// Connections beyond this, across all listeners, are closed straight away.
const MAX_CONNECTIONS: usize = 256;

const USAGE: &str = "usage: podbay-relay [--egress <listen address> <socket path>] \
                     [--forward <socket path> <port>]...";

#[derive(Debug, PartialEq, Eq)]
struct Egress {
    listen: String,
    socket: String,
}

#[derive(Debug, PartialEq, Eq)]
struct Forward {
    socket: String,
    port: u16,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Config {
    egress: Option<Egress>,
    forwards: Vec<Forward>,
}

fn parse(args: &[String]) -> Result<Config, String> {
    let mut config = Config::default();
    let mut args = args.iter();
    while let Some(flag) = args.next() {
        let mut value = || {
            args.next()
                .cloned()
                .ok_or_else(|| format!("{flag} needs two values"))
        };
        match flag.as_str() {
            "--egress" if config.egress.is_none() => {
                let (listen, socket) = (value()?, value()?);
                config.egress = Some(Egress { listen, socket });
            }
            "--forward" => {
                let (socket, port) = (value()?, value()?);
                let port = port
                    .parse::<u16>()
                    .ok()
                    .filter(|p| *p != 0)
                    .ok_or_else(|| format!("invalid port {port:?}"))?;
                config.forwards.push(Forward { socket, port });
            }
            _ => return Err(format!("unexpected argument {flag:?}")),
        }
    }
    if config.egress.is_none() && config.forwards.is_empty() {
        return Err("nothing to do".into());
    }
    Ok(config)
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let config = match parse(&args) {
        Ok(config) => config,
        Err(err) => {
            eprintln!("podbay-relay: {err}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    match serve(config) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("podbay-relay: {err}");
            ExitCode::FAILURE
        }
    }
}

fn serve(config: Config) -> io::Result<()> {
    let active = Arc::new(AtomicUsize::new(0));
    let mut threads = Vec::new();

    // Forward sockets first: podbay waits for the egress ready message, and
    // by then every socket it might connect to exists.
    for forward in config.forwards {
        let listener = UnixListener::bind(&forward.socket)?;
        let active = Arc::clone(&active);
        threads.push(thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(conn) = conn else { continue };
                let port = forward.port;
                spawn_limited(&active, move || {
                    if let Some(upstream) = connect_loopback(port) {
                        let _ = pipe(conn, upstream);
                    }
                });
            }
        }));
    }

    if let Some(egress) = config.egress {
        let listener = TcpListener::bind(&egress.listen)?;
        UnixStream::connect(&egress.socket)?.write_all(READY)?;
        let active = Arc::clone(&active);
        threads.push(thread::spawn(move || {
            for client in listener.incoming() {
                let Ok(client) = client else { continue };
                let socket = egress.socket.clone();
                spawn_limited(&active, move || {
                    if let Ok(upstream) = UnixStream::connect(&socket) {
                        let _ = pipe(client, upstream);
                    }
                });
            }
        }));
    }

    for t in threads {
        let _ = t.join();
    }
    Ok(())
}

/// Runs `work` on its own thread, unless too many already are. Out of
/// threads (the container's process limit) drops the connection rather than
/// the relay.
fn spawn_limited(active: &Arc<AtomicUsize>, work: impl FnOnce() + Send + 'static) {
    if active.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
        active.fetch_sub(1, Ordering::SeqCst);
        return;
    }
    let done = Arc::clone(active);
    let spawned = thread::Builder::new().spawn(move || {
        work();
        done.fetch_sub(1, Ordering::SeqCst);
    });
    if spawned.is_err() {
        active.fetch_sub(1, Ordering::SeqCst);
    }
}

/// The session's server on `port`, on IPv4 loopback or else IPv6.
fn connect_loopback(port: u16) -> Option<TcpStream> {
    [
        SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
        SocketAddr::from((Ipv6Addr::LOCALHOST, port)),
    ]
    .iter()
    .find_map(|addr| TcpStream::connect(addr).ok())
}

/// A stream that can be split for copying both ways.
trait Duplex: Sized + Send + 'static
where
    for<'a> &'a Self: Read + Write,
{
    fn try_clone(&self) -> io::Result<Self>;
    fn shutdown(&self, how: Shutdown) -> io::Result<()>;
}

impl Duplex for TcpStream {
    fn try_clone(&self) -> io::Result<Self> {
        TcpStream::try_clone(self)
    }
    fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        TcpStream::shutdown(self, how)
    }
}

impl Duplex for UnixStream {
    fn try_clone(&self) -> io::Result<Self> {
        UnixStream::try_clone(self)
    }
    fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        UnixStream::shutdown(self, how)
    }
}

/// Copies both ways until both sides are done, passing on half-closes.
fn pipe<A: Duplex, B: Duplex>(a: A, b: B) -> io::Result<()>
where
    for<'x> &'x A: Read + Write,
    for<'x> &'x B: Read + Write,
{
    let (a_in, b_out) = (a.try_clone()?, b.try_clone()?);
    let up = thread::Builder::new().spawn(move || {
        let _ = io::copy(&mut &a_in, &mut &b_out);
        let _ = b_out.shutdown(Shutdown::Write);
    })?;
    let _ = io::copy(&mut &b, &mut &a);
    let _ = a.shutdown(Shutdown::Write);
    let _ = up.join();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn arguments() {
        assert_eq!(
            parse(&args(&[
                "--egress",
                "127.0.0.1:3128",
                "/run/podbay/egress.sock",
                "--forward",
                "/run/podbay-forward/3080.sock",
                "3080",
            ])),
            Ok(Config {
                egress: Some(Egress {
                    listen: "127.0.0.1:3128".into(),
                    socket: "/run/podbay/egress.sock".into(),
                }),
                forwards: vec![Forward {
                    socket: "/run/podbay-forward/3080.sock".into(),
                    port: 3080,
                }],
            })
        );
        for bad in [
            &[][..],
            &["--egress", "127.0.0.1:3128"],
            &["--forward", "/s", "0"],
            &["--forward", "/s", "http"],
            &["--egress", "a", "b", "--egress", "c", "d"],
            &["127.0.0.1:3128", "/run/podbay/egress.sock"],
        ] {
            assert!(parse(&args(bad)).is_err(), "{bad:?} accepted");
        }
    }

    #[test]
    fn forwards_to_loopback() {
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = server.local_addr().unwrap().port();
        thread::spawn(move || {
            let (mut conn, _) = server.accept().unwrap();
            let mut buf = [0u8; 4];
            conn.read_exact(&mut buf).unwrap();
            conn.write_all(b"pong").unwrap();
        });
        let dir = env::temp_dir().join(format!("podbay-relay-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("fwd.sock");
        let _ = std::fs::remove_file(&socket);
        let config = Config {
            egress: None,
            forwards: vec![Forward {
                socket: socket.to_string_lossy().into_owned(),
                port,
            }],
        };
        thread::spawn(move || serve(config));
        let mut conn = loop {
            match UnixStream::connect(&socket) {
                Ok(conn) => break conn,
                Err(_) => thread::sleep(std::time::Duration::from_millis(10)),
            }
        };
        conn.write_all(b"ping").unwrap();
        let mut reply = Vec::new();
        conn.read_to_end(&mut reply).unwrap();
        assert_eq!(reply, b"pong");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
