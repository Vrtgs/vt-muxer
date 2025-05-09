use std::io;
use std::io::Error;
use std::pin::{pin, Pin};
use std::sync::Arc;
use std::task::{ready, Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, BufReader, ReadBuf};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use crate::thin_addr::SocketAddr;
use crate::constructor::ConstructExt;
use crate::poll_mutex::PollMutex;
use crate::read::{ReaderInner, SharedReader};
use crate::write::WriteInner;

pub mod thin_addr;
mod poll_mutex;
mod packet_buffer;
mod constructor;
mod write;
mod read;
mod protocol;
mod integers;

type Writer = OwnedWriteHalf;
type Reader = BufReader<OwnedReadHalf>;

/// Represents a multiplexed connection.
///
/// # Usage
///
/// This struct is intended to be used in networking or IPC (Inter-Process
/// Communication) systems where multiplexing is required. The writer and
/// reader components work together to manage input/output streams.
///
/// Note: Ensure proper synchronization and error handling when dealing
/// with concurrent reads and writes to avoid potential data races or
/// inconsistencies.
pub struct MuxConnection {
    write: Box<WriteInner>,
    read: ReaderInner
}

impl MuxConnection {
    fn new(write: Box<WriteInner>, read: ReaderInner) -> Self {
        Self {
            write,
            read
        }
    }
    
    pub fn addr(&self) -> SocketAddr {
        self.write.addr()
    }
}

impl AsyncWrite for MuxConnection {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<Result<usize, Error>> {
        Pin::new(&mut Pin::into_inner(self).write).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        Pin::new(&mut Pin::into_inner(self).write).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        Pin::new(&mut Pin::into_inner(self).write).poll_shutdown(cx)
    }
}

impl AsyncRead for MuxConnection {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut Pin::into_inner(self).read).poll_read(cx, buf)
    }
}


/// # MuxPipe
/// 
/// The client-side interface for creating multiplexed connections over a single TCP stream.
/// This can also be used by the server, but this structure can only initiate connection
/// and can't accept them
///
/// ## Important Notes
/// 
/// - `MuxPipe` is designed to be used with a `MuxListener` on the server side.
/// - The struct implements `Clone`, allowing it to be safely shared between multiple tasks.
/// - All logical connections created through a single `MuxPipe` share the same underlying TCP connection.
/// - The socket addresses used with `add_connection` serve as identifiers for the logical connections and don't represent actual network endpoints.
/// - The implementation uses Tokio for async I/O operations, so it must be used within a Tokio runtime.
#[derive(Clone)]
pub struct MuxPipe {
    write: Arc<Mutex<Writer>>,
    read: Arc<SharedReader>,
}

impl MuxPipe {
    /// Creates a new `MuxPipe` from a TCP stream. This takes ownership of the stream and splits it into read and write halves for multiplexing.
    pub fn new(stream: TcpStream) -> Self {
        MuxListener::with_listener_capacity(stream, 0).into_pipe()
    }
    
    fn make_writer(&self, addr: SocketAddr) -> Box<WriteInner> {
        WriteInner::box_new((addr, PollMutex::new(Arc::clone(&self.write))))
    }
    
    
    /// Creates a new logical connection with the specified socket address. This performs a handshake with the remote end to establish the connection.
    /// 
    /// #### Parameters
    /// - `addr`: The socket address to use for identifying the logical connection
    /// 
    /// #### Returns
    /// - `io::Result<MuxConnection>`: A result containing the new multiplexed connection if successful
    /// 
    /// ## Usage Example
    /// 
    /// ```rust
    /// # use std::io;
    /// # use vt_muxer::MuxPipe;
    /// # use tokio::net::TcpStream;
    /// # use tokio::io::AsyncWriteExt;
    ///
    /// async fn example() -> io::Result<()> {
    ///     // Create a TCP connection to the server
    ///     let tcp_stream = TcpStream::connect("server_address:port").await?;
    ///     
    ///     // Create a multiplexer over the TCP stream
    ///     let mux = MuxPipe::new(tcp_stream);
    ///     
    ///     // Create multiple logical connections over the same TCP stream
    ///     let addr1 = "127.0.0.1:12345".parse().unwrap();
    ///     let mut connection1 = mux.add_connection(addr1).await?;
    ///     
    ///     let addr2 = "127.0.0.1:12346".parse().unwrap();
    ///     let mut connection2 = mux.add_connection(addr2).await?;
    ///     
    ///     // Use the connections independently
    ///     connection1.write_all(b"Data for connection 1").await?;
    ///     connection2.write_all(b"Data for connection 2").await?;
    ///     
    ///     // Don't forget to properly shut down connections when done
    ///     connection1.shutdown().await?;
    ///     connection2.shutdown().await?;
    ///     
    ///     Ok(())
    /// }
    /// ```
    /// 
    ///
    pub async fn add_connection(&self, addr: SocketAddr) -> io::Result<MuxConnection> {
        let reader = self.read.add_connection(addr)?;
        let mut writer = self.make_writer(addr);
        writer.handshake().await?;
        Ok(MuxConnection::new(writer, reader))
    }
}




