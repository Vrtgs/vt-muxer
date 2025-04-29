use std::convert::Infallible;
use std::io;
use std::io::Error;
use std::pin::Pin;
use std::sync::LazyLock;
use std::task::{ready, Context, Poll, Waker};
use replace_with::replace_with_or_abort_and_return;
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::runtime::Handle;
use tokio::sync::OwnedMutexGuard;
use crate::thin_addr::SocketAddr;
use crate::constructor::Construct;
use crate::packet_buffer::PacketBuffer;
use crate::poll_mutex::PollMutex;
use crate::Writer;
use crate::protocol::MuxFrameHeader;

enum ShutDownState {
    WaitingLock,
    Flushing {
        lock: OwnedMutexGuard<Writer>,
        written: u32
    },
    Writing {
        lock: OwnedMutexGuard<Writer>,
        written: u8
    },
    Done,
    Error
}

enum WriteState {
    Buffering,
    Flushing {
        lock: OwnedMutexGuard<Writer>,
        written: u32
    },
}

enum WritePipeState {
    Poisoned,
    Shutdown(ShutDownState),
    Writing(Waker, WriteState)
}

struct PoisonOnPanic<'a>(&'a mut WritePipeState);

impl<'a> PoisonOnPanic<'a> {
    fn new(state: &'a mut WritePipeState) -> Self {
        if let WritePipeState::Poisoned = state {
            write_poisoned()
        }

        Self(state)
    }
}

#[cold]
pub fn write_poisoned() -> ! {
    panic!("write instance previously poisoned")
}

impl Drop for PoisonOnPanic<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            *self.0 = WritePipeState::Poisoned
        }
    }
}

pub(super) struct WriteInner {
    write_lock: PollMutex<Writer>,
    state: WritePipeState,
    buffer: PacketBuffer,
}

unsafe impl Construct for WriteInner {
    type Data = (SocketAddr, PollMutex<Writer>);

    unsafe fn init(this: *mut Self, (socket, write): Self::Data) {
        unsafe {
            std::ptr::write(&raw mut (*this).write_lock, write);
            std::ptr::write(&raw mut (*this).state, WritePipeState::Writing(
                Waker::noop().clone(),
                WriteState::Buffering
            ));
            PacketBuffer::init(&raw mut (*this).buffer, socket);
        }
    }
}

macro_rules! get_write_state {
    (($write_lock:pat, $write_state: pat) = $state:ident) => {
        let ($write_lock, $write_state) = match $state.0 {
            WritePipeState::Poisoned => unreachable!(),
            WritePipeState::Shutdown(_) => return Poll::Ready(Err(io::Error::from(io::ErrorKind::BrokenPipe))),
            WritePipeState::Writing(waker, state) => (waker, state),
        };
    };
}

fn poll_locked<'a>(state: &'a mut WriteState, write_lock: &mut PollMutex<Writer>, cx: &mut Context<'_>) -> Poll<(&'a mut Writer, &'a mut u32)> {
    Poll::Ready(match state {
        WriteState::Buffering => {
            let lock = ready!(write_lock.poll_lock(cx));

            *state = WriteState::Flushing {
                lock,
                written: 0
            };

            let WriteState::Flushing { lock, written } = state else {
                unreachable!()
            };

            (lock, written)
        },
        WriteState::Flushing { lock, written } => {
            (lock, written)
        }
    })
}

trait WriteCounter {
    fn current_count(&self) -> u32;

    fn add(&mut self, amt: usize);
}

macro_rules! impl_cnt {
    ($ty: ty) => {
        impl WriteCounter for $ty {
            fn current_count(&self) -> u32 {
                (*self) as u32
            }

            fn add(&mut self, amt: usize) {
                *self = self.checked_add(amt as $ty).expect("stream wrote more data than provided")
            }
        }
    };
}


impl_cnt!(u8);
impl_cnt!(u32);

fn flush_buf(
    cx: &mut Context<'_>,
    buffer: &mut PacketBuffer,
    lock: &mut Writer,
    written: &mut dyn WriteCounter
) -> Poll<io::Result<()>> {
    loop {
        let bytes = buffer.bytes_from(written.current_count());
        if bytes.is_empty() {
            buffer.clear();
            return Poll::Ready(Ok(()));
        }

        let mut lock = Pin::new(&mut *lock);
        match lock.as_mut().poll_write(cx, bytes) {
            Poll::Ready(Ok(0)) => return Poll::Ready(Err(Error::new(
                io::ErrorKind::WriteZero,
                "failed to flush packet buffer",
            ))),
            Poll::Ready(Ok(wrote_now)) => written.add(wrote_now),
            Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
            Poll::Pending => return Poll::Pending,
        }
    }
}

