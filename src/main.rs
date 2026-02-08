use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};
use futures_util::{SinkExt, StreamExt};
use rustls::pki_types::ServerName;
use rustls::{version, ClientConfig, RootCertStore};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsConnector;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::{HeaderName, HeaderValue, StatusCode};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{accept_hdr_async, client_async, WebSocketStream};

const AUTH_HEADER_NAME: &str = "auth-secret-key";
const PROFILE_HEADER_NAME: &str = "x-traffic-profile";

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
}

#[derive(Args, Clone, Debug)]
struct DestinationArgs {
    #[arg(long, default_value = "0.0.0.0:8443")]
    listen: SocketAddr,
    #[arg(long, default_value = "127.0.0.1:8080")]
    forward: SocketAddr,
    #[arg(long, env = "AUTH_SECRET_KEY")]
    auth_secret_key: String,
}

struct BridgeState {
    args: BridgeArgs,
    tls_connector: TlsConnector,
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
    let listener = TcpListener::bind(args.listen)
        .await
        .with_context(|| format!("failed to bind bridge listener on {}", args.listen))?;
    let state = Arc::new(BridgeState {
        tls_connector: build_tls_connector(args.traffic_profile)?,
        args,
    });

    println!(
        "Bridge mode ready on {} -> edge {} with host {} (cycle {} min / {} bytes)",
        state.args.listen,
        state.args.edge_addr,
        state.args.host,
        cycle_minutes,
        state.args.cycle_bytes
    );

    loop {
        let (local_stream, peer) = listener.accept().await.context("bridge accept failed")?;
        local_stream
            .set_nodelay(true)
            .context("failed to set TCP_NODELAY on bridge client socket")?;

        let state = Arc::clone(&state);
        tokio::spawn(async move {
            if let Err(err) = handle_bridge_client(local_stream, state).await {
                eprintln!("bridge session from {} closed with error: {err:#}", peer);
            }
        });
    }
}

async fn run_destination_mode(args: DestinationArgs) -> Result<()> {
    let listener = TcpListener::bind(args.listen)
        .await
        .with_context(|| format!("failed to bind destination listener on {}", args.listen))?;

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
            if let Err(err) = handle_destination_client(socket, args).await {
                eprintln!("destination session from {} closed with error: {err:#}", peer);
            }
        });
    }
}

async fn handle_bridge_client(local_stream: TcpStream, state: Arc<BridgeState>) -> Result<()> {
    let ws_stream = connect_bridge_websocket(&state).await?;
    let cycle_after = Duration::from_secs(state.args.cycle_minutes.clamp(10, 15) * 60);

    relay_tcp_over_ws(
        local_stream,
        ws_stream,
        Some(cycle_after),
        Some(state.args.cycle_bytes),
    )
    .await
}

async fn connect_bridge_websocket(
    state: &BridgeState,
) -> Result<WebSocketStream<tokio_rustls::client::TlsStream<TcpStream>>> {
    let edge_stream = TcpStream::connect(state.args.edge_addr)
        .await
        .with_context(|| format!("failed to connect edge {}", state.args.edge_addr))?;
    edge_stream
        .set_nodelay(true)
        .context("failed to set TCP_NODELAY on edge stream")?;

    let server_name =
        ServerName::try_from(state.args.sni.clone()).context("invalid SNI server name")?;
    let tls_stream = state
        .tls_connector
        .connect(server_name, edge_stream)
        .await
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

    let (ws_stream, _) = client_async(request, tls_stream)
        .await
        .context("websocket upgrade to edge failed")?;
    Ok(ws_stream)
}

async fn handle_destination_client(socket: TcpStream, args: DestinationArgs) -> Result<()> {
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

    let target_stream = TcpStream::connect(args.forward)
        .await
        .with_context(|| format!("failed to connect forward target {}", args.forward))?;
    target_stream
        .set_nodelay(true)
        .context("failed to set TCP_NODELAY on forward stream")?;

    relay_tcp_over_ws(target_stream, ws_stream, None, None).await
}

fn unauthorized_response() -> ErrorResponse {
    tokio_tungstenite::tungstenite::http::Response::builder()
        .status(StatusCode::UNAUTHORIZED)
        .header("content-type", "text/plain; charset=utf-8")
        .body(Some(String::from("unauthorized")))
        .expect("failed to build static unauthorized response")
}

async fn relay_tcp_over_ws<S>(
    tcp_stream: TcpStream,
    ws_stream: WebSocketStream<S>,
    cycle_after: Option<Duration>,
    cycle_bytes: Option<u64>,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let byte_counter = Arc::new(AtomicU64::new(0));
    let byte_limit = cycle_bytes.unwrap_or(u64::MAX);

    let (mut tcp_reader, mut tcp_writer) = tcp_stream.into_split();
    let (mut ws_writer, mut ws_reader) = ws_stream.split();

    let up_counter = Arc::clone(&byte_counter);
    let mut tcp_to_ws = tokio::spawn(async move {
        let mut buffer = [0u8; 16 * 1024];
        loop {
            let read_len = tcp_reader.read(&mut buffer).await?;
            if read_len == 0 {
                let _ = ws_writer.send(Message::Close(None)).await;
                break;
            }

            let total = up_counter.fetch_add(read_len as u64, Ordering::Relaxed) + read_len as u64;
            ws_writer
                .send(Message::Binary(buffer[..read_len].to_vec().into()))
                .await?;
            if total >= byte_limit {
                let _ = ws_writer.send(Message::Close(None)).await;
                break;
            }
        }
        Ok::<(), anyhow::Error>(())
    });

    let down_counter = Arc::clone(&byte_counter);
    let mut ws_to_tcp = tokio::spawn(async move {
        while let Some(frame) = ws_reader.next().await {
            match frame? {
                Message::Binary(payload) => {
                    let total = down_counter.fetch_add(payload.len() as u64, Ordering::Relaxed)
                        + payload.len() as u64;
                    tcp_writer.write_all(&payload).await?;
                    if total >= byte_limit {
                        break;
                    }
                }
                Message::Close(_) => break,
                Message::Ping(_) | Message::Pong(_) | Message::Text(_) => {}
                _ => {}
            }
        }
        Ok::<(), anyhow::Error>(())
    });

    match cycle_after {
        Some(window) => {
            let timer = tokio::time::sleep(window);
            tokio::pin!(timer);

            tokio::select! {
                _ = &mut timer => {
                    tcp_to_ws.abort();
                    ws_to_tcp.abort();
                }
                first = &mut tcp_to_ws => {
                    ws_to_tcp.abort();
                    first.context("tcp->ws task join failure")??;
                }
                second = &mut ws_to_tcp => {
                    tcp_to_ws.abort();
                    second.context("ws->tcp task join failure")??;
                }
            }
        }
        None => {
            tokio::select! {
                first = &mut tcp_to_ws => {
                    ws_to_tcp.abort();
                    first.context("tcp->ws task join failure")??;
                }
                second = &mut ws_to_tcp => {
                    tcp_to_ws.abort();
                    second.context("ws->tcp task join failure")??;
                }
            }
        }
    }

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

    config.alpn_protocols = match profile {
        TrafficProfile::Chrome => vec![b"h2".to_vec(), b"http/1.1".to_vec()],
        TrafficProfile::Firefox => vec![b"http/1.1".to_vec(), b"h2".to_vec()],
    };

    Ok(TlsConnector::from(Arc::new(config)))
}