/// `MuxListener` is a structure designed to manage and listen for incoming multiplexed connections.

///
/// # Purpose
/// The `MuxListener` serves as an abstraction to handle incoming connections from a MuxPipe,
/// please note that once discarded there is no way of listening to new incoming connections
pub struct MuxListener {
    pipe: MuxPipe,
    receiver: flume::Receiver<(SocketAddr, ReaderInner)>
}

impl MuxListener {
    pub fn new(stream: TcpStream) -> Self {
        Self::with_listener_capacity(stream, 1)
    }

    fn with_listener_capacity(stream: TcpStream, capacity: usize) -> Self {
        let (read, write) = stream.into_split();
        let reader = BufReader::new(read);
        let (sender, receiver) = flume::bounded(capacity);
        let read = SharedReader::new(reader, sender);
        let write = Arc::new(Mutex::new(write));
        
        Self {
            pipe: MuxPipe { write, read },
            receiver
        }
    }
    
    pub async fn add_connection(&self, addr: SocketAddr) -> io::Result<MuxConnection> {
        self.pipe.add_connection(addr).await
    }
    
    pub async fn accept(&self) -> io::Result<MuxConnection> {
        let mut fut = pin!(self.receiver.recv_async());
        let (addr, reader) = std::future::poll_fn(move |cx| {
            if let Poll::Ready(res) = fut.as_mut().poll(cx) { 
                return Poll::Ready(Ok::<_, Error>(res.expect("receiver should never close")))
            }
            
            match ready!(self.pipe.read.poll(cx))? {}
        }).await?;
        let writer = self.pipe.make_writer(addr);
        Ok(MuxConnection::new(writer, reader))
    }

    pub fn pipe(&self) -> &MuxPipe {
        &self.pipe
    }
    
    pub fn into_pipe(self) -> MuxPipe {
        self.pipe
    }
}

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use tokio::net::TcpListener;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn dummy_addr() -> SocketAddr {
        // Use a dummy address (you may need to adapt this depending on your SocketAddr type)
        "127.0.0.1:12345".parse().unwrap()
    }
    
    async fn mux_pipe() -> (MuxListener, MuxPipe) {
        // Setup a real TCP listener
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        // Connect two TcpStreams
        let server = async {
            let (stream, _) = listener.accept().await.unwrap();
            MuxListener::new(stream)
        };

        let client = async {
            MuxPipe::new(TcpStream::connect(addr).await.unwrap())
        };

        tokio::join!(server, client)
    }

    #[tokio::test]
    async fn test_mux_listener_accept_connection() {
        let (mux_listener, conn) = mux_pipe().await;
        
        // Add a new connection from the client side
        let client_task = async {
            let mut mux_conn = conn.add_connection(dummy_addr()).await.unwrap();
            
            mux_conn.write_all(b"hello world").await.unwrap();
            mux_conn.flush().await.unwrap();
            mux_conn.shutdown().await.unwrap();
        };

        // Accept connection from the server side
        let server_task = async {
            let mut accepted = mux_listener.accept().await.unwrap();
            let mut buf = vec![];
            let n = accepted.read_to_end(&mut buf).await.unwrap();
            let received = &buf[..n];
            assert_eq!(received, b"hello world");
        };

        tokio::join!(client_task, server_task);
    }

    #[tokio::test]
    async fn test_mux_pipe_add_connection_multiple_times() {
        let (mux_pipe_server, mux_pipe_client) = mux_pipe().await;

        // Open two different connections
        let addr1 = dummy_addr();
        let addr2 = "127.0.0.1:12346".parse::<SocketAddr>().unwrap();

        let client_task = async {
            let handle = async |addr, bytes| {
                let mut conn = mux_pipe_client.add_connection(addr).await?;
                conn.write_all(bytes).await?;
                conn.flush().await?;
                conn.shutdown().await
            };
            
            tokio::try_join!(handle(addr1, b"first connection"), handle(addr2, b"second connection"))
        };

        let server_task = async {
            let (mut conn1, mut conn2) = {
                let conn1 = mux_pipe_server.accept().await?;
                let conn2 = mux_pipe_server.accept().await?;
                
                match (conn1.addr(), conn2.addr()) {
                    (con1, con2) if con1 == addr1 && con2 == addr2 => {
                        (conn1, conn2)
                    }
                    (con1, con2) if con1 == addr2 && con2 == addr1 => {
                        (conn2, conn1)
                    }
                    _ => unreachable!()
                }
            };
            
            let mut buf1 = vec![];
            let n1 = conn1.read_to_end(&mut buf1).await?;
            assert_eq!(&buf1[..n1], b"first connection");

            let mut buf2 = vec![];
            let n2 = conn2.read_to_end(&mut buf2).await?;
            assert_eq!(&buf2[..n2], b"second connection");
            Ok(())
        };

        tokio::try_join!(client_task, server_task).unwrap();
    }
}
