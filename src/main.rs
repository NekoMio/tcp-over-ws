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
    sync::{RwLock, mpsc},
    time::timeout,
};
use tokio_tungstenite::{
    accept_async, connect_async, tungstenite::protocol::Message, WebSocketStream,
};
use tonic::{transport::Server, Request, Response, Status, Streaming};
use tokio_stream::wrappers::ReceiverStream;
use tracing::{error, info, warn, Level};
use trust_dns_resolver::{config::*, TokioAsyncResolver};
use url::Url;
use uuid::Uuid;

// gRPC generated code
pub mod tunnel {
    tonic::include_proto!("tunnel");
}

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
        /// Server URL (e.g., ws://example.com:8080 for WebSocket or grpc://example.com:8080 for gRPC)
        server_url: String,
        /// Local address to listen on (e.g., 127.0.0.1:3389)
        local_addr: String,
    },
    /// Run in server mode
    /// e.g., server 0.0.0.0:8080 192.168.1.10:3389
    Server {
        /// Address to listen for connections (e.g., 0.0.0.0:8080)
        listen_addr: String,
        /// Target address to forward traffic to (e.g., 127.0.0.1:3389)
        target_addr: String,
        /// Protocol to use: ws for WebSocket, grpc for gRPC (default: ws)
        #[clap(long, default_value = "ws")]
        protocol: String,
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
        Commands::Client {
            server_url,
            local_addr,
        } => run_client(server_url, local_addr).await,
        Commands::Server {
            listen_addr,
            target_addr,
            protocol,
        } => run_server(listen_addr, target_addr, protocol).await,
    }
}

// --- Server Implementation ---

async fn run_server(listen_addr: String, target_addr: String, protocol: String) -> Result<()> {
    info!("Starting server mode...");
    info!("Protocol: {}", protocol);
    info!("Listening for connections on: {}", listen_addr);
    info!("Forwarding traffic to: {}", target_addr);

    match protocol.as_str() {
        "grpc" => run_grpc_server(listen_addr, target_addr).await,
        "ws" | _ => run_ws_server(listen_addr, target_addr).await,
    }
}