impl WriteInner {
    pub(crate) fn addr(&self) -> SocketAddr {
        self.buffer.addr()
    }
    pub(crate) async fn handshake(&mut self) -> io::Result<()> {
        assert!(matches!(self.state, WritePipeState::Writing(_, WriteState::Buffering)));
        assert!(self.buffer.is_empty());

        let mut lock = self.write_lock.lock().await;
        lock.write_all(self.buffer.bytes()).await
    }
}

impl AsyncWrite for WriteInner {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, mut buf: &[u8]) -> Poll<Result<usize, Error>> {
        let this = Pin::into_inner(self);
        let state = PoisonOnPanic::new(&mut this.state);

        get_write_state!((waker, write_state) = state);
        waker.clone_from(cx.waker());

        let mut wrote = 0;

        while !buf.is_empty() {
            macro_rules! push {
                ($wrote: ident, $already_wrote:expr) => {
                    let buffer_wrote = usize::from(this.buffer.push(buf, $already_wrote));
                    buf = &buf[buffer_wrote..];
                    $wrote += buffer_wrote;
                };
            }

            macro_rules! flush {
                ($writer:ident, $amt:ident) => {
                    match flush_buf(cx, &mut this.buffer, $writer, $amt) {
                        Poll::Ready(res) => {
                            *write_state = WriteState::Buffering;
                            res?
                        },
                        Poll::Pending if wrote != 0 => return Poll::Ready(Ok(wrote)),
                        Poll::Pending => return Poll::Pending
                    }
                };
            }

            match write_state {
                WriteState::Buffering => {
                    if this.buffer.is_full() {
                        let (writer, amt) = ready!(poll_locked(write_state, &mut this.write_lock, cx));
                        flush!(writer, amt);
                    }

                    push!(wrote, 0);
                }
                WriteState::Flushing { lock, written } => {
                    if !this.buffer.must_flush_for(*written) {
                        push!(wrote, *written);
                    }

                    let lock = &mut **lock;
                    flush!(lock, written)
                }
            }
        }
        
        Poll::Ready(Ok(wrote))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        let this = Pin::into_inner(self);
        let state = PoisonOnPanic::new(&mut this.state);

        get_write_state!((waker, write_state) = state);

        waker.clone_from(cx.waker());

        macro_rules! lock {
            ($lock: pat, $written: pat) => {
                let ($lock, $written) = ready!(poll_locked(write_state, &mut this.write_lock, cx));
            };
        }
        
        if !this.buffer.is_empty() {
            lock!(lock, written);
            // the buffer isn't empty
            if let Err(err) = ready!(flush_buf(cx, &mut this.buffer, lock, written)) {
                *write_state = WriteState::Buffering;
                return Poll::Ready(Err(err))
            }
            
            debug_assert!(this.buffer.is_empty());
        }

        lock!(lock, _);
        Pin::new(lock).poll_flush(cx).map(|res| {
            *write_state = WriteState::Buffering;
            res
        })
    }
    
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        let this = Pin::into_inner(self);
        let state = PoisonOnPanic::new(&mut this.state);

        let state = match &mut *state.0 {
            WritePipeState::Poisoned => unreachable!(),
            WritePipeState::Shutdown(state) => state,
            write_state @ WritePipeState::Writing(..) => {
                let waker = replace_with_or_abort_and_return(write_state, |old_state| {
                    let WritePipeState::Writing(waker, state) = old_state else {
                        unreachable!()
                    };

                    let state = WritePipeState::Shutdown(match state {
                        WriteState::Buffering => ShutDownState::WaitingLock,
                        WriteState::Flushing { lock, written } => {
                            ShutDownState::Flushing { lock, written }
                        },
                    });

                    (waker, state)
                });

                let WritePipeState::Shutdown(state) = state.0 else {
                    unreachable!()
                };
                waker.wake();

                state
            }
        };

        macro_rules! guarded_flush {
            ($($tt:tt)*) => {
                if let Err(err) = ready!(flush_buf($($tt)*)) {
                    *state = ShutDownState::Error;
                    return Poll::Ready(Err(err))
                }
            };
        }
        
        loop {
            match state {
                ShutDownState::WaitingLock => {
                    let lock = ready!(this.write_lock.poll_lock(cx));
                    *state = ShutDownState::Flushing { 
                        lock,
                        written: 0
                    }
                }
                ShutDownState::Flushing { lock, written } => {
                    let lock = &mut **lock;
                    if !this.buffer.is_empty() {
                        guarded_flush!(cx, &mut this.buffer, lock, written);
                    }

                    replace_with::replace_with_or_abort(state, |state| {
                        match state {
                            ShutDownState::Flushing { lock, .. } => {
                                ShutDownState::Writing {
                                    lock,
                                    written: 0
                                }
                            },
                            _ => unreachable!()
                        }
                    })
                }
                ShutDownState::Writing { lock, written } => {
                    let lock = &mut **lock;
                    debug_assert!(this.buffer.is_empty());
                    guarded_flush!(cx, &mut this.buffer, lock, written);
                    *state = ShutDownState::Done;
                }
                ShutDownState::Done => return Poll::Ready(Ok(())),
                ShutDownState::Error => return Poll::Ready(Err(Error::other("mux connection shutdown previously failed"))),
            }
        }
    }
}

