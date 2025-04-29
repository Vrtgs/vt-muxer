use std::cmp::Ordering;
use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::convert::Infallible;
use std::error;
use std::fmt::{Debug, Formatter};
use std::mem::MaybeUninit;
use std::num::NonZero;
use std::pin::{pin, Pin};
use std::sync::{Arc, Weak};
use std::task::{ready, Context, Poll, Waker};
use parking_lot::{Mutex, MutexGuard};
use tokio::io;
use tokio::io::{AsyncRead, AsyncReadExt, ReadBuf};
use crate::thin_addr::SocketAddr;
use crate::read::read_buffer::ReadBuffer;
use crate::Reader;
use crate::protocol::MuxFrameHeader;

mod read_buffer;

// this will be in an `Arc`, and needs to free the read buffer even if there are still `Weak` references
// and so the buffer is heap-based
#[derive(Debug)]
struct ReaderEntry {
    reader_pipe_waker: Waker,
    waiting_waker: Waker,
    buffer: ReadBuffer,
}

pub struct ReaderInner {
    shared_reader: Arc<SharedReader>,
    entry: Arc<Mutex<ReaderEntry>>
}

#[derive(Debug)]
pub enum SharedReaderState {
    Read {
        owner: SocketAddr,
        left: NonZero<u16>
    },
    NextPacket {
        header: MaybeUninit<MuxFrameHeader>,
        filled: u8,
    },
    SendSocket {
        future: flume::r#async::SendFut<'static, (SocketAddr, ReaderInner)>,
    },
    Error
}


type SocketMap = HashMap<SocketAddr, Weak<Mutex<ReaderEntry>>>;

struct LockedReaderState {
    reader: Reader,
    socket_map: SocketMap,
    state: SharedReaderState,
}

type SocketSender = flume::Sender<(SocketAddr, ReaderInner)>;

pub(super) struct SharedReader {
    locked_state: Mutex<LockedReaderState>,
    new_sockets: SocketSender,
}

const DEFAULT_BUFFER_CAPACITY: NonZero<usize> = NonZero::new((u16::MAX as usize).checked_mul(2).unwrap()).unwrap();

fn next_packet() -> SharedReaderState {
    SharedReaderState::NextPacket { header: MaybeUninit::uninit(), filled: 0 }
}

struct ReaderLock<'a> {
    locked_state: MutexGuard<'a, LockedReaderState>,
    new_sockets: &'a SocketSender,
    inner: &'a Arc<SharedReader>
}

struct ReaderMut<'a> {
    reader: &'a mut Reader,
    state: &'a mut SharedReaderState,
    socket_map: &'a mut SocketMap,
    new_sockets: &'a SocketSender,
    inner: &'a Arc<SharedReader>
}

impl Debug for ReaderMut<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f
            .debug_struct("ReaderMut")
            .field("reader", self.reader)
            .field("state", self.state)
            .field("socket_map", self.socket_map)
            .field("new_sockets", self.new_sockets)
            .finish_non_exhaustive()
    }
}

impl ReaderLock<'_> {
    fn as_mut(&mut self) -> ReaderMut<'_> {
        let state = &mut *self.locked_state;
        ReaderMut {
            reader: &mut state.reader,
            state: &mut state.state,
            socket_map: &mut state.socket_map,
            new_sockets: self.new_sockets,
            inner: self.inner
        }
    }
}

macro_rules! error {
    ($self: ident, $cause: ident, $err:expr) => {{
        return Poll::Ready(Err(
            $self.error(io::ErrorKind::$cause, $err)
        ))
    }};
}

