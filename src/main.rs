use anyhow::{anyhow, Context, Result};
use bytes::{Bytes, BytesMut};
use clap::Parser;
use futures_util::{
    stream::{SplitSink, SplitStream},
    SinkExt, StreamExt,
};
use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, BufWriter},
    net::{TcpListener, TcpStream, UdpSocket},
    sync::RwLock,
    time::timeout,
};
use tokio_tungstenite::{
    connect_async, tungstenite::protocol::Message, accept_async,
    WebSocketStream,
};
use tracing::{error, info, warn, Level};
use trust_dns_resolver::{config::*, TokioAsyncResolver};
use url::Url;
use uuid::Uuid;

// --- Configuration Constants ---

const TCP_BUFFER_SIZE: usize = 65536; // 64KB buffer for better throughput
const UDP_BUFFER_SIZE: usize = 65535; // Max UDP packet size
const WS_PING_INTERVAL: Duration = Duration::from_secs(30);
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(10);
const STALE_CONNECTION_TIMEOUT: Duration = Duration::from_secs(120);
const CLEANUP_INTERVAL: Duration = Duration::from_secs(60);

// --- Command Line Argument Parsing ---

#[derive(Parser, Debug)]
#[command(author, version, about = "TCP/UDP over WebSocket Tunnel (Rust Edition)", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(clap::Subcommand, Debug)]
enum Commands {
    /// Run in client mode
    /// e.g., client ws://your-server.com:8080 127.0.0.1:3389
    Client {
        /// WebSocket server URL (e.g., ws://example.com:8080 or wss://...)
        server_url: String,
        /// Local address to listen on (e.g., 127.0.0.1:3389)
        local_addr: String,
    },
    /// Run in server mode
    /// e.g., server 0.0.0.0:8080 192.168.1.10:3389
    Server {
        /// Address to listen for WebSocket connections (e.g., 0.0.0.0:8080)
        listen_addr: String,
        /// Target address to forward traffic to (e.g., 127.0.0.1:3389)
        target_addr: String,
    },
}

// --- Shared State for the Server ---

// Holds information about an active tunnel on the server
struct TunnelInfo {
    last_activity: Instant,
    // we don't need to store the connection handles here, they live in their own tasks.
    // this struct is for metadata and session management.
}

// A map of all active tunnels, shared safely across tasks
type TunnelMap = Arc<RwLock<HashMap<String, TunnelInfo>>>;

// --- Main Application Logic ---

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_max_level(Level::INFO).init();
    let cli = Cli::parse();

    match cli.command {
        Commands::Client { server_url, local_addr } => {
            run_client(server_url, local_addr).await
        }
        Commands::Server { listen_addr, target_addr } => {
            run_server(listen_addr, target_addr).await
        }
    }
}

// --- Server Implementation ---

async fn run_server(listen_addr: String, target_addr: String) -> Result<()> {
    info!("Starting server mode...");
    info!("Listening for WebSocket connections on: {}", listen_addr);
    info!("Forwarding traffic to: {}", target_addr);

    let listener = TcpListener::bind(&listen_addr).await.context("Failed to bind server listener")?;
    let tunnel_map = TunnelMap::new(RwLock::new(HashMap::new()));
    let target_addr = Arc::new(target_addr);
    
    // Spawn a task to periodically clean up stale connections
    let tunnel_map_clone = tunnel_map.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(CLEANUP_INTERVAL).await;
            let mut tunnels = tunnel_map_clone.write().await;
            let now = Instant::now();
            tunnels.retain(|uuid, info| {
                let stale = now.duration_since(info.last_activity) > STALE_CONNECTION_TIMEOUT;
                if stale {
                    warn!(uuid, "Pruning stale connection.");
                }
                !stale
            });
        }
    });

    loop {
        let (socket, addr) = listener.accept().await?;
        info!(peer = %addr, "Accepted new incoming connection");
        
        let tunnel_map_ref = tunnel_map.clone();
        let target_addr_ref = target_addr.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_server_connection(socket, addr, tunnel_map_ref, target_addr_ref).await {
                warn!(peer = %addr, "Connection handler error: {:?}", e);
            }
        });
    }
}

