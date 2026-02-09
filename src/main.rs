use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::SystemTime;
use std::time::{Duration, Instant};
use std::{
    io,
    task::{Context as TaskContext, Poll},
};

use anyhow::{anyhow, Context, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};
use futures_util::{Sink, SinkExt, StreamExt};
use rustls::pki_types::ServerName;
use rustls::{version, ClientConfig, RootCertStore};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::RwLock;
use tokio::sync::Semaphore;
use tokio::time::{sleep, timeout};
use tokio_rustls::TlsConnector;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::{HeaderName, HeaderValue, StatusCode};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{accept_hdr_async, client_async, WebSocketStream};

const AUTH_HEADER_NAME: &str = "auth-secret-key";
const PROFILE_HEADER_NAME: &str = "x-traffic-profile";
const EDGE_INITIAL_SEGMENT_BYTES_DEFAULT: usize = 0;
const EDGE_INITIAL_SEGMENT_CHUNK_DEFAULT: usize = 3;
const EDGE_CONNECT_RETRIES: u32 = 3;
const AUTH_PREFACE_READ_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Parser, Debug)]
#[command(
    name = "aegis-edge-relay",
    version,
    about = "TCP-over-WebSocket relay for network resilience"
)]
struct Cli {
    #[command(subcommand)]
    mode: Mode,
}

#[derive(Subcommand, Debug)]
enum Mode {
    Bridge(BridgeArgs),
    Destination(DestinationArgs),
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum TrafficProfile {
    Chrome,
    Firefox,
}

#[derive(Args, Clone, Debug)]
struct BridgeArgs {
    #[arg(long, default_value = "127.0.0.1:7000")]
    listen: SocketAddr,
    #[arg(long, default_value = "1.1.1.1:443")]
    edge_addr: SocketAddr,
    #[arg(long)]
    host: String,
    #[arg(long)]
    sni: String,
    #[arg(long, default_value = "/relay")]
    path: String,
    #[arg(long, env = "AUTH_SECRET_KEY")]
    auth_secret_key: String,
    #[arg(long, value_enum, default_value = "chrome")]
    traffic_profile: TrafficProfile,
    #[arg(long, default_value_t = 10)]
    cycle_minutes: u64,
    #[arg(long, default_value_t = 100 * 1024 * 1024)]
    cycle_bytes: u64,
    #[arg(long, default_value_t = 250)]
    initial_chunk_bytes: usize,
    #[arg(long, default_value_t = 8)]
    initial_chunk_size: usize,
    #[arg(long, default_value_t = 2)]
    initial_chunk_delay_ms: u64,
    #[arg(long, default_value_t = EDGE_INITIAL_SEGMENT_BYTES_DEFAULT)]
    edge_initial_segment_bytes: usize,
    #[arg(long, default_value_t = EDGE_INITIAL_SEGMENT_CHUNK_DEFAULT)]
    edge_initial_segment_chunk: usize,
    #[arg(long, default_value_t = 24)]
    max_concurrent_edge_connects: usize,
    #[arg(long)]
    health_listen: Option<SocketAddr>,
}

#[derive(Args, Clone, Debug)]
struct DestinationArgs {
    #[arg(long, default_value = "0.0.0.0:8443")]
    listen: SocketAddr,
    #[arg(long, default_value = "127.0.0.1:8080")]
    forward: SocketAddr,
    #[arg(long, env = "AUTH_SECRET_KEY")]
    auth_secret_key: String,
    #[arg(long)]
    health_listen: Option<SocketAddr>,
}

#[derive(Clone, Copy, Debug)]
struct ChunkingConfig {
    initial_bytes: usize,
    chunk_size: usize,
    chunk_delay: Duration,
}

impl ChunkingConfig {
    fn disabled() -> Self {
        Self {
            initial_bytes: 0,
            chunk_size: 0,
            chunk_delay: Duration::ZERO,
        }
    }
}

struct BridgeState {
    args: BridgeArgs,
    tls_connector: TlsConnector,
    rotation: Arc<RotationManager>,
    edge_connect_slots: Arc<Semaphore>,
    edge_candidates: Arc<Vec<SocketAddr>>,
}

struct RotationManager {
    cycle_after: Duration,
    cycle_bytes: u64,
    next_generation_id: AtomicU64,
    current: RwLock<Arc<GenerationState>>,
}

struct GenerationState {
    id: u64,
    started_at: Instant,
    bytes: AtomicU64,
    active_streams: AtomicU64,
    draining: AtomicBool,
}

struct GenerationLease {
    generation: Arc<GenerationState>,
}

struct AcquireOutcome {
    lease: GenerationLease,
    rotated_to: Option<u64>,
}

#[derive(Debug, Clone, Copy, Default)]
struct RelayStats {
    up_bytes: u64,
    down_bytes: u64,
}

struct InitialChunkedTcpStream {
    inner: TcpStream,
    segmented_limit: usize,
    segmented_chunk_size: usize,
    bytes_written: usize,
}

impl InitialChunkedTcpStream {
    fn new(inner: TcpStream, segmented_limit: usize, segmented_chunk_size: usize) -> Self {
        Self {
            inner,
            segmented_limit,
            segmented_chunk_size: segmented_chunk_size.max(1),
            bytes_written: 0,
        }
    }
}

impl AsyncRead for InitialChunkedTcpStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for InitialChunkedTcpStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }

        let mut write_len = buf.len();
        if self.bytes_written < self.segmented_limit {
            let remaining = self.segmented_limit - self.bytes_written;
            write_len = write_len
                .min(self.segmented_chunk_size)
                .min(remaining.max(1));
        }

        match Pin::new(&mut self.inner).poll_write(cx, &buf[..write_len]) {
            Poll::Ready(Ok(written)) => {
                self.bytes_written = self.bytes_written.saturating_add(written);
                Poll::Ready(Ok(written))
            }
            other => other,
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }
}

