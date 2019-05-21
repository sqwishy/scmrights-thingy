#[macro_use]
extern crate clap;

extern crate libc;

extern crate memchr;

extern crate mio;

extern crate serde;

#[macro_use]
extern crate slog;
extern crate slog_journald;
extern crate slog_term;

use std::io;
use std::net::SocketAddr;
use std::os::unix::io::RawFd;
use std::time::Instant;

use mio::unix::UnixReady;
use mio::{Events, Poll, Token};

mod chat;
mod ipc;
use ipc::Ipc;

mod lineio;

const IPC_SERVER_TOKEN: Token = Token(0);
const IPC_PEER_TOKEN: Token = Token(1);
const CHAT_TOKEN: Token = Token(2);

#[derive(Debug, PartialEq)]
pub enum Control {
    Continue,
    Quit(String),
}

impl Default for Control {
    fn default() -> Self {
        Control::Continue
    }
}

impl From<()> for Control {
    fn from(_: ()) -> Control {
        Control::default()
    }
}

trait MapDefault<T, E> {
    fn map_default(self) -> Result<T, E>;
}

impl<D, E> MapDefault<D, E> for Result<(), E>
where
    D: Default,
{
    fn map_default(self) -> Result<D, E> {
        self.map(|()| D::default())
    }
}

fn main() -> Result<(), io::Error> {
    let matches = clap::App::new(env!("CARGO_PKG_NAME"))
        .version(env!("CARGO_PKG_VERSION"))
        .arg(clap::Arg::from_usage("--journal"))
        .arg(
            clap::Arg::from_usage("--replace")
                .possible_values(&["nope", "try", "must"])
                .default_value("try"),
        )
        .arg(clap::Arg::from_usage("--ipc").default_value("/tmp/potato"))
        .arg(clap::Arg::with_name("chat").default_value("127.0.0.1:31337"))
        .get_matches();

    let log = new_slog(&matches);

    let do_replace = matches.value_of("replace").unwrap();
    let ipc_path = value_t_or_exit!(matches, "ipc", String);
    let chat_addr = value_t_or_exit!(matches, "chat", SocketAddr);

    trace!(log, "{}", env!("CARGO_PKG_NAME");
           "do_replace" => do_replace,
           "ipc_path" => ipc_path.clone(),
           "chat_addr" => chat_addr.clone());

    let maybe_chat_fd = match do_replace {
        "nope" => None,
        "try" => request_fds(&ipc_path, &log).unwrap_or_else(|err| {
            warn!(log, "failed to replace existing process: {}", err);
            None
        }),
        "must" => request_fds(&ipc_path, &log)?,
        _ => unreachable!(),
    };

    let poll = Poll::new()?;
    let mut events = Events::with_capacity(1024);
    let mut ipc = Ipc::listening(&ipc_path, &log, &poll)?;
    let mut chat = if let Some(chat_fd) = maybe_chat_fd {
        use std::os::unix::io::FromRawFd;
        let stream = unsafe { mio::net::TcpStream::from_raw_fd(chat_fd) };
        chat::Client::from_stream(stream, &chat_addr, &log, &poll)?
    } else {
        chat::Client::connecting(&chat_addr, &log, &poll)?
    };

    // We track error counts in buckets and if we see more than like six or something across a
    // dozen loops then we halt.
    // I don't really like how this is written but I'm not sure what I'm doing.
    use std::cell::RefCell;
    let mut buckets = [0usize; 12];
    let badness: RefCell<_> = BucketCounts::with_buckets(&mut buckets).into();
    let control_for_err = |_| {
        let mut badness = badness.borrow_mut();
        badness.add_one();
        let errcount = badness.sum();
        if errcount < badness.buckets.len() / 2 {
            Control::Continue
        } else {
            Control::Quit(format!("too many ({}) errors too quickly", errcount,))
        }
    };

    info!(log, "running ...");

    loop {
        badness.borrow_mut().next_bucket();

        // check our "timer"
        let timeout = if let Some(when) = chat.wake_at() {
            let now = Instant::now();
            if when < now {
                if let Err(err) = chat.wake_up(&poll) {
                    error!(log, "failed to wake up chat: {}", err);
                }
                None
            } else {
                Some(when - now)
            }
        } else {
            None
        };

        poll.poll(&mut events, timeout)?;

        for event in &events {
            // trace!(log, "event: {:?}", event);
            let unix_ready = UnixReady::from(event.readiness());
            let res = match event.token() {
                IPC_SERVER_TOKEN => ipc.connect(&poll).map_default(),
                IPC_PEER_TOKEN if unix_ready.is_hup() => ipc.lost_peer(&poll).map_default(),
                IPC_PEER_TOKEN if unix_ready.is_error() => ipc.lost_peer(&poll).map_default(),
                IPC_PEER_TOKEN => ipc.peer_event(&poll, event, &chat),
                CHAT_TOKEN => chat.event(&poll, event).map_default(),
                _ => unreachable!(),
            }
            .unwrap_or_else(|err| {
                error!(log, "failed to handle event {:?}: {}", event, err);
                control_for_err(err)
            });
            match res {
                Control::Continue => (),
                Control::Quit(why) => {
                    info!(log, "quitting: {}", why);
                    return Ok(());
                }
            }
        }
    }
}

/// with blocking calls, connect to the running service and ask for its file descriptors
fn request_fds(ipc_path: &str, log: &slog::Logger) -> Result<Option<RawFd>, io::Error> {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;

    info!(log, "connecting to peer to replace ...");
    let mut stream = UnixStream::connect(&ipc_path)?;

    serde_json::to_writer(&stream, &ipc::Message::RequestFds)?;
    stream.write(b"\n")?;

    info!(log, "waiting for fds ...");
    let res = match ipc::recv_fds(&stream)?.as_slice() {
        &[fd] => Some(fd.into()),
        &[] => None,
        fds @ _ => {
            warn!(log, "got more fds than we bargained for: {:?}", fds);
            Some(fds[0])
        }
    };

    serde_json::to_writer(&stream, &ipc::Message::Quit)?;
    stream.write(b"\n")?;

    let mut wat = [0u8; 128];
    match stream.read(&mut wat[..])? {
        0 => (),
        n @ _ => error!(
            log,
            "unexpected data ({}) after sending quit to previous process?\n{:?}",
            n,
            &wat[..n]
        ),
    };

    Ok(res)
}

pub fn new_slog(matches: &clap::ArgMatches) -> slog::Logger {
    use slog::Drain;
    if matches.is_present("journal") {
        let drain = slog_journald::JournaldDrain.ignore_res();
        slog::Logger::root(drain, o!())
    } else {
        let decorator = slog_term::TermDecorator::new().build();
        let drain = slog_term::CompactFormat::new(decorator).build().fuse();
        let drain = std::sync::Mutex::new(drain).fuse();
        slog::Logger::root(drain, o!())
    }
}

struct BucketCounts<'a> {
    pub current: usize,
    pub buckets: &'a mut [usize],
}

impl<'a> BucketCounts<'a> {
    pub fn with_buckets(buckets: &'a mut [usize]) -> Self {
        assert_ne!(0, buckets.len());
        BucketCounts {
            buckets,
            current: 0,
        }
    }

    pub fn next_bucket(&mut self) {
        self.current = (self.current + 1) % self.buckets.len();
        self.buckets[self.current] = 0;
    }

    pub fn add_one(&mut self) -> usize {
        self.buckets[self.current] += 1;
        self.buckets[self.current]
    }

    pub fn sum(&self) -> usize {
        self.buckets.iter().sum()
    }
}