async fn handle_server_connection(
    stream: TcpStream,
    addr: SocketAddr,
    tunnels: TunnelMap,
    target_addr: Arc<String>,
) -> Result<()> {
    info!(
        "New WebSocket handshake from {}",
        addr
    );

    let ws_stream = accept_async(stream).await.context("WebSocket handshake failed")?;
    let (mut ws_sender, mut ws_receiver) = ws_stream.split();

    // The first message from the client MUST be the UUID
    let uuid = match timeout(CONNECTION_TIMEOUT, ws_receiver.next()).await {
        Ok(Some(Ok(Message::Text(uuid)))) => uuid.to_string(),
        _ => {
            ws_sender.send(Message::Close(None)).await.ok();
            return Err(anyhow!("Client failed to send UUID on time."));
        }
    };
    info!(uuid, peer = %addr, "Client connected and provided UUID");

    let is_udp = uuid.starts_with('U');
    
    { // Scope for the write lock
        let mut tunnel_map = tunnels.write().await;
        tunnel_map.insert(uuid.clone(), TunnelInfo { last_activity: Instant::now() });
    }

    if is_udp {
        // Handle UDP
        let local_udp_socket = UdpSocket::bind("0.0.0.0:0").await.context("Failed to bind local UDP socket")?;
        local_udp_socket.connect(&*target_addr).await.context("Failed to connect to target UDP address")?;
        info!(uuid, "UDP tunnel established for target {}", target_addr);
        proxy_udp(uuid.clone(), tunnels, ws_sender, ws_receiver, local_udp_socket).await?;
    } else {
        // Handle TCP
        let target_stream = TcpStream::connect(&*target_addr).await.context("Failed to connect to target TCP address")?;
        info!(uuid, "TCP tunnel established for target {}", target_addr);
        proxy_tcp(uuid.clone(), tunnels, ws_sender, ws_receiver, target_stream).await?;
    }

    Ok(())
}


// --- Client Implementation ---

async fn run_client(server_url: String, local_addr: String) -> Result<()> {
    info!("Starting client mode...");
    info!("Connecting to WebSocket server: {}", server_url);
    info!("Listening for local connections on: {}", local_addr);
    
    // Perform DNS IP preference logic
    let server_url = Arc::new(find_best_server_ip(server_url).await?);
    let local_addr = Arc::new(local_addr);

    // TCP Handler
    let tcp_listener = TcpListener::bind(&*local_addr).await.context("Failed to bind local TCP listener")?;
    let server_url_tcp = server_url.clone();
    tokio::spawn(async move {
        loop {
            let (local_stream, addr) = tcp_listener.accept().await.unwrap();
            info!(peer = %addr, "Accepted new local TCP connection");
            
            let server_url_clone = server_url_tcp.clone();
            tokio::spawn(async move {
                let uuid = format!("T-{}", Uuid::new_v4().simple());
                if let Err(e) = handle_client_tcp_connection(uuid, local_stream, server_url_clone).await {
                    warn!("Client TCP handler error: {:?}", e);
                }
            });
        }
    });

    // UDP Handler
    let server_url_udp = server_url.clone();
    let local_addr_udp = local_addr.clone();
    tokio::spawn(async move {
        let uuid = format!("U-{}", Uuid::new_v4().simple());
        if let Err(e) = handle_client_udp_connection(uuid, &local_addr_udp, server_url_udp).await {
            error!("Client UDP handler failed catastrophically: {:?}", e);
        }
    });
    
    // Keep main alive
    tokio::signal::ctrl_c().await?;
    info!("Client shutting down.");
    Ok(())
}

async fn handle_client_tcp_connection(uuid: String, local_stream: TcpStream, server_url: Arc<String>) -> Result<()> {
    info!(uuid, "Attempting to connect to WebSocket server...");
    
    let (ws_stream, _) = connect_async(&**server_url).await.context("Failed to connect to WebSocket server")?;
    let (mut ws_sender, ws_receiver) = ws_stream.split();
    
    // Send our UUID to the server to initiate the tunnel
    ws_sender.send(Message::Text(uuid.clone().into())).await?;
    info!(uuid, "WebSocket connection established, sent UUID.");

    proxy_tcp(uuid, Arc::new(RwLock::new(HashMap::new())), ws_sender, ws_receiver, local_stream).await?;
    Ok(())
}

