use std::fmt;
use std::io;

pub struct LineIo {
    head: usize,
    tail: usize,
    buf: [u8; 16384],
}

impl fmt::Debug for LineIo {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "LineIo {{ head: {}, tail: {} }}", self.head, self.tail)
    }
}

impl LineIo {
    const CAPACITY: usize = 16384;

    pub fn new() -> Self {
        LineIo {
            head: 0,
            tail: 0,
            buf: [0u8; LineIo::CAPACITY],
        }
    }

    // pub fn is_empty(&self) -> bool {
    //     self.head == self.tail
    // }

    // pub fn is_full(&self) -> bool {
    //     self.tail == LineIo::CAPACITY
    // }

    /// read from a reader, appending to our own internal buffer
    pub fn read_from<R>(&mut self, r: &mut R) -> io::Result<usize>
    where
        R: io::Read,
    {
        self.realign_buffer();
        if self.tail >= LineIo::CAPACITY {
            eprintln!("buffer at capacity and we haven't found a line; discarding the buffer");
            // TODO; should this be an error because the peer isn't conforming to protocol?
            self.head = 0;
            self.tail = 0;
        }
        loop {
            debug_assert!(self.buf[self.tail..].len() > 0);
            match r.read(&mut self.buf[self.tail..]) {
                Ok(n) => {
                    self.tail += n;
                    break Ok(n);
                }
                Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
    }

    pub fn find_line(&mut self) -> Option<&[u8]> {
        if self.head < self.tail {
            match memchr::memchr(b'\n', &self.buf[self.head..self.tail]) {
                Some(i) => {
                    let line = &self.buf[self.head..self.head + i + 1];
                    self.head = self.head + i + 1;
                    debug_assert!(self.head <= self.tail);
                    return Some(line);
                }
                None => None,
            }
        } else {
            None
        }
    }

    pub fn find_line_utf8(&mut self) -> io::Result<Option<&str>> {
        Ok(self.find_line()).and_then(|maybe_line| match maybe_line {
            Some(line) => std::str::from_utf8(line).map(Some).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "stream did not contain valid UTF-8",
                )
            }),
            None => Ok(None),
        })
    }

    pub fn realign_buffer(&mut self) {
        if self.head == self.tail {
            self.head = 0;
            self.tail = 0;
        } else if self.head >= LineIo::CAPACITY / 2 {
            let (low, high) = self.buf.split_at_mut(self.head);
            self.tail -= self.head;
            self.head = 0;
            let src = &high[..self.tail];
            let dst = &mut low[..self.tail];
            dst.copy_from_slice(src);
        }
    }
}
