//! Single-instance guard. The first ("primary") instance binds a loopback
//! TCP listener and records its port under the app data dir; later instances
//! connect, forward their message — a deep-link profile to launch, or just
//! "raise your window" — and exit immediately. This is what makes desktop
//! shortcuts work while the app is already open, instead of spawning a
//! second window.
//!
//! The port file can go stale (crash, or another process reusing the port),
//! so the primary greets with a magic line first; a connector that doesn't
//! see the greeting ignores the socket and takes over as primary.

use std::io::{BufRead, BufReader, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::time::Duration;

use log::warn;

use crate::backend::directories;

const PORT_FILE: &str = "instance.port";
const GREETING: &str = "STARLIGHT v1";
/// Written back once the primary has accepted a message, so the secondary can
/// tell delivery from a message that was dropped on the floor.
const ACK: &str = "OK";
const IO_TIMEOUT: Duration = Duration::from_millis(500);

pub enum Instance {
    /// This process is the primary. `Some` carries the listener to serve
    /// (`None` if binding failed — run without single-instance support).
    Primary(Option<TcpListener>),
    /// Another instance is running; the message was forwarded to it and this
    /// process should exit.
    Forwarded,
}

/// What a secondary instance asks the primary to do.
#[derive(Debug, Clone)]
pub enum Message {
    /// A `starlight://` URL to handle, forwarded verbatim — the primary parses
    /// it with [`crate::backend::deeplink`], so new link kinds need no changes
    /// to this protocol.
    DeepLink(String),
    /// No payload — just bring the window to the front.
    Activate,
}

/// Decide whether this process is the primary instance. If a primary is
/// already running, forward `request` (or an activate request) to it and
/// return [`Instance::Forwarded`].
pub fn acquire(request: Option<Message>) -> Instance {
    if let Some(mut stream) = connect_to_primary() {
        let line = match &request {
            Some(Message::DeepLink(url)) => format!("link {url}"),
            Some(Message::Activate) | None => "activate".to_string(),
        };
        // Only a confirmed message counts as forwarded. Without the ack the
        // primary may have dropped it (or died mid-handshake), and exiting
        // then would lose the deep link entirely — so fall through and take
        // over as primary instead.
        if send(&mut stream, &line) {
            return Instance::Forwarded;
        }
        warn!("running instance did not acknowledge '{line}'; starting a new one");
    }

    match TcpListener::bind((Ipv4Addr::LOCALHOST, 0)) {
        Ok(listener) => {
            match (directories::app_data_dir(), listener.local_addr()) {
                (Ok(dir), Ok(addr)) => {
                    let _ = std::fs::create_dir_all(&dir);
                    if let Err(e) = std::fs::write(dir.join(PORT_FILE), addr.port().to_string()) {
                        warn!("failed to write instance port file: {e}");
                    }
                }
                _ => warn!("single-instance port not recorded; duplicate instances possible"),
            }
            Instance::Primary(Some(listener))
        }
        Err(e) => {
            warn!("single-instance listener bind failed: {e}");
            Instance::Primary(None)
        }
    }
}

/// Send one message and wait for the primary's [`ACK`]. `false` means the
/// message wasn't delivered: the write failed, the primary didn't recognize
/// the line, or it went away before answering.
fn send(stream: &mut TcpStream, line: &str) -> bool {
    if stream
        .write_all(format!("{line}\n").as_bytes())
        .and_then(|()| stream.flush())
        .is_err()
    {
        return false;
    }
    let mut ack = String::new();
    BufReader::new(stream).read_line(&mut ack).is_ok() && ack.trim() == ACK
}

/// Connect to the recorded primary and verify it greets like Starlight.
fn connect_to_primary() -> Option<TcpStream> {
    let port_path = directories::app_data_dir().ok()?.join(PORT_FILE);
    let port: u16 = std::fs::read_to_string(port_path)
        .ok()?
        .trim()
        .parse()
        .ok()?;
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let stream = TcpStream::connect_timeout(&addr, IO_TIMEOUT).ok()?;
    stream.set_read_timeout(Some(IO_TIMEOUT)).ok()?;
    stream.set_write_timeout(Some(IO_TIMEOUT)).ok()?;

    let mut greeting = String::new();
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    reader.read_line(&mut greeting).ok()?;
    (greeting.trim() == GREETING).then_some(stream)
}

/// Serve messages from later instances on a dedicated thread for the life of
/// the process. `on_message` is invoked on that thread — it must hand work
/// off to thread-safe backend calls or the event bus, not touch UI directly.
pub fn serve(listener: TcpListener, on_message: impl Fn(Message) + Send + 'static) {
    std::thread::Builder::new()
        .name("single-instance".into())
        .spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
                let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
                if stream
                    .write_all(format!("{GREETING}\n").as_bytes())
                    .is_err()
                {
                    continue;
                }
                let mut line = String::new();
                if BufReader::new(&mut stream).read_line(&mut line).is_err() {
                    continue;
                }
                let line = line.trim();
                let understood = if let Some(url) = line.strip_prefix("link ") {
                    on_message(Message::DeepLink(url.to_string()));
                    true
                } else if line == "activate" {
                    on_message(Message::Activate);
                    true
                } else {
                    warn!("ignoring unknown message from another instance: {line}");
                    false
                };
                // The secondary waits for this before it exits; leaving it
                // unsent is what tells it the message didn't land.
                if understood {
                    let _ = stream.write_all(format!("{ACK}\n").as_bytes());
                }
            }
        })
        .expect("spawn single-instance thread");
}
