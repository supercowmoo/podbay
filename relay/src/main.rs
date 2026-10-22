//! podbay-relay: the only way out of a session with a network allowlist.
//!
//! It runs in a container with no network of its own, and the session
//! shares that container's network namespace, so both see nothing but
//! loopback. The relay listens there, and forwards every connection, byte
//! for byte, to a Unix socket that podbay serves on the host. podbay
//! enforces the allowlist; the relay makes no decisions and never touches a
//! real network.
//!
//! It is built into a minimal image from this one file with plain `rustc`,
//! so it uses the standard library only.
//!
//! Usage: podbay-relay <listen address> <socket path>

use std::env;
use std::io::{self, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

/// What the relay sends podbay once it is listening.
pub const READY: &[u8] = b"podbay-relay ready\n";

/// Connections beyond this are closed straight away.
const MAX_CONNECTIONS: usize = 256;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    let [_, listen, socket] = args.as_slice() else {
        eprintln!("usage: podbay-relay <listen address> <socket path>");
        return ExitCode::from(2);
    };
    match serve(listen, socket) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("podbay-relay: {err}");
            ExitCode::FAILURE
        }
    }
}

fn serve(listen: &str, socket: &str) -> io::Result<()> {
    let listener = TcpListener::bind(listen)?;
    UnixStream::connect(socket)?.write_all(READY)?;

    let active = Arc::new(AtomicUsize::new(0));
    for client in listener.incoming() {
        let Ok(client) = client else { continue };
        if active.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
            active.fetch_sub(1, Ordering::SeqCst);
            continue;
        }
        let socket = socket.to_owned();
        let done = Arc::clone(&active);
        // Out of threads (the container's process limit) drops the
        // connection rather than the relay.
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
    Ok(())
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