impl GenerationState {
    fn new(id: u64) -> Self {
        Self {
            id,
            started_at: Instant::now(),
            bytes: AtomicU64::new(0),
            active_streams: AtomicU64::new(0),
            draining: AtomicBool::new(false),
        }
    }

    fn add_bytes(&self, count: u64) {
        self.bytes.fetch_add(count, Ordering::Relaxed);
    }
}

impl Drop for GenerationLease {
    fn drop(&mut self) {
        let remaining = self
            .generation
            .active_streams
            .fetch_sub(1, Ordering::Relaxed)
            .saturating_sub(1);
        if self.generation.draining.load(Ordering::Relaxed) && remaining == 0 {
            println!("bridge generation {} drained", self.generation.id);
        }
    }
}

impl RotationManager {
    fn new(cycle_after: Duration, cycle_bytes: u64) -> Self {
        Self {
            cycle_after,
            cycle_bytes: cycle_bytes.max(1),
            next_generation_id: AtomicU64::new(1),
            current: RwLock::new(Arc::new(GenerationState::new(1))),
        }
    }

    async fn acquire(&self) -> AcquireOutcome {
        let mut rotated_to = None;
        let generation = {
            let mut guard = self.current.write().await;
            if self.should_rotate(&guard) {
                let old_generation = Arc::clone(&guard);
                old_generation.draining.store(true, Ordering::Relaxed);

                let new_id = self
                    .next_generation_id
                    .fetch_add(1, Ordering::Relaxed)
                    .saturating_add(1);
                let replacement = Arc::new(GenerationState::new(new_id));
                *guard = Arc::clone(&replacement);
                rotated_to = Some(new_id);
                println!(
                    "bridge rotation: generation {} -> {}",
                    old_generation.id, new_id
                );
            }

            let active_generation = Arc::clone(&guard);
            active_generation
                .active_streams
                .fetch_add(1, Ordering::Relaxed);
            active_generation
        };

        AcquireOutcome {
            lease: GenerationLease { generation },
            rotated_to,
        }
    }

