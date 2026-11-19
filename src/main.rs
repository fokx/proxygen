use clap::Parser;
use socket2::{SockRef, TcpKeepalive};
use std::io::ErrorKind;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;

const COPY_BUF_SIZE: usize = 16 * 1024;

#[derive(clap::Parser, Debug, Clone)]
#[command(version, about, long_about = None)]
struct Cli {
    /// Upstream port to forward traffic to
    #[arg(short, long)]
    from: u16,
    /// Local port to listen on
    #[arg(short, long)]
    to: u16,
    /// Local address to bind to
    #[arg(long, default_value = "127.0.0.1")]
    bind: String,
    /// Upstream host to connect to
    #[arg(long, default_value = "127.0.0.1")]
    upstream: String,
    /// Upstream TCP connect timeout in seconds
    #[arg(long, default_value_t = 10)]
    connect_timeout: u64,
    /// Idle connection timeout in seconds (0 to disable)
    #[arg(long, default_value_t = 300)]
    idle_timeout: u64,
    /// Half-closed connection drain idle timeout in seconds (0 to disable)
    #[arg(long, default_value_t = 15)]
    half_close_timeout: u64,
    /// Maximum concurrent active connections
    #[arg(long, default_value_t = 4096)]
    max_connections: usize,
}

#[derive(Debug, Clone, Copy)]
struct RelayConfig {
    connect_timeout: Duration,
    idle_timeout: Duration,
    half_close_timeout: Duration,
}

fn configure_tcp_stream(stream: &TcpStream) {
    if let Err(e) = stream.set_nodelay(true) {
        log::debug!("failed to set TCP_NODELAY: {}", e);
    }

    let keepalive = TcpKeepalive::new()
        .with_time(Duration::from_secs(60))
        .with_interval(Duration::from_secs(15));
    #[cfg(any(
        target_os = "linux",
        target_os = "android",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "macos"
    ))]
    let keepalive = keepalive.with_retries(4);

    if let Err(e) = SockRef::from(stream).set_tcp_keepalive(&keepalive) {
        log::debug!("failed to set TCP keepalive: {}", e);
    }
}

fn is_transient_accept_error(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted | ErrorKind::Interrupted
    )
}

async fn copy_one_way<R, W>(
    reader: &mut R,
    writer: &mut W,
    start: Instant,
    last_activity_ms: &AtomicU64,
) -> std::io::Result<u64>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buf = vec![0u8; COPY_BUF_SIZE];
    let mut total = 0u64;
    loop {
        let n = reader.read(&mut buf).await?;
        if n == 0 {
            let _ = writer.shutdown().await;
            return Ok(total);
        }
        last_activity_ms.store(start.elapsed().as_millis() as u64, Ordering::Relaxed);
        writer.write_all(&buf[..n]).await?;
        last_activity_ms.store(start.elapsed().as_millis() as u64, Ordering::Relaxed);
        total += n as u64;
    }
}

async fn wait_for_idle(start: Instant, last_activity_ms: &AtomicU64, timeout: Duration) {
    if timeout.is_zero() {
        std::future::pending::<()>().await;
        return;
    }
    let timeout_ms = timeout.as_millis() as u64;
    loop {
        let last = last_activity_ms.load(Ordering::Relaxed);
        let now = start.elapsed().as_millis() as u64;
        let elapsed = now.saturating_sub(last);
        if elapsed >= timeout_ms {
            return;
        }
        tokio::time::sleep(Duration::from_millis(timeout_ms - elapsed)).await;
    }
}

async fn relay(
    mut inbound: TcpStream,
    mut outbound: TcpStream,
    idle_timeout: Duration,
    half_close_timeout: Duration,
) {
    let (mut ri, mut wi) = inbound.split();
    let (mut ro, mut wo) = outbound.split();

    let start = Instant::now();
    let last_activity_ms = AtomicU64::new(0);

    let c2s = copy_one_way(&mut ri, &mut wo, start, &last_activity_ms);
    let s2c = copy_one_way(&mut ro, &mut wi, start, &last_activity_ms);
    tokio::pin!(c2s);
    tokio::pin!(s2c);

    tokio::select! {
        res = &mut c2s => {
            match res {
                Ok(_) => {
                    last_activity_ms.store(start.elapsed().as_millis() as u64, Ordering::Relaxed);
                    tokio::select! {
                        _ = &mut s2c => {}
                        _ = wait_for_idle(start, &last_activity_ms, half_close_timeout) => {
                            log::debug!("half-close idle timeout waiting for upstream->client");
                        }
                    }
                }
                Err(e) => {
                    log::debug!("client->upstream copy error: {}", e);
                }
            }
        }
        res = &mut s2c => {
            match res {
                Ok(_) => {
                    last_activity_ms.store(start.elapsed().as_millis() as u64, Ordering::Relaxed);
                    tokio::select! {
                        _ = &mut c2s => {}
                        _ = wait_for_idle(start, &last_activity_ms, half_close_timeout) => {
                            log::debug!("half-close idle timeout waiting for client->upstream");
                        }
                    }
                }
                Err(e) => {
                    log::debug!("upstream->client copy error: {}", e);
                }
            }
        }
        _ = wait_for_idle(start, &last_activity_ms, idle_timeout) => {
            log::debug!("connection idle timeout");
        }
    }
}