async fn run_ws_server(listen_addr: String, target_addr: String) -> Result<()> {

    let listener = TcpListener::bind(&listen_addr)
        .await
        .context("Failed to bind server listener")?;
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
            if let Err(e) =
                handle_server_connection(socket, addr, tunnel_map_ref, target_addr_ref).await
            {
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
    info!("New WebSocket handshake from {}", addr);

    let ws_stream = accept_async(stream)
        .await
        .context("WebSocket handshake failed")?;
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

    {
        // Scope for the write lock
        let mut tunnel_map = tunnels.write().await;
        tunnel_map.insert(
            uuid.clone(),
            TunnelInfo {
                last_activity: Instant::now(),
            },
        );
    }

    if is_udp {
        // Handle UDP
        let local_udp_socket = UdpSocket::bind("0.0.0.0:0")
            .await
            .context("Failed to bind local UDP socket")?;
        local_udp_socket
            .connect(&*target_addr)
            .await
            .context("Failed to connect to target UDP address")?;
        info!(uuid, "UDP tunnel established for target {}", target_addr);
        proxy_udp(
            uuid.clone(),
            tunnels,
            ws_sender,
            ws_receiver,
            local_udp_socket,
        )
        .await?;
    } else {
        // Handle TCP
        let target_stream = TcpStream::connect(&*target_addr)
            .await
            .context("Failed to connect to target TCP address")?;
        info!(uuid, "TCP tunnel established for target {}", target_addr);
        proxy_tcp(uuid.clone(), tunnels, ws_sender, ws_receiver, target_stream).await?;
    }

    Ok(())
}

// --- Client Implementation ---

async fn run_client(server_url: String, local_addr: String) -> Result<()> {
    if server_url.starts_with("grpc://") || server_url.starts_with("grpcs://") {
        // Convert grpc:// to http:// and grpcs:// to https:// for tonic
        let grpc_url = server_url.replace("grpc://", "http://").replace("grpcs://", "https://");
        run_grpc_client(grpc_url, local_addr).await
    } else {
        run_ws_client(server_url, local_addr).await
    }
}

async fn run_ws_client(server_url: String, local_addr: String) -> Result<()> {
    info!("Starting client mode...");
    info!("Connecting to WebSocket server: {}", server_url);
    info!("Listening for local connections on: {}", local_addr);

    // Perform DNS IP preference logic
    let server_url = Arc::new(find_best_server_ip(server_url).await?);
    let local_addr = Arc::new(local_addr);

    // TCP Handler
    let tcp_listener = TcpListener::bind(&*local_addr)
        .await
        .context("Failed to bind local TCP listener")?;
    let server_url_tcp = server_url.clone();
    tokio::spawn(async move {
        loop {
            let (local_stream, addr) = tcp_listener.accept().await.unwrap();
            info!(peer = %addr, "Accepted new local TCP connection");

            let server_url_clone = server_url_tcp.clone();
            tokio::spawn(async move {
                let uuid = format!("T-{}", Uuid::new_v4().simple());
                if let Err(e) =
                    handle_client_tcp_connection(uuid, local_stream, server_url_clone).await
                {
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

async fn handle_client_tcp_connection(
    uuid: String,
    local_stream: TcpStream,
    server_url: Arc<String>,
) -> Result<()> {
    info!(uuid, "Attempting to connect to WebSocket server...");

    let (ws_stream, _) = connect_async(&**server_url)
        .await
        .context("Failed to connect to WebSocket server")?;
    let (mut ws_sender, ws_receiver) = ws_stream.split();

    // Send our UUID to the server to initiate the tunnel
    ws_sender.send(Message::Text(uuid.clone().into())).await?;
    info!(uuid, "WebSocket connection established, sent UUID.");

    proxy_tcp(
        uuid,
        Arc::new(RwLock::new(HashMap::new())),
        ws_sender,
        ws_receiver,
        local_stream,
    )
    .await?;
    Ok(())
}

async fn handle_client_udp_connection(
    uuid: String,
    local_addr: &str,
    server_url: Arc<String>,
) -> Result<()> {
    let local_socket = UdpSocket::bind(local_addr)
        .await
        .context("Failed to bind local UDP socket")?;
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
                                    if tx_clone.send(data).await.is_err() {
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

    let port = url
        .port()
        .unwrap_or(if url.scheme() == "wss" { 443 } else { 80 });

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
        url.set_ip_host(ip)
            .map_err(|_| anyhow!("Failed to set IP host"))?;
        Ok(url.to_string())
    } else {
        Err(anyhow!(
            "Could not find any responsive IP for host: {}",
            host
        ))
    }
}

// --- gRPC Server Implementation ---

async fn run_grpc_server(listen_addr: String, target_addr: String) -> Result<()> {
    let tunnel_map = TunnelMap::new(RwLock::new(HashMap::new()));
    let target_addr = Arc::new(target_addr);

    // Spawn cleanup task
    let tunnel_map_clone = tunnel_map.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(CLEANUP_INTERVAL).await;
            let mut tunnels = tunnel_map_clone.write().await;
            let now = Instant::now();
            tunnels.retain(|uuid, info| {
                let stale = now.duration_since(info.last_activity) > STALE_CONNECTION_TIMEOUT;
                if stale {
                    warn!(uuid, "Pruning stale gRPC connection.");
                }
                !stale
            });
        }
    });

    let tunnel_service = TunnelServiceImpl {
        tunnels: tunnel_map,
        target_addr,
    };

    let addr = listen_addr.parse()?;
    info!("gRPC server listening on {}", addr);

    Server::builder()
        .add_service(tunnel::tunnel_service_server::TunnelServiceServer::new(tunnel_service))
        .serve(addr)
        .await?;

    Ok(())
}

#[derive(Clone)]
struct TunnelServiceImpl {
    tunnels: TunnelMap,
    target_addr: Arc<String>,
}

#[tonic::async_trait]
impl tunnel::tunnel_service_server::TunnelService for TunnelServiceImpl {
    type TcpTunnelStream = ReceiverStream<Result<tunnel::TunnelMessage, Status>>;
    type UdpTunnelStream = ReceiverStream<Result<tunnel::TunnelMessage, Status>>;

    async fn tcp_tunnel(
        &self,
        request: Request<Streaming<tunnel::TunnelMessage>>,
    ) -> Result<Response<Self::TcpTunnelStream>, Status> {
        let mut stream = request.into_inner();
        let (tx, rx) = mpsc::channel(1000);

        // Get initial message with UUID
        let init_msg = stream.next().await
            .ok_or_else(|| Status::invalid_argument("Missing init message"))?
            .map_err(|e| Status::internal(format!("Stream error: {}", e)))?;

        let uuid = match init_msg.message_type {
            Some(tunnel::tunnel_message::MessageType::Init(init)) => init.uuid,
            _ => return Err(Status::invalid_argument("First message must be init")),
        };

        info!(uuid, "gRPC TCP tunnel connection established");

        // Register tunnel
        {
            let mut tunnels = self.tunnels.write().await;
            tunnels.insert(uuid.clone(), TunnelInfo {
                last_activity: Instant::now(),
            });
        }

        // Connect to target
        let target_stream = TcpStream::connect(&*self.target_addr).await
            .map_err(|e| Status::internal(format!("Failed to connect to target: {}", e)))?;

        let tunnels_clone = self.tunnels.clone();
        let uuid_clone = uuid.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_grpc_tcp_proxy(uuid_clone, tunnels_clone, stream, tx, target_stream).await {
                error!("gRPC TCP proxy error: {:?}", e);
            }
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }

    async fn udp_tunnel(
        &self,
        request: Request<Streaming<tunnel::TunnelMessage>>,
    ) -> Result<Response<Self::UdpTunnelStream>, Status> {
        let mut stream = request.into_inner();
        let (tx, rx) = mpsc::channel(1000);

        // Get initial message with UUID
        let init_msg = stream.next().await
            .ok_or_else(|| Status::invalid_argument("Missing init message"))?
            .map_err(|e| Status::internal(format!("Stream error: {}", e)))?;

        let uuid = match init_msg.message_type {
            Some(tunnel::tunnel_message::MessageType::Init(init)) => init.uuid,
            _ => return Err(Status::invalid_argument("First message must be init")),
        };

        info!(uuid, "gRPC UDP tunnel connection established");

        // Register tunnel
        {
            let mut tunnels = self.tunnels.write().await;
            tunnels.insert(uuid.clone(), TunnelInfo {
                last_activity: Instant::now(),
            });
        }

        // Bind local UDP socket and connect to target
        let udp_socket = UdpSocket::bind("0.0.0.0:0").await
            .map_err(|e| Status::internal(format!("Failed to bind UDP socket: {}", e)))?;
        udp_socket.connect(&*self.target_addr).await
            .map_err(|e| Status::internal(format!("Failed to connect to target: {}", e)))?;

        let tunnels_clone = self.tunnels.clone();
        let uuid_clone = uuid.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_grpc_udp_proxy(uuid_clone, tunnels_clone, stream, tx, udp_socket).await {
                error!("gRPC UDP proxy error: {:?}", e);
            }
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }
}

async fn handle_grpc_tcp_proxy(
    uuid: String,
    tunnels: TunnelMap,
    mut grpc_stream: Streaming<tunnel::TunnelMessage>,
    grpc_sender: mpsc::Sender<Result<tunnel::TunnelMessage, Status>>,
    mut tcp_stream: TcpStream,
) -> Result<()> {
    let (tcp_reader, tcp_writer) = tcp_stream.split();
    let mut tcp_reader = BufReader::with_capacity(TCP_BUFFER_SIZE, tcp_reader);
    let mut tcp_writer = BufWriter::with_capacity(TCP_BUFFER_SIZE, tcp_writer);
    let mut tcp_buf = BytesMut::with_capacity(TCP_BUFFER_SIZE);
    tcp_buf.resize(TCP_BUFFER_SIZE, 0);

    loop {
        tokio::select! {
            // Read from gRPC stream, write to TCP
            Some(result) = grpc_stream.next() => {
                match result {
                    Ok(msg) => {
                        if let Ok(mut map) = tunnels.try_write() {
                            if let Some(info) = map.get_mut(&uuid) {
                                info.last_activity = Instant::now();
                            }
                        }

                        match msg.message_type {
                            Some(tunnel::tunnel_message::MessageType::Data(data)) => {
                                if tcp_writer.write_all(&data.payload).await.is_err() || tcp_writer.flush().await.is_err() {
                                    break;
                                }
                            }
                            Some(tunnel::tunnel_message::MessageType::Ping(ping)) => {
                                let pong_msg = tunnel::TunnelMessage {
                                    message_type: Some(tunnel::tunnel_message::MessageType::Pong(
                                        tunnel::PongMessage { timestamp: ping.timestamp }
                                    )),
                                };
                                if grpc_sender.send(Ok(pong_msg)).await.is_err() {
                                    break;
                                }
                            }
                            Some(tunnel::tunnel_message::MessageType::Close(_)) => {
                                break;
                            }
                            _ => {}
                        }
                    }
                    Err(_) => break,
                }
            },
            // Read from TCP, write to gRPC stream
            Ok(n) = tcp_reader.read(&mut tcp_buf) => {
                if n == 0 {
                    break;
                }
                if let Ok(mut map) = tunnels.try_write() {
                    if let Some(info) = map.get_mut(&uuid) {
                        info.last_activity = Instant::now();
                    }
                }

                let data_msg = tunnel::TunnelMessage {
                    message_type: Some(tunnel::tunnel_message::MessageType::Data(
                        tunnel::DataMessage {
                            payload: tcp_buf[..n].to_vec(),
                        }
                    )),
                };

                if grpc_sender.send(Ok(data_msg)).await.is_err() {
                    break;
                }
            },
            else => {
                break;
            }
        }
    }

    info!(uuid, "gRPC TCP proxy loop terminated");
    let close_msg = tunnel::TunnelMessage {
        message_type: Some(tunnel::tunnel_message::MessageType::Close(
            tunnel::CloseMessage {
                reason: "Connection closed".to_string(),
            }
        )),
    };
    grpc_sender.send(Ok(close_msg)).await.ok();

    if let Ok(mut map) = tunnels.try_write() {
        map.remove(&uuid);
    }
    Ok(())
}

async fn handle_grpc_udp_proxy(
    uuid: String,
    tunnels: TunnelMap,
    mut grpc_stream: Streaming<tunnel::TunnelMessage>,
    grpc_sender: mpsc::Sender<Result<tunnel::TunnelMessage, Status>>,
    udp_socket: UdpSocket,
) -> Result<()> {
    let mut udp_buf = BytesMut::with_capacity(UDP_BUFFER_SIZE);
    udp_buf.resize(UDP_BUFFER_SIZE, 0);

    loop {
        tokio::select! {
            // Read from gRPC stream, write to UDP
            Some(result) = grpc_stream.next() => {
                match result {
                    Ok(msg) => {
                        if let Ok(mut map) = tunnels.try_write() {
                            if let Some(info) = map.get_mut(&uuid) {
                                info.last_activity = Instant::now();
                            }
                        }

                        match msg.message_type {
                            Some(tunnel::tunnel_message::MessageType::Data(data)) => {
                                if udp_socket.send(&data.payload).await.is_err() {
                                    break;
                                }
                            }
                            Some(tunnel::tunnel_message::MessageType::Ping(ping)) => {
                                let pong_msg = tunnel::TunnelMessage {
                                    message_type: Some(tunnel::tunnel_message::MessageType::Pong(
                                        tunnel::PongMessage { timestamp: ping.timestamp }
                                    )),
                                };
                                if grpc_sender.send(Ok(pong_msg)).await.is_err() {
                                    break;
                                }
                            }
                            Some(tunnel::tunnel_message::MessageType::Close(_)) => {
                                break;
                            }
                            _ => {}
                        }
                    }
                    Err(_) => break,
                }
            },
            // Read from UDP, write to gRPC stream
            Ok(n) = udp_socket.recv(&mut udp_buf) => {
                if let Ok(mut map) = tunnels.try_write() {
                    if let Some(info) = map.get_mut(&uuid) {
                        info.last_activity = Instant::now();
                    }
                }

                let data_msg = tunnel::TunnelMessage {
                    message_type: Some(tunnel::tunnel_message::MessageType::Data(
                        tunnel::DataMessage {
                            payload: udp_buf[..n].to_vec(),
                        }
                    )),
                };

                if grpc_sender.send(Ok(data_msg)).await.is_err() {
                    break;
                }
            },
            else => {
                break;
            }
        }
    }

    info!(uuid, "gRPC UDP proxy loop terminated");
    let close_msg = tunnel::TunnelMessage {
        message_type: Some(tunnel::tunnel_message::MessageType::Close(
            tunnel::CloseMessage {
                reason: "Connection closed".to_string(),
            }
        )),
    };
    grpc_sender.send(Ok(close_msg)).await.ok();

    if let Ok(mut map) = tunnels.try_write() {
        map.remove(&uuid);
    }
    Ok(())
}

// --- gRPC Client Implementation ---

async fn run_grpc_client(server_url: String, local_addr: String) -> Result<()> {
    info!("Starting gRPC client mode...");
    info!("Connecting to gRPC server: {}", server_url);
    info!("Listening for local connections on: {}", local_addr);

    let server_url = Arc::new(server_url);
    let local_addr = Arc::new(local_addr);

    // TCP Handler
    let tcp_listener = TcpListener::bind(&*local_addr)
        .await
        .context("Failed to bind local TCP listener")?;
    let server_url_tcp = server_url.clone();
    tokio::spawn(async move {
        loop {
            let (local_stream, addr) = tcp_listener.accept().await.unwrap();
            info!(peer = %addr, "Accepted new local TCP connection");

            let server_url_clone = server_url_tcp.clone();
            tokio::spawn(async move {
                let uuid = format!("T-{}", Uuid::new_v4().simple());
                if let Err(e) =
                    handle_grpc_client_tcp_connection(uuid, local_stream, server_url_clone).await
                {
                    warn!("gRPC client TCP handler error: {:?}", e);
                }
            });
        }
    });

    // UDP Handler
    let server_url_udp = server_url.clone();
    let local_addr_udp = local_addr.clone();
    tokio::spawn(async move {
        let uuid = format!("U-{}", Uuid::new_v4().simple());
        if let Err(e) = handle_grpc_client_udp_connection(uuid, &local_addr_udp, server_url_udp).await {
            error!("gRPC client UDP handler failed catastrophically: {:?}", e);
        }
    });

    // Keep main alive
    tokio::signal::ctrl_c().await?;
    info!("gRPC client shutting down.");
    Ok(())
}

async fn handle_grpc_client_tcp_connection(
    uuid: String,
    mut local_stream: TcpStream,
    server_url: Arc<String>,
) -> Result<()> {
    info!(uuid, "Attempting to connect to gRPC server...");

    let server_url_string = server_url.as_ref().clone();
    let mut client = tunnel::tunnel_service_client::TunnelServiceClient::connect(server_url_string).await
        .context("Failed to connect to gRPC server")?;

    let (tx, rx) = mpsc::channel(1000);
    let outbound = ReceiverStream::new(rx);

    // Send init message
    let init_msg = tunnel::TunnelMessage {
        message_type: Some(tunnel::tunnel_message::MessageType::Init(
            tunnel::InitMessage {
                uuid: uuid.clone(),
                protocol: "tcp".to_string(),
            }
        )),
    };
    tx.send(init_msg).await?;

    let response = client.tcp_tunnel(Request::new(outbound)).await?;
    let mut inbound = response.into_inner();

    info!(uuid, "gRPC TCP connection established");

    let (tcp_reader, tcp_writer) = local_stream.split();
    let mut tcp_reader = BufReader::with_capacity(TCP_BUFFER_SIZE, tcp_reader);
    let mut tcp_writer = BufWriter::with_capacity(TCP_BUFFER_SIZE, tcp_writer);
    let mut tcp_buf = BytesMut::with_capacity(TCP_BUFFER_SIZE);
    tcp_buf.resize(TCP_BUFFER_SIZE, 0);

    loop {
        tokio::select! {
            // Read from gRPC, write to TCP
            Some(result) = inbound.next() => {
                match result {
                    Ok(msg) => {
                        match msg.message_type {
                            Some(tunnel::tunnel_message::MessageType::Data(data)) => {
                                if tcp_writer.write_all(&data.payload).await.is_err() || tcp_writer.flush().await.is_err() {
                                    break;
                                }
                            }
                            Some(tunnel::tunnel_message::MessageType::Close(_)) => {
                                break;
                            }
                            _ => {}
                        }
                    }
                    Err(_) => break,
                }
            },
            // Read from TCP, write to gRPC
            Ok(n) = tcp_reader.read(&mut tcp_buf) => {
                if n == 0 {
                    break;
                }

                let data_msg = tunnel::TunnelMessage {
                    message_type: Some(tunnel::tunnel_message::MessageType::Data(
                        tunnel::DataMessage {
                            payload: tcp_buf[..n].to_vec(),
                        }
                    )),
                };

                if tx.send(data_msg).await.is_err() {
                    break;
                }
            },
            else => {
                break;
            }
        }
    }

    info!(uuid, "gRPC client TCP connection closed");
    Ok(())
}

async fn handle_grpc_client_udp_connection(
    uuid: String,
    local_addr: &str,
    server_url: Arc<String>,
) -> Result<()> {
    let local_socket = UdpSocket::bind(local_addr)
        .await
        .context("Failed to bind local UDP socket")?;
    info!(uuid, "Listening for UDP packets on {}", local_addr);

    let server_url_string = server_url.as_ref().clone();
    let mut client = tunnel::tunnel_service_client::TunnelServiceClient::connect(server_url_string).await
        .context("Failed to connect to gRPC server")?;

    let (tx, rx) = mpsc::channel(1000);
    let outbound = ReceiverStream::new(rx);

    // Send init message
    let init_msg = tunnel::TunnelMessage {
        message_type: Some(tunnel::tunnel_message::MessageType::Init(
            tunnel::InitMessage {
                uuid: uuid.clone(),
                protocol: "udp".to_string(),
            }
        )),
    };
    tx.send(init_msg).await?;

    let response = client.udp_tunnel(Request::new(outbound)).await?;
    let mut inbound = response.into_inner();

    info!(uuid, "gRPC UDP connection established");

    let mut buf = BytesMut::with_capacity(UDP_BUFFER_SIZE);
    buf.resize(UDP_BUFFER_SIZE, 0);
    let mut last_peer_addr: Option<SocketAddr> = None;

    loop {
        tokio::select! {
            // Data received from local application
            Ok((len, peer_addr)) = local_socket.recv_from(&mut buf) => {
                last_peer_addr = Some(peer_addr);
                let data = buf[..len].to_vec();

                let data_msg = tunnel::TunnelMessage {
                    message_type: Some(tunnel::tunnel_message::MessageType::Data(
                        tunnel::DataMessage {
                            payload: data,
                        }
                    )),
                };

                if tx.send(data_msg).await.is_err() {
                    warn!(uuid, "Failed to send UDP data to gRPC stream");
                    break;
                }
            },
            // Data received from gRPC
            Some(result) = inbound.next() => {
                match result {
                    Ok(msg) => {
                        match msg.message_type {
                            Some(tunnel::tunnel_message::MessageType::Data(data)) => {
                                if let Some(peer_addr) = last_peer_addr {
                                    if let Err(e) = local_socket.send_to(&data.payload, peer_addr).await {
                                        warn!(uuid, "Failed to send UDP packet to local peer {}: {:?}", peer_addr, e);
                                    }
                                }
                            }
                            Some(tunnel::tunnel_message::MessageType::Close(_)) => {
                                break;
                            }
                            _ => {}
                        }
                    }
                    Err(_) => break,
                }
            }
        }
    }

    info!(uuid, "gRPC client UDP connection closed");
    Ok(())
}
