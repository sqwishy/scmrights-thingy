use std::io;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use mio::unix::UnixReady;
use mio::{Event, Poll, PollOpt, Ready};

use crate::CHAT_TOKEN;

#[derive(Debug)]
pub struct Client {
    log: slog::Logger,
    server_addr: SocketAddr,
    state: State,
}

#[derive(Debug)]
enum State {
    Waiting {
        failures: usize,
        wake_at: Instant,
    },
    /// I had to combine states Connecting { failures, stream } and Connected { stream } because I
    /// couldn't "move" the stream from one to the next from a mutable borrow to self. ¯\_(ツ)_/¯
    Connecteding {
        failures: usize,
        stream: mio::net::TcpStream,
    },
}

impl Client {
    pub fn connecting(
        server_addr: &SocketAddr,
        log: &slog::Logger,
        poll: &Poll,
    ) -> io::Result<Self> {
        let stream = mio::net::TcpStream::connect(server_addr)?;
        Self::from_stream(stream, server_addr, log, poll)
    }

    pub fn from_stream(
        stream: mio::net::TcpStream,
        server_addr: &SocketAddr,
        log: &slog::Logger,
        poll: &Poll,
    ) -> io::Result<Self> {
        poll.register(&stream, CHAT_TOKEN, Ready::writable(), PollOpt::oneshot())?;
        Ok(Client {
            log: log.clone(),
            server_addr: server_addr.clone(),
            state: State::Connecteding {
                stream: stream.into(),
                failures: 0,
            },
        })
    }

    pub fn stream(&self) -> Option<&mio::net::TcpStream> {
        match &self.state {
            State::Waiting { .. } => None,
            State::Connecteding { stream, .. } => Some(stream),
        }
    }

    pub fn wake_at(&self) -> Option<Instant> {
        match self.state {
            State::Waiting { wake_at, .. } => Some(wake_at),
            _ => None,
        }
    }

    pub fn wake_up(&mut self, poll: &Poll) -> io::Result<()> {
        match self.state {
            State::Waiting { failures, .. } => {
                let stream = mio::net::TcpStream::connect(&self.server_addr)?;
                poll.register(&stream, CHAT_TOKEN, Ready::writable(), PollOpt::oneshot())?;
                self.state = State::Connecteding {
                    stream: stream.into(),
                    failures,
                }
            }
            _ => (),
        };
        Ok(())
    }

    pub fn event(&mut self, poll: &Poll, event: Event) -> io::Result<()> {
        let ready = event.readiness();
        let unix_ready = UnixReady::from(ready);
        if unix_ready.is_hup() || unix_ready.is_error() {
            self.reconnect(&poll)?;
        } else if ready.is_readable() {
            match &mut self.state {
                State::Connecteding { stream, .. } => loop {
                    match io::copy(stream, &mut io::stdout()) {
                        Ok(0) => return self.reconnect(poll),
                        Ok(_) => break,
                        Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => break,
                        Err(e) => return Err(e),
                    }
                },
                _ => unreachable!(),
            }
        } else if ready.is_writable() {
            match &mut self.state {
                State::Connecteding {
                    stream,
                    ref mut failures,
                } => {
                    *failures = 0;
                    info!(self.log, "connected as {}", stream.local_addr()?);
                    poll.reregister(stream, CHAT_TOKEN, Ready::readable(), PollOpt::level())?;
                }
                _ => unreachable!(),
            }
        }
        Ok(())
    }

    fn reconnect(&mut self, poll: &Poll) -> io::Result<()> {
        let failures = match &self.state {
            State::Waiting { .. } => return Ok(()),
            State::Connecteding { failures, stream } => {
                if let Some(err) = stream.take_error()? {
                    error!(self.log, "disconnected with error: {}", err);
                }
                poll.deregister(stream)?;
                failures + 1
            }
        };

        self.state = if failures > 0 {
            use std::cmp;
            let delay = Duration::from_millis(cmp::min(failures.pow(3), 500) as u64).into();
            info!(
                self.log,
                "reconnecting to {} in {:?} ...", self.server_addr, delay,
            );
            let wake_at = Instant::now() + delay;
            State::Waiting { failures, wake_at }
        } else {
            let stream = mio::net::TcpStream::connect(&self.server_addr)?;
            poll.register(&stream, CHAT_TOKEN, Ready::writable(), PollOpt::oneshot())?;
            State::Connecteding {
                stream: stream.into(),
                failures,
            }
        };
        Ok(())
    }
}