async fn handle_connection(
    inbound: TcpStream,
    peer_addr: SocketAddr,
    upstream_addr: String,
    cfg: RelayConfig,
) {
    configure_tcp_stream(&inbound);

    let outbound = match tokio::time::timeout(
        cfg.connect_timeout,
        TcpStream::connect(&upstream_addr),
    )
    .await
    {
        Ok(Ok(stream)) => stream,
        Ok(Err(e)) => {
            log::warn!(
                "failed to connect to upstream {} for {}: {}",
                upstream_addr,
                peer_addr,
                e
            );
            return;
        }
        Err(_) => {
            log::warn!(
                "timed out connecting to upstream {} for {}",
                upstream_addr,
                peer_addr
            );
            return;
        }
    };

    configure_tcp_stream(&outbound);
    relay(inbound, outbound, cfg.idle_timeout, cfg.half_close_timeout).await;
}

async fn run_server(
    listener: TcpListener,
    upstream_addr: String,
    cfg: RelayConfig,
    max_connections: usize,
) {
    let semaphore = Arc::new(Semaphore::new(max_connections.max(1)));

    loop {
        let (inbound, peer_addr) = match listener.accept().await {
            Ok(conn) => conn,
            Err(e) => {
                if is_transient_accept_error(&e) {
                    log::debug!("transient accept error: {}", e);
                    continue;
                }
                log::error!("accept error (backing off 100ms): {}", e);
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };

        log::debug!("request from {}", peer_addr);

        let permit = match semaphore.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                log::warn!(
                    "max connections ({}) reached, rejecting connection from {}",
                    max_connections,
                    peer_addr
                );
                drop(inbound);
                continue;
            }
        };

        let upstream = upstream_addr.clone();
        tokio::spawn(async move {
            let _permit = permit;
            handle_connection(inbound, peer_addr, upstream, cfg).await;
        });
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = Cli::parse();

    let listen_addr = format!("{}:{}", args.bind, args.to);
    let upstream_addr = format!("{}:{}", args.upstream, args.from);
    log::warn!(
        "forwarding {} -> {} (max_connections={}, connect_timeout={}s, idle_timeout={}s, half_close_timeout={}s)",
        listen_addr,
        upstream_addr,
        args.max_connections,
        args.connect_timeout,
        args.idle_timeout,
        args.half_close_timeout
    );

    let listener = TcpListener::bind(&listen_addr).await?;
    let cfg = RelayConfig {
        connect_timeout: Duration::from_secs(args.connect_timeout),
        idle_timeout: Duration::from_secs(args.idle_timeout),
        half_close_timeout: Duration::from_secs(args.half_close_timeout),
    };

    run_server(listener, upstream_addr, cfg, args.max_connections).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn spawn_proxy(upstream_addr: String, cfg: RelayConfig, max_conns: usize) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(run_server(listener, upstream_addr, cfg, max_conns));
        addr
    }

    #[tokio::test]
    async fn forwards_bidirectional_data_cleanly() {
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = upstream.local_addr().unwrap();

        tokio::spawn(async move {
            let (mut sock, _) = upstream.accept().await.unwrap();
            let mut buf = [0u8; 64];
            let n = sock.read(&mut buf).await.unwrap();
            sock.write_all(&buf[..n]).await.unwrap();
            sock.shutdown().await.unwrap();
        });

        let proxy_addr = spawn_proxy(
            upstream_addr.to_string(),
            RelayConfig {
                connect_timeout: Duration::from_secs(2),
                idle_timeout: Duration::from_secs(5),
                half_close_timeout: Duration::from_secs(2),
            },
            16,
        )
        .await;

        let mut client = TcpStream::connect(proxy_addr).await.unwrap();
        client.write_all(b"hello proxygen").await.unwrap();
        client.shutdown().await.unwrap();

        let mut resp = Vec::new();
        client.read_to_end(&mut resp).await.unwrap();
        assert_eq!(resp, b"hello proxygen");
    }

    #[tokio::test]
    async fn reaps_half_open_zombie_connection() {
        // Upstream accepts and reads client EOF, but NEVER closes its write half (zombie)
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = upstream.local_addr().unwrap();

        let (tx, mut rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            let (mut sock, _) = upstream.accept().await.unwrap();
            let mut buf = Vec::new();
            let _ = sock.read_to_end(&mut buf).await;
            // Intentionally keep `sock` open until test finishes
            let _ = rx.try_recv();
            tokio::time::sleep(Duration::from_secs(10)).await;
            drop(sock);
        });

        let proxy_addr = spawn_proxy(
            upstream_addr.to_string(),
            RelayConfig {
                connect_timeout: Duration::from_secs(2),
                idle_timeout: Duration::from_secs(5),
                half_close_timeout: Duration::from_millis(200),
            },
            1, // max_connections = 1 proves the slot is actually released!
        )
        .await;

        let mut client = TcpStream::connect(proxy_addr).await.unwrap();
        client.write_all(b"ping").await.unwrap();
        client.shutdown().await.unwrap();

        // Proxy should close client's read side after half_close_timeout (~200ms)
        let mut buf = Vec::new();
        let res = tokio::time::timeout(Duration::from_secs(2), client.read_to_end(&mut buf)).await;
        assert!(res.is_ok(), "half-open connection was not reaped in time");
        drop(tx);
    }

    #[tokio::test]
    async fn reaps_completely_idle_connection() {
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = upstream.local_addr().unwrap();

        tokio::spawn(async move {
            let (_sock, _) = upstream.accept().await.unwrap();
            tokio::time::sleep(Duration::from_secs(10)).await;
        });

        let proxy_addr = spawn_proxy(
            upstream_addr.to_string(),
            RelayConfig {
                connect_timeout: Duration::from_secs(2),
                idle_timeout: Duration::from_millis(200),
                half_close_timeout: Duration::from_millis(100),
            },
            1,
        )
        .await;

        let mut client = TcpStream::connect(proxy_addr).await.unwrap();
        let mut buf = [0u8; 16];
        let res = tokio::time::timeout(Duration::from_secs(2), client.read(&mut buf)).await;
        assert!(res.is_ok(), "idle connection was not reaped in time");
        assert_eq!(res.unwrap().unwrap(), 0);
    }

    #[tokio::test]
    async fn survives_upstream_unavailable_without_crashing() {
        // Bind then immediately drop to get an unused port that refuses connections
        let unused = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let unused_addr = unused.local_addr().unwrap();
        drop(unused);

        let proxy_addr = spawn_proxy(
            unused_addr.to_string(),
            RelayConfig {
                connect_timeout: Duration::from_secs(1),
                idle_timeout: Duration::from_secs(5),
                half_close_timeout: Duration::from_secs(1),
            },
            2,
        )
        .await;

        // First connection fails to connect upstream; proxy must stay alive
        let mut c1 = TcpStream::connect(proxy_addr).await.unwrap();
        let mut buf = [0u8; 16];
        let _ = c1.read(&mut buf).await;

        // Second connection should still be accepted by proxy
        let c2 = TcpStream::connect(proxy_addr).await;
        assert!(c2.is_ok(), "proxy stopped accepting after upstream error");
    }

    #[tokio::test]
    async fn active_one_way_stream_not_killed_by_idle_timeout() {
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = upstream.local_addr().unwrap();

        tokio::spawn(async move {
            let (mut sock, _) = upstream.accept().await.unwrap();
            for _ in 0..5 {
                tokio::time::sleep(Duration::from_millis(80)).await;
                sock.write_all(b"chunk").await.unwrap();
            }
            sock.shutdown().await.unwrap();
        });

        let proxy_addr = spawn_proxy(
            upstream_addr.to_string(),
            RelayConfig {
                connect_timeout: Duration::from_secs(2),
                idle_timeout: Duration::from_millis(250),
                half_close_timeout: Duration::from_millis(250),
            },
            4,
        )
        .await;

        // Client sends nothing, only reads chunks totaling 400ms (> 250ms idle timeout)
        let mut client = TcpStream::connect(proxy_addr).await.unwrap();
        let mut resp = Vec::new();
        client.read_to_end(&mut resp).await.unwrap();
        assert_eq!(resp, b"chunkchunkchunkchunkchunk");
    }
}