impl Drop for WriteInner {
    fn drop(&mut self) {
        let writer = match std::mem::replace(&mut self.state, WritePipeState::Poisoned) {
            WritePipeState::Shutdown(_) | WritePipeState::Poisoned => return,
            WritePipeState::Writing(_, WriteState::Flushing {
                lock,
                written: written @ 1..
            }) => {
                let rest_data = Box::<[u8]>::from(self.buffer.bytes_from(written));
                Err(Err((lock, rest_data)))
            },
            WritePipeState::Writing(_, WriteState::Flushing { lock, written: 0 }) => {
                Err(Ok(lock))
            }
            WritePipeState::Writing(_, WriteState::Buffering) => Ok(self.write_lock.take_token())
        };

        let socket = self.buffer.addr();

        let future = async move {
            let mut mutex;
            let mut late_lock;
            let mut early_lock;

            let lock = match writer {
                Ok(lock) => {
                    mutex = lock;
                    late_lock = mutex.lock().await;

                    &mut *late_lock
                }
                Err(Ok(locked)) => {
                    early_lock = locked;
                    &mut *early_lock
                },
                Err(Err((to_flush, data))) => {
                    early_lock = to_flush;

                    let Ok(()) = early_lock.write_all(&data).await else {
                        return;
                    };

                    &mut *early_lock
                }
            };

            let eof_header = MuxFrameHeader::empty(socket);
            let _ = lock.write_all(bytemuck::bytes_of(&eof_header)).await;
        };

        static GLOBAL_RT: LazyLock<Handle> =  LazyLock::new(|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_io()
                .build()
                .unwrap();

            let handle = runtime.handle().clone();

            std::thread::spawn(move || {
                runtime.block_on(std::future::pending::<Infallible>())
            });

            handle
        });

        // runtime may shutdown any time, and that will ruin the rest of the connections by mangling the protocol.
        // run the future in a new thread, so even if the runtime running this future is shutdown,
        // as long as the original runtime where the TcpStream was first created is not shutdown
        // this future will still run to completion

        // but since this global runtime never enters shutdown so we can just spawn directly
        // and as long as the program is running it is in our best interest to keep the protocol intact
        let _ = GLOBAL_RT.spawn(future);
    }
}


#[cfg(test)]
#[cfg(not(miri))]
mod tests {
    use std::mem::MaybeUninit;
    use std::net::Ipv4Addr;
    use std::sync::Arc;
    use bytemuck::Zeroable;
    use rand::{Rng, RngCore, SeedableRng};
    use rand_xoshiro::Xoshiro512StarStar;
    use tokio::io::{AsyncRead, AsyncReadExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::Mutex;
    use crate::constructor::ConstructExt;
    use crate::protocol::MuxFrameHeader;
    use super::*;

    async fn read_header<R: AsyncRead + Unpin>(reader: &mut R) -> io::Result<MuxFrameHeader> {
        let mut this = MuxFrameHeader::zeroed();
        let init_bytes = bytemuck::bytes_of_mut(&mut this);
        <R as AsyncReadExt>::read_exact(reader, init_bytes).await?;
        Ok(this)
    }
    
    async fn make_pipe() -> (TcpStream, Box<WriteInner>) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (_read, writer) = TcpStream::connect(addr).await.unwrap().into_split();

        let mut writer = WriteInner::box_new((
            SocketAddr::from_std(writer.local_addr().unwrap()),
            PollMutex::new(Arc::new(Mutex::new(writer)))
        ));

        writer.handshake().await.unwrap();
        (listener.accept().await.unwrap().0, writer)
    }