impl ReaderMut<'_> {
    fn insert(&mut self, socket: SocketAddr, cx: &mut Context<'_>) -> io::Result<ReaderInner> {
        match self.socket_map.entry(socket) {
            Entry::Occupied(_) => Err(io::Error::new(io::ErrorKind::AddrInUse, "duplicate socket entry")),
            Entry::Vacant(slot) => {
                let entry = Arc::new(Mutex::new(ReaderEntry {
                    reader_pipe_waker: Waker::noop().clone(),
                    waiting_waker: cx.waker().clone(),
                    buffer: ReadBuffer::new(DEFAULT_BUFFER_CAPACITY)?,
                }));
                
                let weak_entry = Arc::downgrade(&entry);
                slot.insert(weak_entry);
                
                Ok(ReaderInner {
                    shared_reader: Arc::clone(self.inner),
                    entry,
                })
            }
        }
    }
    
    fn remove(&mut self, socket: SocketAddr) {
        if let Some(weak_entry) = self.socket_map.remove(&socket) {
            if let Some(entry) = weak_entry.upgrade() {
                let read_pipe_wake = entry.lock().reader_pipe_waker.clone();

                // ensure weak count hits 0 before waking up read pipe (mark EOS)
                drop(weak_entry);
                debug_assert_eq!(Arc::weak_count(&entry), 0);
                read_pipe_wake.wake()
            }
        }
    }
    
    fn next_packet(&mut self) {
        *self.state = next_packet()
    }

    fn read(&mut self, owner: SocketAddr, amount: NonZero<u16>) {
        *self.state = SharedReaderState::Read { owner, left: amount };
    }
    
    fn error<E: Into<Box<dyn error::Error + Send + Sync>>>(&mut self, kind: io::ErrorKind, error: E) -> io::Error {
        *self.state = SharedReaderState::Error;
        io::Error::new(kind, error)
    }

    #[inline]
    fn poll_read(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let SharedReaderState::Read { owner, ref mut left } = *self.state else {
            unreachable!()
        };
        
        let mut entry_slot = None;
        let mut buffer_inner_lock = None;
        let entry = self.socket_map.get(&owner)
            .and_then(Weak::upgrade)
            .map(|entry| {
                let entry = &**entry_slot.insert(entry);
                let mut_buff = &mut **buffer_inner_lock.insert(entry.lock());
                (&mut mut_buff.buffer, &mut_buff.reader_pipe_waker, &mut mut_buff.waiting_waker)
            });

        let reader = pin!((&mut self.reader).take(left.get().into()));
        
        let read_amount = match entry {
            None => {
                let mut scratch = [MaybeUninit::uninit(); u16::MAX as usize];
                let mut buf = ReadBuf::uninit(&mut scratch);
                ready!(reader.poll_read(cx, &mut buf))?;
                buf.filled().len()
            },
            Some((buffer, reader_pipe_waker, waiter_waker)) => {
                let res = ready!(buffer.fill_buffer(cx, reader)?);
                reader_pipe_waker.wake_by_ref();
                match res {
                    Some(amt) => amt,
                    None => {
                        waiter_waker.clone_from(cx.waker());
                        return Poll::Pending
                    }
                }
            }
        };

        drop(buffer_inner_lock);
        drop(entry_slot);

        if read_amount == 0 { 
            let expected = *left;
            error!(self, UnexpectedEof, format!("expected {expected} more bytes"))
        }
        
        let new_left = match read_amount.try_into().ok().and_then(|amt| left.get().checked_sub(amt)) {
            Some(left) => left,
            None => error!(self, Other, "read more bytes than in provided buffer")
        };
        
        match NonZero::new(new_left) {
            None => self.next_packet(),
            Some(new_left) => *left = new_left
        }
        
        Poll::Ready(Ok(()))
    }
    
    fn poll_next_packet(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let SharedReaderState::NextPacket { header, filled } = self.state else {
            unreachable!()
        };

        let header_bytes = MuxFrameHeader::uninit_bytes(header);
        let mut buffer = ReadBuf::uninit(&mut header_bytes[(*filled) as usize..]);
        
        ready!(Pin::new(&mut self.reader).poll_read(cx, &mut buffer))?;
        let extra_fill = buffer.filled().len();

        if extra_fill == 0 {
            error!(self, UnexpectedEof, "eof on header read")
        }

        let new_filled = (*filled) + extra_fill as u8;

        match new_filled.cmp(&const { size_of::<MuxFrameHeader>() as u8 }) {
            Ordering::Less => {
                *filled = new_filled;
                Poll::Pending
            }
            Ordering::Equal => {
                let header = unsafe { header.assume_init() };
                let addr = header.addr();
                match NonZero::new(header.len()) {
                    Some(len) => self.read(addr, len),
                    None => match self.socket_map.contains_key(&addr) {
                        true => {
                            self.remove(addr);
                            self.next_packet()
                        },
                        false => match self.new_sockets.is_disconnected() {
                            true => self.next_packet(),
                            false => {
                                let reader = self.insert(addr, cx)?;
                                let sender = self.new_sockets.clone();
                                *self.state = SharedReaderState::SendSocket {
                                    future: sender.into_send_async((addr, reader))
                                }
                            }
                        }
                    }
                }
                
                Poll::Ready(Ok(()))
            }
            Ordering::Greater => error!(self, Other, "reader read more bytes than were provided"),
        }
    }
    
    fn poll_once(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match *self.state {
            SharedReaderState::Read { .. } => self.poll_read(cx),
            SharedReaderState::NextPacket { .. } => self.poll_next_packet(cx),
            SharedReaderState::SendSocket {
                future: ref mut sock_fut,
            } => {
                // not really important that the connection was received
                let _ = ready!(Pin::new(sock_fut).poll(cx));
                self.next_packet();
                
                Poll::Ready(Ok(()))
            },
            SharedReaderState::Error => Poll::Ready(Err(io::Error::new(io::ErrorKind::BrokenPipe, "socket hit fatal error")))
        }
    }
    
    fn poll(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<Infallible>> {
        loop {
            ready!(self.poll_once(cx))?;
            ready!(pin!(tokio::task::coop::consume_budget()).poll(cx))
        }
    }
}