    fn should_rotate(&self, generation: &GenerationState) -> bool {
        generation.started_at.elapsed() >= self.cycle_after
            || generation.bytes.load(Ordering::Relaxed) >= self.cycle_bytes
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.mode {
        Mode::Bridge(args) => run_bridge_mode(args).await,
        Mode::Destination(args) => run_destination_mode(args).await,
    }
}

async fn run_bridge_mode(args: BridgeArgs) -> Result<()> {
    let cycle_minutes = args.cycle_minutes.clamp(10, 15);
    let cycle_after = Duration::from_secs(cycle_minutes * 60);
    let edge_candidates = Arc::new(resolve_edge_candidates(&args).await);
    let listener = TcpListener::bind(args.listen)
        .await
        .with_context(|| format!("failed to bind bridge listener on {}", args.listen))?;
    let state = Arc::new(BridgeState {
        tls_connector: build_tls_connector(args.traffic_profile)?,
        rotation: Arc::new(RotationManager::new(cycle_after, args.cycle_bytes)),
        edge_connect_slots: Arc::new(Semaphore::new(args.max_concurrent_edge_connects.max(1))),
        edge_candidates,
        args,
    });

    if let Some(health_addr) = state.args.health_listen {
        start_health_server("bridge", health_addr).await?;
    }

    println!(
        "Bridge mode ready on {} -> edge {} with host {} (rotation {} min / {} bytes, edge-segment {}/{}B, max-edge-connects {}, edge-candidates={})",
        state.args.listen,
        state.args.edge_addr,
        state.args.host,
        cycle_minutes,
        state.args.cycle_bytes,
        state.args.edge_initial_segment_bytes,
        state.args.edge_initial_segment_chunk,
        state.args.max_concurrent_edge_connects,
        state.edge_candidates.len()
    );

    loop {
        let (local_stream, peer) = listener.accept().await.context("bridge accept failed")?;
        local_stream
            .set_nodelay(true)
            .context("failed to set TCP_NODELAY on bridge client socket")?;

        let state = Arc::clone(&state);
        tokio::spawn(async move {
            let acquisition = state.rotation.acquire().await;
            if let Some(new_generation) = acquisition.rotated_to {
                let warmup_state = Arc::clone(&state);
                tokio::spawn(async move {
                    if let Err(err) = warmup_generation(warmup_state, new_generation).await {
                        eprintln!(
                            "bridge generation {} warmup failed: {err:#}",
                            new_generation
                        );
                    }
                });
            }

            if let Err(err) =
                handle_bridge_client(local_stream, state, acquisition.lease, peer).await
            {
                eprintln!("bridge session from {} closed with error: {err:#}", peer);
            }
        });
    }
}

async fn run_destination_mode(args: DestinationArgs) -> Result<()> {
    let listener = TcpListener::bind(args.listen)
        .await
        .with_context(|| format!("failed to bind destination listener on {}", args.listen))?;

    if let Some(health_addr) = args.health_listen {
        start_health_server("destination", health_addr).await?;
    }

    println!(
        "Destination mode ready on {} -> forward {}",
        args.listen, args.forward
    );

    loop {
        let (socket, peer) = listener
            .accept()
            .await
            .context("destination accept failed")?;
        socket
            .set_nodelay(true)
            .context("failed to set TCP_NODELAY on destination socket")?;

        let args = args.clone();
        tokio::spawn(async move {
            if let Err(err) = handle_destination_client(socket, args, peer).await {
                eprintln!(
                    "destination session from {} closed with error: {err:#}",
                    peer
                );
            }
        });
    }
}

async fn handle_bridge_client(
    local_stream: TcpStream,
    state: Arc<BridgeState>,
    lease: GenerationLease,
    peer: SocketAddr,
) -> Result<()> {
    let generation_id = lease.generation.id;
    let generation = Arc::clone(&lease.generation);
    println!(
        "bridge session from {} assigned to generation {}",
        peer, generation_id
    );

    let ws_stream = connect_bridge_websocket_with_retry(&state, EDGE_CONNECT_RETRIES)
        .await
        .with_context(|| format!("generation {} failed to connect to edge", generation_id))?;
    println!(
        "bridge session from {} connected to edge using generation {}",
        peer, generation_id
    );

    let stats = relay_tcp_over_ws(
        local_stream,
        ws_stream,
        Some(generation),
        state.chunking_config(),
    )
    .await?;
    println!(
        "bridge session from {} finished: up={}B down={}B generation={}",
        peer, stats.up_bytes, stats.down_bytes, generation_id
    );
    Ok(())
}

impl BridgeState {
    fn chunking_config(&self) -> ChunkingConfig {
        ChunkingConfig {
            initial_bytes: self.args.initial_chunk_bytes,
            chunk_size: self.args.initial_chunk_size.max(1),
            chunk_delay: Duration::from_millis(self.args.initial_chunk_delay_ms),
        }
    }
}

async fn warmup_generation(state: Arc<BridgeState>, generation_id: u64) -> Result<()> {
    let mut ws = connect_bridge_websocket_with_retry(&state, EDGE_CONNECT_RETRIES)
        .await
        .with_context(|| format!("generation {} warmup connect failed", generation_id))?;
    ws.close(None)
        .await
        .context("generation warmup websocket close failed")?;
    println!("bridge generation {} warmed", generation_id);
    Ok(())
}

async fn connect_bridge_websocket(
    state: &BridgeState,
    edge_addr: SocketAddr,
) -> Result<WebSocketStream<tokio_rustls::client::TlsStream<InitialChunkedTcpStream>>> {
    let edge_stream = timeout(Duration::from_secs(3), TcpStream::connect(edge_addr))
        .await
        .with_context(|| format!("timed out connecting edge {}", edge_addr))?
        .with_context(|| format!("failed to connect edge {}", edge_addr))?;
    edge_stream
        .set_nodelay(true)
        .context("failed to set TCP_NODELAY on edge stream")?;

    let server_name =
        ServerName::try_from(state.args.sni.clone()).context("invalid SNI server name")?;
    let edge_stream = InitialChunkedTcpStream::new(
        edge_stream,
        state.args.edge_initial_segment_bytes,
        state.args.edge_initial_segment_chunk,
    );
    let tls_stream = timeout(
        Duration::from_secs(8),
        state.tls_connector.connect(server_name, edge_stream),
    )
    .await
    .context("TLS handshake to edge timed out")?
    .context("TLS handshake to edge failed")?;

    let normalized_path = if state.args.path.starts_with('/') {
        state.args.path.clone()
    } else {
        format!("/{}", state.args.path)
    };
    let ws_url = format!("wss://{}{}", state.args.host, normalized_path);
    let mut request = ws_url
        .into_client_request()
        .context("failed to build websocket request")?;

    let host_header = HeaderValue::from_str(&state.args.host).context("invalid host header")?;
    request
        .headers_mut()
        .insert(HeaderName::from_static("host"), host_header);
    request.headers_mut().insert(
        HeaderName::from_static(AUTH_HEADER_NAME),
        HeaderValue::from_str(&state.args.auth_secret_key).context("invalid auth header value")?,
    );
    request.headers_mut().insert(
        HeaderName::from_static(PROFILE_HEADER_NAME),
        HeaderValue::from_static(match state.args.traffic_profile {
            TrafficProfile::Chrome => "chrome",
            TrafficProfile::Firefox => "firefox",
        }),
    );

    let (ws_stream, _) = timeout(Duration::from_secs(15), client_async(request, tls_stream))
        .await
        .context("websocket upgrade to edge timed out")?
        .context("websocket upgrade to edge failed")?;
    Ok(ws_stream)
}

async fn connect_bridge_websocket_with_retry(
    state: &BridgeState,
    attempts: u32,
) -> Result<WebSocketStream<tokio_rustls::client::TlsStream<InitialChunkedTcpStream>>> {
    let _edge_slot = state
        .edge_connect_slots
        .clone()
        .acquire_owned()
        .await
        .context("failed to acquire edge connect slot")?;
    let max_attempts = attempts.max(1);
    let mut last_error = None;

    let primary_edge = *state
        .edge_candidates
        .first()
        .expect("bridge must have at least one edge candidate");

    for attempt in 1..=max_attempts {
        let edge_addr = primary_edge;
        match connect_bridge_websocket(state, edge_addr).await {
            Ok(stream) => return Ok(stream),
            Err(err) => {
                eprintln!(
                    "bridge edge connect attempt {}/{} via {} failed: {err:#}",
                    attempt, max_attempts, edge_addr
                );
                last_error = Some(err);
                if attempt < max_attempts {
                    sleep(Duration::from_millis(150 * u64::from(attempt))).await;
                }
            }
        }
    }

    Err(last_error.expect("at least one connection attempt should have failed"))
}

async fn handle_destination_client(
    socket: TcpStream,
    args: DestinationArgs,
    peer: SocketAddr,
) -> Result<()> {
    let mut probe = [0u8; 3];
    let peeked = socket
        .peek(&mut probe)
        .await
        .context("failed to probe destination socket")?;
    if peeked >= 3 && &probe == b"GET" {
        println!("destination session from {} detected websocket mode", peer);
        handle_destination_websocket(socket, args, peer).await
    } else {
        println!("destination session from {} detected raw mode", peer);
        handle_destination_raw(socket, args, peer).await
    }
}

async fn handle_destination_websocket(
    socket: TcpStream,
    args: DestinationArgs,
    peer: SocketAddr,
) -> Result<()> {
    let shared_secret = args.auth_secret_key.clone();
    let ws_stream = accept_hdr_async(socket, move |req: &Request, response: Response| {
        let incoming = req
            .headers()
            .get(AUTH_HEADER_NAME)
            .and_then(|value| value.to_str().ok());
        if incoming == Some(shared_secret.as_str()) {
            Ok(response)
        } else {
            Err(unauthorized_response())
        }
    })
    .await
    .context("destination websocket handshake failed")?;
    println!("destination websocket session from {} authenticated", peer);

    let target_stream = TcpStream::connect(args.forward)
        .await
        .with_context(|| format!("failed to connect forward target {}", args.forward))?;
    target_stream
        .set_nodelay(true)
        .context("failed to set TCP_NODELAY on forward stream")?;
    println!(
        "destination websocket session from {} connected forward target {}",
        peer, args.forward
    );

    let stats =
        relay_tcp_over_ws(target_stream, ws_stream, None, ChunkingConfig::disabled()).await?;
    println!(
        "destination websocket session from {} finished: up={}B down={}B",
        peer, stats.up_bytes, stats.down_bytes
    );
    Ok(())
}

async fn handle_destination_raw(
    mut inbound: TcpStream,
    args: DestinationArgs,
    peer: SocketAddr,
) -> Result<()> {
    let auth_line = read_auth_line(&mut inbound, 512).await?;
    let expected = format!("AUTH {}", args.auth_secret_key);
    if auth_line.trim_end_matches(['\r', '\n']) != expected {
        eprintln!(
            "destination raw session from {} failed authentication",
            peer
        );
        return Err(anyhow!("unauthorized raw destination connection"));
    }
    println!("destination raw session from {} authenticated", peer);

    let outbound = TcpStream::connect(args.forward)
        .await
        .with_context(|| format!("failed to connect forward target {}", args.forward))?;
    outbound
        .set_nodelay(true)
        .context("failed to set TCP_NODELAY on forward stream")?;
    println!(
        "destination raw session from {} connected forward target {}",
        peer, args.forward
    );

    let stats = relay_tcp_over_tcp(inbound, outbound).await?;
    println!(
        "destination raw session from {} finished: up={}B down={}B",
        peer, stats.up_bytes, stats.down_bytes
    );
    Ok(())
}

async fn read_auth_line(stream: &mut TcpStream, max_len: usize) -> Result<String> {
    let mut data = Vec::with_capacity(64);
    loop {
        if data.len() >= max_len {
            return Err(anyhow!("authentication preface is too long"));
        }
        let mut byte = [0u8; 1];
        let read_len = timeout(AUTH_PREFACE_READ_TIMEOUT, stream.read(&mut byte))
            .await
            .context("authentication preface read timed out")?
            .context("failed to read authentication preface")?;
        if read_len == 0 {
            return Err(anyhow!("connection closed before authentication preface"));
        }
        data.push(byte[0]);
        if byte[0] == b'\n' {
            break;
        }
    }
    String::from_utf8(data).context("authentication preface is not valid UTF-8")
}

fn unauthorized_response() -> ErrorResponse {
    tokio_tungstenite::tungstenite::http::Response::builder()
        .status(StatusCode::UNAUTHORIZED)
        .header("content-type", "text/plain; charset=utf-8")
        .body(Some(String::from("unauthorized")))
        .expect("failed to build static unauthorized response")
}

async fn relay_tcp_over_tcp(left: TcpStream, right: TcpStream) -> Result<RelayStats> {
    let (mut left_reader, mut left_writer) = left.into_split();
    let (mut right_reader, mut right_writer) = right.into_split();

    let left_to_right = tokio::spawn(async move {
        let copied = tokio::io::copy(&mut left_reader, &mut right_writer)
            .await
            .context("raw relay left->right failed")?;
        right_writer
            .shutdown()
            .await
            .context("raw relay failed to shutdown right writer")?;
        Ok::<u64, anyhow::Error>(copied)
    });

    let right_to_left = tokio::spawn(async move {
        let copied = tokio::io::copy(&mut right_reader, &mut left_writer)
            .await
            .context("raw relay right->left failed")?;
        left_writer
            .shutdown()
            .await
            .context("raw relay failed to shutdown left writer")?;
        Ok::<u64, anyhow::Error>(copied)
    });

    let (left_res, right_res) = tokio::join!(left_to_right, right_to_left);
    let up_bytes = left_res.context("raw relay left->right join failure")??;
    let down_bytes = right_res.context("raw relay right->left join failure")??;

    Ok(RelayStats {
        up_bytes,
        down_bytes,
    })
}

async fn relay_tcp_over_ws<S>(
    tcp_stream: TcpStream,
    ws_stream: WebSocketStream<S>,
    generation: Option<Arc<GenerationState>>,
    chunking: ChunkingConfig,
) -> Result<RelayStats>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut tcp_reader, mut tcp_writer) = tcp_stream.into_split();
    let (mut ws_writer, mut ws_reader) = ws_stream.split();

    let generation_for_up = generation.clone();
    let generation_for_down = generation;

    let tcp_to_ws = tokio::spawn(async move {
        let mut buffer = [0u8; 16 * 1024];
        let mut initial_remaining = chunking.initial_bytes;
        let mut transferred = 0u64;
        loop {
            let read_len = tcp_reader.read(&mut buffer).await?;
            if read_len == 0 {
                let _ = ws_writer.send(Message::Close(None)).await;
                break;
            }
            transferred = transferred.saturating_add(read_len as u64);

            if let Some(state) = &generation_for_up {
                state.add_bytes(read_len as u64);
            }

            send_binary_with_chunking(
                &mut ws_writer,
                &buffer[..read_len],
                &mut initial_remaining,
                chunking,
            )
            .await?;
        }
        Ok::<u64, anyhow::Error>(transferred)
    });

    let ws_to_tcp = tokio::spawn(async move {
        let mut initial_remaining = chunking.initial_bytes;
        let mut transferred = 0u64;
        while let Some(frame) = ws_reader.next().await {
            match frame? {
                Message::Binary(payload) => {
                    transferred = transferred.saturating_add(payload.len() as u64);
                    if let Some(state) = &generation_for_down {
                        state.add_bytes(payload.len() as u64);
                    }
                    write_with_chunking(
                        &mut tcp_writer,
                        &payload,
                        &mut initial_remaining,
                        chunking,
                    )
                    .await?;
                }
                Message::Close(_) => break,
                Message::Ping(_) | Message::Pong(_) | Message::Text(_) => {}
                _ => {}
            }
        }
        Ok::<u64, anyhow::Error>(transferred)
    });

    let (up_res, down_res) = tokio::join!(tcp_to_ws, ws_to_tcp);
    let up_bytes = up_res.context("tcp->ws task join failure")??;
    let down_bytes = down_res.context("ws->tcp task join failure")??;

    Ok(RelayStats {
        up_bytes,
        down_bytes,
    })
}