    #[tokio::test]
    async fn test_drop() {
        let (mut stream, writer) = make_pipe().await;
        drop(writer);

        let handshake_header = read_header(&mut stream).await.unwrap();
        let start_addr = handshake_header.addr();
        eprintln!("got connection: {}", start_addr);
        assert_eq!(handshake_header.len(), 0);

        let shutdown_header = read_header(&mut stream).await.unwrap();
        assert_eq!(start_addr, shutdown_header.addr());
        eprintln!("closed connection: {start_addr}");
        assert_eq!(shutdown_header.len(), 0);

        assert_eq!(stream.read(&mut [0]).await.unwrap(), 0, "stream had too much data")
    }

    
    pub fn generate_payload<const N: usize>() -> Arc<[u8; N]> {
        // speed is needed here so tests don't take an astronomical amount of time to run
        
        let mut payload = Arc::<[u8; N]>::new_uninit();
        
        let mut rng = Xoshiro512StarStar::from_os_rng();

        let payload_mut = Arc::get_mut(&mut payload).unwrap();
        unsafe {
            let byte = rng.next_u32() as u8;
            std::ptr::write_bytes(
                payload_mut,
                byte,
                1
            );
            MaybeUninit::assume_init_mut(payload_mut);
        };
        
        unsafe { payload.assume_init() }
    }
    
    #[tokio::test]
    async fn test_write() {
        let (mut stream, mut writer) = make_pipe().await;
        
        let payload = generate_payload::<{ 1 << 30 }>();
        
        let send_payload = Arc::clone(&payload);
        tokio::spawn(async move {
            let mut rng = Xoshiro512StarStar::from_os_rng();
            
            // make sure empty flushes do nothing
            writer.flush().await.unwrap();
            let mut payload = send_payload.as_slice();
            while !payload.is_empty() {
                let split_point = rng.random_range(1..=(payload.len()/2).max(1));
                let (to_write, rest) = payload.split_at(split_point);
                payload = rest;
                writer.write_all(to_write).await.unwrap();
                if rng.random_bool(1.0/3.0) {
                    writer.flush().await.unwrap()
                }
            }
            writer.shutdown().await.unwrap();
        });

        let mut received_payload = Vec::with_capacity(payload.len());

        let handshake_header = read_header(&mut stream).await.unwrap();
        let addr = handshake_header.addr();
        eprintln!("got connection: {addr}");
        assert_eq!(handshake_header.len(), 0);

        loop {
            let header = read_header(&mut stream).await.unwrap();
            assert_eq!(header.addr(), addr);
            let len = header.len();
            if len == 0 {
                eprintln!("received shutdown");
                break
            }

            let rcv = (&mut stream).take(len.into()).read_to_end(&mut received_payload).await.unwrap();
            assert_eq!(rcv, len.into(), "early eof expected {len} bytes, found {rcv}");
        }

        assert_eq!(stream.read(&mut [0]).await.unwrap(), 0, "stream had too much data");
        assert_eq!(payload.len(), received_payload.len());
        let same_payload = (*payload).eq(&*received_payload);
        assert!(same_payload);
    }