impl SharedReader {
    pub fn new(reader: Reader, new_sockets: flume::Sender<(SocketAddr, ReaderInner)>) -> Arc<Self> { 
        Arc::new(SharedReader {
            locked_state: Mutex::new(LockedReaderState {
                reader,
                socket_map: HashMap::new(),
                state: next_packet(),
            }),
            new_sockets
        })
    }
    
    fn lock(self: &Arc<Self>) -> ReaderLock {
        ReaderLock {
            locked_state: self.locked_state.lock(),
            new_sockets: &self.new_sockets,
            inner: self
        }
    }
    
    pub(super) fn poll(self: &Arc<Self>, cx: &mut Context<'_>) -> Poll<io::Result<Infallible>> {
        self.lock().as_mut().poll(cx)
    }

    pub(super) fn add_connection(self: &Arc<Self>, sock: SocketAddr) -> io::Result<ReaderInner> {
        self.lock().as_mut().insert(sock, &mut Context::from_waker(Waker::noop()))
    }
}

impl AsyncRead for ReaderInner {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        // reading an empty slice always does nothing
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()))
        }
        
        let this = Pin::into_inner(self);
        
        // ensure no references are held while polling the shared reader by creating a scope
        {
            let entry = &mut *this.entry.lock();
            let buffer = &mut entry.buffer;
            let remaining_start = buf.remaining();
            buffer.read(buf);

            // if we didn't read anything,
            // there is no need to wake the waiting waker,
            // because there is no one waiting as the buffer is empty
            // the polling only waits if the buffer is full
            if remaining_start != buf.remaining() {
                entry.waiting_waker.wake_by_ref();
                return Poll::Ready(Ok(()));
            }

            if Arc::weak_count(&this.entry) == 0 { 
                return Poll::Ready(Ok(()))
            }
            
            // we are now waiting on the read pipe
            entry.reader_pipe_waker.clone_from(cx.waker());
        }
        
        match ready!(this.shared_reader.poll(cx))? {}
    }
}