async fn resolve_edge_candidates(args: &BridgeArgs) -> Vec<SocketAddr> {
    vec![args.edge_addr]
}

async fn send_binary_with_chunking<W>(
    ws_writer: &mut W,
    data: &[u8],
    initial_remaining: &mut usize,
    chunking: ChunkingConfig,
) -> Result<()>
where
    W: Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    if data.is_empty() {
        return Ok(());
    }

    let segmented_len = data.len().min(*initial_remaining);
    if segmented_len > 0 {
        for piece in data[..segmented_len].chunks(chunking.chunk_size) {
            ws_writer
                .send(Message::Binary(piece.to_vec()))
                .await
                .context("failed to write segmented websocket frame")?;
            if !chunking.chunk_delay.is_zero() {
                sleep(chunking.chunk_delay).await;
            }
        }
        *initial_remaining -= segmented_len;
    }

    if segmented_len < data.len() {
        ws_writer
            .send(Message::Binary(data[segmented_len..].to_vec()))
            .await
            .context("failed to write websocket frame")?;
    }

    Ok(())
}

async fn write_with_chunking<W>(
    writer: &mut W,
    data: &[u8],
    initial_remaining: &mut usize,
    chunking: ChunkingConfig,
) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    if data.is_empty() {
        return Ok(());
    }

    let segmented_len = data.len().min(*initial_remaining);
    if segmented_len > 0 {
        for piece in data[..segmented_len].chunks(chunking.chunk_size) {
            writer
                .write_all(piece)
                .await
                .context("failed to write segmented TCP payload")?;
            writer
                .flush()
                .await
                .context("failed to flush segmented TCP payload")?;
            if !chunking.chunk_delay.is_zero() {
                sleep(chunking.chunk_delay).await;
            }
        }
        *initial_remaining -= segmented_len;
    }

    if segmented_len < data.len() {
        writer
            .write_all(&data[segmented_len..])
            .await
            .context("failed to write TCP payload")?;
    }

    Ok(())
}