async fn handle_client_udp_connection(uuid: String, local_addr: &str, server_url: Arc<String>) -> Result<()> {
    let local_socket = UdpSocket::bind(local_addr).await.context("Failed to bind local UDP socket")?;
    info!(uuid, "Listening for UDP packets on {}", local_addr);

    let mut buf = BytesMut::with_capacity(UDP_BUFFER_SIZE);
    buf.resize(UDP_BUFFER_SIZE, 0);
    let mut ws_sender_opt: Option<SplitSink<_, Message>> = None;
    
    // We need a channel to receive data from the WebSocket task
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Bytes>(1000); // Increased buffer size
    let mut last_peer_addr: Option<SocketAddr> = None;

    loop {
        tokio::select! {
            // Data received from a local application (e.g., a game client)
            Ok((len, peer_addr)) = local_socket.recv_from(&mut buf) => {
                last_peer_addr = Some(peer_addr);
                let data = buf[..len].to_vec();

                // If this is the first packet, establish the WebSocket connection
                if ws_sender_opt.is_none() {
                    info!(uuid, "First UDP packet received, connecting to WebSocket server...");
                    match connect_async(&**server_url).await {
                        Ok((ws, _)) => {
                            let (sender, mut receiver) = ws.split();
                            ws_sender_opt = Some(sender);
                            
                            // Send UUID
                            if let Some(s) = ws_sender_opt.as_mut() {
                                s.send(Message::Text(uuid.clone().into())).await?;
                            }
                            
                            // Spawn task to read from WebSocket and send to our mpsc channel
                            let tx_clone = tx.clone();
                            tokio::spawn(async move {
                                while let Some(Ok(Message::Binary(data))) = receiver.next().await {
                                    if tx_clone.send(Bytes::from(data)).await.is_err() {
                                        break; // Channel closed
                                    }
                                }
                            });
                        },
                        Err(e) => {
                            error!(uuid, "Failed to establish WebSocket for UDP: {:?}", e);
                            continue;
                        }
                    }
                }
                
                // Forward the UDP packet over the WebSocket
                if let Some(ws_sender) = ws_sender_opt.as_mut() {
                    if ws_sender.send(Message::Binary(data.into())).await.is_err() {
                        warn!(uuid, "WebSocket send error, connection may be down. Will attempt to reconnect on next packet.");
                        ws_sender_opt = None; // Reset to trigger reconnection
                    }
                }
            },
            // Data received from the WebSocket (from the server)
            Some(data) = rx.recv() => {
                if let Some(peer_addr) = last_peer_addr {
                    if let Err(e) = local_socket.send_to(&data, peer_addr).await {
                        warn!(uuid, "Failed to send UDP packet to local peer {}: {:?}", peer_addr, e);
                    }
                }
            }
        }
    }
}


// --- Generic Proxying Logic ---

async fn proxy_tcp<S>(
    uuid: String,
    tunnels: TunnelMap,
    mut ws_sender: SplitSink<WebSocketStream<S>, Message>,
    mut ws_receiver: SplitStream<WebSocketStream<S>>,
    mut tcp_stream: TcpStream,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (tcp_reader, tcp_writer) = tcp_stream.split();
    let mut tcp_reader = BufReader::with_capacity(TCP_BUFFER_SIZE, tcp_reader);
    let mut tcp_writer = BufWriter::with_capacity(TCP_BUFFER_SIZE, tcp_writer);
    let mut tcp_buf = BytesMut::with_capacity(TCP_BUFFER_SIZE);
    tcp_buf.resize(TCP_BUFFER_SIZE, 0);

    // WebSocket ping interval timer
    let mut ping_interval = tokio::time::interval(WS_PING_INTERVAL);
    ping_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            // WebSocket ping timer
            _ = ping_interval.tick() => {
                if ws_sender.send(Message::Ping(Bytes::new())).await.is_err() {
                    info!(uuid, "Failed to send ping, connection may be closed");
                    break;
                }
            },
            // Read from WebSocket, write to TCP
            Some(Ok(msg)) = ws_receiver.next() => {
                if let Ok(mut map) = tunnels.try_write() {
                    if let Some(info) = map.get_mut(&uuid) {
                        info.last_activity = Instant::now();
                    }
                }

                match msg {
                    Message::Binary(data) => {
                        if tcp_writer.write_all(&data).await.is_err() || tcp_writer.flush().await.is_err() {
                            break; // TCP connection closed
                        }
                    }
                    Message::Ping(data) => {
                        if ws_sender.send(Message::Pong(data)).await.is_err() {
                            break;
                        }
                    }
                    Message::Close(_) => {
                        break;
                    }
                    _ => {} // Ignore Text, Pong
                }
            },
            // Read from TCP, write to WebSocket
            Ok(n) = tcp_reader.read(&mut tcp_buf) => {
                if n == 0 {
                    break; // TCP connection closed
                }
                if let Ok(mut map) = tunnels.try_write() {
                    if let Some(info) = map.get_mut(&uuid) {
                        info.last_activity = Instant::now();
                    }
                }
                if ws_sender.send(Message::Binary(tcp_buf[..n].to_vec().into())).await.is_err() {
                    break;
                }
            },
            else => {
                break; // One of the streams has an error or is closed
            }
        }
    }
    
    info!(uuid, "Proxy loop terminated. Closing connection.");
    ws_sender.close().await.ok();
    // TCP stream is closed automatically when it goes out of scope.
    if let Ok(mut map) = tunnels.try_write() {
        map.remove(&uuid);
    }
    Ok(())
}

