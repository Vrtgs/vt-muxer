# VT-Muxer

the Vrtgs Tcp Muxer

```toml
[dependencies]
vt-muxer = "0.1.0"
```

### Basic Example
```rust
use vt_muxer::{MuxListener, MuxPipe, MuxConnection};
use tokio::net::TcpStream;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

// Server side
async fn run_server(tcp_stream: TcpStream) {
    let mux = MuxListener::new(tcp_stream);
    
    // Accept new multiplexed connections
    while let Ok(mut connection) = mux.accept().await {
        tokio::spawn(async move {
            let mut buf = vec![0; 1024];
            while let Ok(n) = connection.read(&mut buf).await {
                if n == 0 { break; }
                // Handle data...
            }
        });
    }
}

// Client side
async fn run_client(tcp_stream: TcpStream) {
    let mux = MuxPipe::new(tcp_stream);
    
    // Create a new multiplexed connection
    let addr = "127.0.0.1:12345".parse().unwrap();
    let mut connection = mux.add_connection(addr).await.unwrap();
    
    // Use the connection
    connection.write_all(b"Hello world!").await.unwrap();
}
```
## Core Types

### MuxListener

The server-side multiplexer that accepts new logical connections:

- `new(stream: TcpStream) -> MuxListener` - Create a new multiplexer from a TCP stream
- `accept() -> Future<Result<MuxConnection>>` - Accept a new multiplexed connection
- `add_connection(addr: SocketAddr) -> Future<Result<MuxConnection>>` - Explicitly add a new connection

### MuxPipe

The client-side interface for creating new multiplexed connections:

- `new(stream: TcpStream) -> MuxPipe` - Create a new multiplexer from a TCP stream
- `add_connection(addr: SocketAddr) -> Future<Result<MuxConnection>>` - Create a new multiplexed connection

### MuxConnection

Represents a single _**Buffered**_ multiplexed connection:

- Implements `AsyncRead` and `AsyncWrite` for standard async I/O operations
- `addr() -> SocketAddr` - Get the address associated with this connection


MuxConnection should always be shutdown, otherwise a task will be spawned to shutdown the connection

## Address Types

The library provides optimized address types that merge Ipv4 and Ipv6 addresses into one using [Ipv6 mapped/compatible addresses](https://www.rfc-editor.org/rfc/rfc4291.html#section-2.5.5.1) for network operations:

- `SocketAddr` - A compact socket address representation
- `IpAddr` - A lightweight IP address type

## Performance

- Zero-copy packet handling using `bytemuck`
- Efficient multiplexing with minimal overhead
- Optimized for high-throughput scenarios
- Spawning of tasks/threads only occurs when a MuxConnection is dropped without being shutdown