    #[tokio::test]
    async fn test_parallel_independent_sockets() {
        // Number of sockets to test concurrently
        const SOCKET_COUNT: usize = 200;

        // Create a shared listener
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        eprintln!("Listening on {}", addr);

        // Start accepting connections in the background
        let accept_task = tokio::spawn(async move {
            let mut connections = Vec::with_capacity(SOCKET_COUNT);

            // Accept all connections
            for i in 0..SOCKET_COUNT {
                let (stream, client_addr) = listener.accept().await.unwrap();
                eprintln!("Accepted connection {} from {}", i, client_addr);
                connections.push(stream);
            }

            // Return the connections
            connections
        });

        // Create multiple writers
        let mut writers = Vec::with_capacity(SOCKET_COUNT);
        let mut payload_refs = Vec::with_capacity(SOCKET_COUNT);

        // Generate a different payload for each connection
        for i in 0..SOCKET_COUNT {
            // Connect to the listener
            let (_read, writer) = TcpStream::connect(addr).await.unwrap().into_split();

            // Create a WriteInner
            let mut inner_writer = WriteInner::box_new((
                SocketAddr::from_std(writer.local_addr().unwrap()),
                PollMutex::new(Arc::new(Mutex::new(writer)))
            ));

            // Perform handshake
            inner_writer.handshake().await.unwrap();

            // Create a unique payload for this connection
            // Smaller payload for parallel test
            
            let mut payload = generate_payload::<8192>();
            // Mark the first byte with the connection index to help with debugging
            
            Arc::get_mut(&mut payload).unwrap()[0] = (i % 256) as u8;
            payload_refs.push(Arc::clone(&payload));
            writers.push(inner_writer);
        }

        // Start all writers concurrently
        let write_tasks: Vec<_> = writers
            .into_iter()
            .zip(payload_refs.iter())
            .enumerate()
            .map(|(i, (mut writer, payload))| {
                let payload = Arc::clone(payload);
                tokio::spawn(async move {
                    eprintln!("Starting writer {}", i);
                    // Write the payload in chunks
                    let mut payload_slice = &payload[..];
                    while !payload_slice.is_empty() {
                        let chunk_size = rand::random_range(1..=std::cmp::min(payload_slice.len(), 1024));
                        let (to_write, rest) = payload_slice.split_at(chunk_size);
                        payload_slice = rest;

                        writer.write_all(to_write).await.unwrap();

                        // Occasionally flush
                        if rand::random_bool(0.1) {
                            writer.flush().await.unwrap();
                        }

                        // Add a small delay sometimes to test interleaving
                        if rand::random_bool(0.05) {
                            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                        }
                    }

                    // Proper shutdown
                    writer.shutdown().await.unwrap();
                    eprintln!("Writer {} completed", i);

                    payload
                })
            })
            .collect();

        // Wait for all connections to be accepted
        let streams = accept_task.await.unwrap();
        assert_eq!(streams.len(), SOCKET_COUNT, "Failed to accept all connections");

        // Process all received data
        let receive_tasks: Vec<_> = streams
            .into_iter()
            .enumerate()
            .map(|(i, mut stream)| {
                tokio::spawn(async move {
                    let mut received_payload = Vec::new();

                    // Read handshake header
                    let handshake_header = read_header(&mut stream).await.unwrap();
                    let addr = handshake_header.addr();
                    eprintln!("Connection {}: Got handshake from {}", i, addr);
                    assert_eq!(handshake_header.len(), 0);

                    // Read all data frames
                    loop {
                        let header = read_header(&mut stream).await.unwrap();
                        assert_eq!(header.addr(), addr, "Address mismatch in header");

                        let len = header.len();
                        if len == 0 {
                            eprintln!("Connection {}: Received shutdown", i);
                            break;
                        }

                        let expected_len = len as usize;

                        // Read the frame data
                        let bytes_read = (&mut stream)
                            .take(len.into())
                            .read_to_end(&mut received_payload)
                            .await
                            .unwrap();

                        assert_eq!(
                            bytes_read,
                            expected_len,
                            "Connection {}: Early EOF expected {} bytes, found {}",
                            i, expected_len, bytes_read
                        );

                        eprintln!(
                            "Connection {}: Read frame with {} bytes (total: {})",
                            i, bytes_read, received_payload.len()
                        );
                    }

                    // Verify we've reached EOF
                    let mut buf = [0u8; 1];
                    assert_eq!(
                        stream.read(&mut buf).await.unwrap(),
                        0,
                        "Connection {}: Stream had unexpected data after shutdown",
                        i
                    );

                    (i, received_payload)
                })
            })
            .collect();

        // Wait for all write tasks to complete
        let sent_payloads = futures::future::join_all(write_tasks)
            .await
            .into_iter()
            .map(|r| r.unwrap())
            .collect::<Vec<_>>();

        // Wait for all receive tasks to complete
        let received_data = futures::future::join_all(receive_tasks)
            .await
            .into_iter()
            .map(|r| r.unwrap())
            .collect::<Vec<_>>();

        // Verify all data was received correctly
        for (conn_idx, received_payload) in received_data {
            let sent_payload = &sent_payloads[conn_idx];

            assert_eq!(
                sent_payload.len(),
                received_payload.len(),
                "Connection {}: Payload length mismatch, sent {} bytes but received {}",
                conn_idx, sent_payload.len(), received_payload.len()
            );

            assert!(
                (**sent_payload).eq(&*received_payload),
                "Connection {}: Received data does not match sent data",
                conn_idx
            );

            eprintln!("Connection {}: Data verified successfully", conn_idx);
        }

        eprintln!("All {} connections completed successfully", SOCKET_COUNT);
    }
}