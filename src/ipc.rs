use std::boxed::Box;
use std::io;
use std::os::unix::io::AsRawFd;

use mio::{Poll, PollOpt, Ready};

use crate as main;
use crate::chat;
use crate::lineio::LineIo;
use crate::MapDefault;
use crate::IPC_PEER_TOKEN;
use crate::IPC_SERVER_TOKEN;

#[derive(Debug)]
pub struct Ipc {
    log: slog::Logger,
    listener: mio_uds::UnixListener,
    peer: Option<IpcConnection>,
}

impl Ipc {
    pub fn listening(path: &str, log: &slog::Logger, poll: &Poll) -> io::Result<Self> {
        if let Err(e) = std::fs::remove_file(path) {
            if e.kind() != io::ErrorKind::NotFound {
                error!(log, "couldn't remove old ipc socket?: {}", e)
            }
        }
        let listener = mio_uds::UnixListener::bind(path)?;
        poll.register(
            &listener,
            IPC_SERVER_TOKEN,
            Ready::readable(),
            PollOpt::level(),
        )?;
        Ok(Ipc {
            listener,
            log: log.clone(),
            peer: None,
        })
    }

    pub fn connect(&mut self, poll: &mio::Poll) -> io::Result<()> {
        match self.listener.accept()? {
            Some((stream, _peer_addr)) => {
                if self.peer.is_some() {
                    error!(self.log, "we already have a connection");
                    stream.shutdown(std::net::Shutdown::Both)?;
                } else {
                    info!(self.log, "wow, connected!");
                    let peer = IpcConnection::from_stream(stream, &self.log);
                    poll.register(
                        &peer.stream,
                        IPC_PEER_TOKEN,
                        Ready::readable(),
                        PollOpt::level(),
                    )?;
                    self.peer = Some(peer);
                }
            }
            None => warn!(self.log, "no ipc peer to accept?"),
        };
        Ok(())
    }

    pub fn peer_event(
        &mut self,
        poll: &mio::Poll,
        event: mio::Event,
        chat: &chat::Client,
    ) -> io::Result<main::Control> {
        if let Some(peer) = self.peer.as_mut() {
            if event.readiness().is_writable() {
                peer.write(poll, chat).map_default()
            } else {
                peer.read(poll)
            }
        } else {
            error!(self.log, "no ipc peer to read from?");
            Ok(main::Control::default())
        }
    }

    pub fn lost_peer(&mut self, poll: &mio::Poll) -> Result<(), io::Error> {
        if let Some(peer) = self.peer.take() {
            poll.deregister(&peer.stream)?;
            if let Some(err) = peer.stream.take_error()? {
                error!(self.log, "ipc stream error: {}", err);
            } else {
                info!(self.log, "lost ipc peer");
            };
        } else {
            unreachable!("didn't have an ipc peer to lose?");
        };
        Ok(())
    }
}

use std::collections::VecDeque;

#[derive(Debug)]
pub struct IpcConnection {
    log: slog::Logger,
    naughtyness: u8,
    writebuf: VecDeque<Sendable>,
    // Alternatively, I think if we just used datagrams instead of a stream, the message boundaries
    // are maintained and we don't have to split them up again like this...
    readbuf: Box<LineIo>,
    // proto: IpcProtocol, // for decoupling from the socket & the event loop ...
    stream: mio_uds::UnixStream,
}

impl IpcConnection {
    pub fn from_stream(stream: mio_uds::UnixStream, log: &slog::Logger) -> Self {
        IpcConnection {
            log: log.clone(),
            // proto: IpcProtocol,
            naughtyness: 0,
            writebuf: VecDeque::with_capacity(8),
            readbuf: Box::new(LineIo::new()),
            stream,
        }
    }

    pub fn read(&mut self, poll: &mio::Poll) -> Result<main::Control, io::Error> {
        match self.readbuf.read_from(&mut self.stream) {
            // Ok(0) => unimplemented!("EOF?"),
            Ok(_) => (),
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => (),
            Err(e) => return Err(e),
        };

        loop {
            let line = match self.readbuf.find_line_utf8() {
                Ok(Some(line)) if line.is_empty() => break,
                Ok(Some(line)) => line,
                Ok(None) => break,
                Err(e) => {
                    warn!(self.log, "error reading lines: {}", e);
                    self.naughtyness += 1; // TODO reset this?
                    if self.naughtyness >= 8 {
                        error!(
                            self.log,
                            "peer is naughty ({}); shutting down", self.naughtyness
                        );
                        self.stream.shutdown(std::net::Shutdown::Both)?;
                        break;
                    } else {
                        continue;
                    }
                }
            };

            match serde_json::from_str::<Message>(&line) {
                Ok(msg) => {
                    debug!(self.log, "{:?}", msg);
                    match msg {
                        Message::Ping => self.send_soon(Message::Pong),
                        Message::Pong => Ok(()),
                        Message::RequestFds => self.send_soon(Sendable::Fds),
                        Message::Quit => {
                            let peer = self
                                .stream
                                .peer_addr()
                                .map(|addr| format!("{:?}", addr))
                                .unwrap_or_else(|e| format!("(peer unknown; {})", e));
                            return Ok(main::Control::Quit(format!("stop requested by: {}", peer)));
                        }
                    }?;
                }
                Err(e) => error!(self.log, "invalid message: {:?}... line was\n{}", e, line),
            };
        }

        let mut readiness = Ready::readable();
        if !self.writebuf.is_empty() {
            readiness |= Ready::writable();
        }
        poll.reregister(&self.stream, IPC_PEER_TOKEN, readiness, PollOpt::level())?;

        Ok(Default::default())
    }