async fn proxy_udp<S>(
    uuid: String,
    tunnels: TunnelMap,
    mut ws_sender: SplitSink<WebSocketStream<S>, Message>,
    mut ws_receiver: SplitStream<WebSocketStream<S>>,
    udp_socket: UdpSocket,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut udp_buf = BytesMut::with_capacity(UDP_BUFFER_SIZE);
    udp_buf.resize(UDP_BUFFER_SIZE, 0);

    loop {
        tokio::select! {
             // Read from WebSocket, write to UDP target
            Some(Ok(msg)) = ws_receiver.next() => {
                if let Ok(mut map) = tunnels.try_write() {
                    if let Some(info) = map.get_mut(&uuid) {
                        info.last_activity = Instant::now();
                    }
                }
                if let Message::Binary(data) = msg {
                    if udp_socket.send(&data).await.is_err() {
                        break;
                    }
                }
            },
            // Read from UDP target, write to WebSocket
            Ok(n) = udp_socket.recv(&mut udp_buf) => {
                 if let Ok(mut map) = tunnels.try_write() {
                    if let Some(info) = map.get_mut(&uuid) {
                        info.last_activity = Instant::now();
                    }
                }
                if ws_sender.send(Message::Binary(udp_buf[..n].to_vec().into())).await.is_err() {
                    break;
                }
            },
            else => {
                break;
            }
        }
    }
    
    info!(uuid, "UDP Proxy loop terminated. Closing connection.");
    ws_sender.close().await.ok();
    if let Ok(mut map) = tunnels.try_write() {
        map.remove(&uuid);
    }
    Ok(())
}

// --- Helper: DNS IP Preference Logic ---

async fn find_best_server_ip(server_url: String) -> Result<String> {
    let mut url = Url::parse(&server_url)?;
    let host = url.host_str().unwrap_or("").to_string();

    // Do not resolve if it's already an IP address
    if host.parse::<std::net::IpAddr>().is_ok() {
        info!("Server address is an IP, skipping DNS lookup.");
        return Ok(server_url);
    }
    
    info!("Resolving and pinging IPs for host: {}", host);
    let resolver = TokioAsyncResolver::tokio(ResolverConfig::default(), ResolverOpts::default());
    let response = resolver.lookup_ip(&host).await?;

    let port = url.port().unwrap_or(if url.scheme() == "wss" { 443 } else { 80 });

    let mut best_ip = None;
    let mut min_latency = Duration::from_secs(10);

    for ip in response.iter() {
        let addr = SocketAddr::new(ip, port);
        let start_time = Instant::now();
        match timeout(Duration::from_secs(2), TcpStream::connect(addr)).await {
            Ok(Ok(_)) => {
                let latency = start_time.elapsed();
                info!(ip = %ip, latency = ?latency, "Ping successful");
                if latency < min_latency {
                    min_latency = latency;
                    best_ip = Some(ip);
                }
            }
            _ => {
                warn!(ip = %ip, "Ping failed or timed out");
            }
        }
    }

    if let Some(ip) = best_ip {
        info!(ip = %ip, latency = ?min_latency, "Selected best IP");
        url.set_ip_host(ip).map_err(|_| anyhow!("Failed to set IP host"))?;
        Ok(url.to_string())
    } else {
        Err(anyhow!("Could not find any responsive IP for host: {}", host))
    }
}