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
    /// Launch this profile (from a `starlight://profile/{id}` deep link).
    OpenProfile(String),
    /// Show this profile's page (from a `starlight://profile/{id}/edit` link).
    EditProfile(String),
    /// No payload — just bring the window to the front.
    Activate,
}

/// Decide whether this process is the primary instance. If a primary is
/// already running, forward `request` (or an activate request) to it and
/// return [`Instance::Forwarded`].
pub fn acquire(request: Option<Message>) -> Instance {
    if let Some(mut stream) = connect_to_primary() {
        let message = match &request {
            Some(Message::OpenProfile(id)) => format!("open {id}\n"),
            Some(Message::EditProfile(id)) => format!("edit {id}\n"),
            Some(Message::Activate) | None => "activate\n".to_string(),
        };
        if stream.write_all(message.as_bytes()).is_ok() && stream.flush().is_ok() {
            return Instance::Forwarded;
        }
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
                if let Some(id) = line.strip_prefix("open ") {
                    on_message(Message::OpenProfile(id.to_string()));
                } else if let Some(id) = line.strip_prefix("edit ") {
                    on_message(Message::EditProfile(id.to_string()));
                } else if line == "activate" {
                    on_message(Message::Activate);
                }
            }
        })
        .expect("spawn single-instance thread");
}