    pub fn write(&mut self, poll: &mio::Poll, chat: &chat::Client) -> Result<(), io::Error> {
        use std::io::Write;
        while let Some(sendable) = self.writebuf.get(0) {
            let result = match sendable {
                Sendable::Message(msg) => {
                    let mut v = serde_json::to_vec(msg)?;
                    v.push(b'\n');
                    self.stream.write(&v[..])
                }
                Sendable::Fds => {
                    if let Some(peer) = chat.stream() {
                        send_fds(&self.stream, &[peer.as_raw_fd()])
                    } else {
                        send_fds(&self.stream, &[])
                    }
                }
            };
            match result {
                Ok(_) => {
                    self.writebuf.pop_front();
                }
                Err(ref e) if e.kind() == io::ErrorKind::Interrupted => (),
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(e) => return Err(e),
            };
        }

        let mut readiness = Ready::readable();
        if !self.writebuf.is_empty() {
            readiness |= Ready::writable();
        }
        poll.reregister(&self.stream, IPC_PEER_TOKEN, readiness, PollOpt::level())?;

        Ok(())
    }

    fn send_soon<S>(&mut self, s: S) -> io::Result<()>
    where
        S: Into<Sendable>,
    {
        self.writebuf.push_back(s.into());
        Ok(())
    }
}

use serde::{Deserialize, Serialize};

#[derive(Debug)]
pub enum Sendable {
    Fds,
    Message(Message),
}

#[derive(Serialize, Deserialize, Debug)]
pub enum Message {
    Ping,
    Pong,
    RequestFds,
    Quit,
}

impl From<Message> for Sendable {
    fn from(m: Message) -> Self {
        Sendable::Message(m)
    }
}

// // todo move to cmsg.rs or something?
// use std::ffi;
// use std::mem;
//
// // /usr/inclue/bits/socket.h ???
// const fn cmsg_align(len: usize) -> usize {
//     (len + mem::size_of::<libc::size_t>() - 1) & !(mem::size_of::<libc::size_t>() - 1)
// }
//
// const fn cmsg_space(len: usize) -> usize {
//     cmsg_align(mem::size_of::<libc::cmsghdr>()) + cmsg_align(len)
// }
//
// const fn cmsg_len(len: usize) -> usize {
//     cmsg_align(mem::size_of::<libc::cmsghdr>()) + len
// }
//
// fn send_fds<S>(stream: &S, fds: &[RawFd]) -> Result<(), io::Error>
// where
//     S: AsRawFd,
// {
//     let mut iov = libc::iovec {
//         iov_base: ffi::CString::new("hi").unwrap().into_bytes().as_mut_ptr() as *mut libc::c_void,
//         iov_len: 2,
//     };
//
//     let mut cmsgbuf = Vec::<u8>::with_capacity(cmsg_space(mem::size_of_val(fds)));
//     unsafe { cmsgbuf.set_len(cmsgbuf.capacity()) };
//
//     unsafe {
//         let hdr = libc::cmsghdr {
//             cmsg_len: cmsg_len(mem::size_of_val(fds)),
//             cmsg_level: libc::SOL_SOCKET,
//             cmsg_type: libc::SCM_RIGHTS,
//             ..mem::zeroed()
//         };
//         std::ptr::copy_nonoverlapping(
//             &hdr as *const libc::cmsghdr as *const u8,
//             cmsgbuf[..].as_mut_ptr(),
//             mem::size_of::<libc::cmsghdr>(),
//         );
//         std::ptr::copy_nonoverlapping(
//             fds.as_ptr(),
//             cmsgbuf[cmsg_len(0)..].as_mut_ptr() as _,
//             mem::size_of_val(fds),
//         );
//     };
//
//     let msg = libc::msghdr {
//         msg_name: std::ptr::null_mut(),
//         msg_namelen: 0,
//         msg_iov: &mut iov,
//         msg_iovlen: 1,
//         msg_control: cmsgbuf.as_ptr() as _,
//         msg_controllen: cmsg_space(mem::size_of_val(fds)),
//         msg_flags: 0,
//     };
//
//     unsafe { libc::sendmsg(stream.as_raw_fd(), &msg, 0) };
//
//     Ok(())
// }

use std::os::unix::io::RawFd;

pub fn recv_fds<S>(stream: &S) -> Result<Vec<i32>, io::Error>
where
    S: AsRawFd,
{
    use nix::sys::socket::{recvmsg, CmsgSpace, ControlMessage, MsgFlags};
    use nix::sys::uio::IoVec;

    let mut msg = [0u8; 2]; // "hi"
    let iov = IoVec::from_mut_slice(&mut msg[..]);

    let mut space: CmsgSpace<([RawFd; 1])> = CmsgSpace::new();
    // TODO, convert the nix error to a std::io one?
    let r = recvmsg(
        stream.as_raw_fd(),
        &[iov],
        Some(&mut space),
        MsgFlags::empty(),
    )
    .unwrap();
    match r.cmsgs().next() {
        Some(ControlMessage::ScmRights(fds)) => Ok(fds.into()),
        Some(_) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unexpected control message",
        )),
        None => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "no control message found",
        )),
    }
}

pub fn send_fds<S>(stream: &S, fds: &[RawFd]) -> io::Result<usize>
where
    S: AsRawFd,
{
    use nix::sys::socket::{sendmsg, ControlMessage, MsgFlags};
    use nix::sys::uio::IoVec;

    let iovs = [IoVec::from_slice(b"hi")]; // we have to send something here?
    let cmsgs = [ControlMessage::ScmRights(fds)];
    sendmsg(stream.as_raw_fd(), &iovs, &cmsgs, MsgFlags::empty(), None).map_err(|e| {
        panic!("TODO {}", e);
    })
}