async fn start_health_server(mode: &'static str, listen: SocketAddr) -> Result<()> {
    let listener = TcpListener::bind(listen)
        .await
        .with_context(|| format!("failed to bind {mode} health listener on {listen}"))?;
    println!("{mode} health endpoint ready on http://{listen}/healthz");

    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((mut socket, peer)) => {
                    tokio::spawn(async move {
                        if let Err(err) = handle_health_connection(&mut socket, mode).await {
                            eprintln!("{mode} health request from {} failed: {err:#}", peer);
                        }
                    });
                }
                Err(err) => {
                    eprintln!("{mode} health accept failed: {err:#}");
                }
            }
        }
    });

    Ok(())
}

async fn handle_health_connection(socket: &mut TcpStream, mode: &str) -> Result<()> {
    let mut buffer = [0u8; 1024];
    let read_len = timeout(Duration::from_secs(3), socket.read(&mut buffer))
        .await
        .context("health request timeout")?
        .context("failed to read health request")?;
    if read_len == 0 {
        return Ok(());
    }

    let request = String::from_utf8_lossy(&buffer[..read_len]);
    let request_line = request.lines().next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let path = parts.next().unwrap_or("/");

    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let (status, body) = match (method, path) {
        ("GET", "/healthz") | ("GET", "/readyz") => (
            StatusCode::OK,
            format!(
                "{{\"status\":\"ok\",\"mode\":\"{}\",\"unix_time\":{}}}",
                mode, now
            ),
        ),
        ("GET", "/") => (
            StatusCode::OK,
            format!(
                "{{\"service\":\"aegis-edge-relay\",\"mode\":\"{}\",\"health\":\"/healthz\"}}",
                mode
            ),
        ),
        ("GET", _) => (
            StatusCode::NOT_FOUND,
            String::from("{\"error\":\"not found\"}"),
        ),
        _ => (
            StatusCode::METHOD_NOT_ALLOWED,
            String::from("{\"error\":\"method not allowed\"}"),
        ),
    };

    let status_code = status.as_u16();
    let reason = status.canonical_reason().unwrap_or("Unknown");
    let response = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        status_code,
        reason,
        body.len(),
        body
    );
    socket
        .write_all(response.as_bytes())
        .await
        .context("failed to write health response")?;
    socket
        .shutdown()
        .await
        .context("failed to shutdown health response socket")?;
    Ok(())
}

fn build_tls_connector(profile: TrafficProfile) -> Result<TlsConnector> {
    let _h2_marker = h2::Reason::NO_ERROR;
    let mut roots = RootCertStore::empty();

    let native_certs = rustls_native_certs::load_native_certs();
    for cert in native_certs.certs {
        let _ = roots.add(cert);
    }
    if !native_certs.errors.is_empty() {
        eprintln!(
            "native certificate loading reported {} issues; continuing with available roots",
            native_certs.errors.len()
        );
    }

    if roots.is_empty() {
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    }

    let mut config = ClientConfig::builder_with_protocol_versions(&[&version::TLS13])
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.enable_sni = true;
    // WebSocket upgrade here is HTTP/1.1-based; advertising h2 can make
    // Cloudflare select HTTP/2, which breaks tungstenite's HTTP/1.1 parser.
    let _profile_marker = profile;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];

    Ok(TlsConnector::from(Arc::new(config)))
}
