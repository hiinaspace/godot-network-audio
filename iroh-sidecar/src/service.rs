use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, Mutex, RwLock},
    time::{Duration, Instant},
};

use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use iroh::{
    endpoint::{presets, Connection, PathId},
    Endpoint, EndpointAddr, EndpointId,
};
use tokio::{
    runtime::{Builder, Runtime},
    sync::{broadcast, oneshot},
};

pub const VOICE_ALPN_V0: &[u8] = b"godot-network-audio/voice/0";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RemotePeer {
    pub id: EndpointId,
}

#[derive(Debug, Clone)]
pub enum VoiceEvent {
    PeerConnected {
        peer: RemotePeer,
    },
    PeerReplaced {
        peer: RemotePeer,
    },
    PeerDisconnected {
        peer: RemotePeer,
    },
    PacketReceived {
        peer: RemotePeer,
        bytes: Bytes,
        received_at_mono_us: u64,
    },
}

#[derive(Debug, Clone)]
pub struct VoiceIrohConfig {
    pub alpn: Vec<u8>,
    /// If `Some`, bind only to this address and disable relay/pkarr/DNS.
    /// Intended for netem testing over a veth pair where all traffic must
    /// go through a specific interface.  When `None`, the relay preset
    /// handles binding.
    pub bind_addr: Option<SocketAddr>,
    /// If `false`, use `Builder::empty()` — no relay, no pkarr/DNS.
    /// Automatically implied when `bind_addr` is `Some`.  Default `true`.
    pub relay: bool,
}

impl Default for VoiceIrohConfig {
    fn default() -> Self {
        Self {
            alpn: VOICE_ALPN_V0.to_vec(),
            bind_addr: None,
            relay: true,
        }
    }
}

type PacketHandler = Arc<dyn Fn(RemotePeer, Bytes, u64) + Send + Sync>;

struct SharedState {
    start: Instant,
    peers: Mutex<HashMap<EndpointId, Connection>>,
    events: broadcast::Sender<VoiceEvent>,
    alpn: Vec<u8>,
    packet_handler: RwLock<Option<PacketHandler>>,
}

impl SharedState {
    fn now_mono_us(&self) -> u64 {
        self.start.elapsed().as_micros() as u64
    }

    fn insert_connection(&self, connection: Connection) -> VoiceEvent {
        let peer = RemotePeer {
            id: connection.remote_id(),
        };
        let mut peers = self.peers.lock().expect("peer map poisoned");
        let replaced = peers.insert(peer.id, connection).is_some();
        if replaced {
            VoiceEvent::PeerReplaced { peer }
        } else {
            VoiceEvent::PeerConnected { peer }
        }
    }

    fn remove_connection(&self, peer: EndpointId) -> Option<VoiceEvent> {
        let mut peers = self.peers.lock().expect("peer map poisoned");
        if peers.remove(&peer).is_some() {
            Some(VoiceEvent::PeerDisconnected {
                peer: RemotePeer { id: peer },
            })
        } else {
            None
        }
    }
}

pub struct VoiceIrohService {
    runtime: Runtime,
    endpoint: Endpoint,
    shared: Arc<SharedState>,
    stop_tx: Option<oneshot::Sender<()>>,
    accept_task: Option<tokio::task::JoinHandle<()>>,
}

impl std::fmt::Debug for VoiceIrohService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VoiceIrohService")
            .field("endpoint_id", &self.endpoint.id())
            .finish_non_exhaustive()
    }
}

impl VoiceIrohService {
    pub fn bind_default() -> Result<Self> {
        Self::bind(VoiceIrohConfig::default())
    }

    pub fn bind(config: VoiceIrohConfig) -> Result<Self> {
        let runtime = Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("gna-iroh")
            .build()
            .context("build tokio runtime for iroh sidecar")?;

        let (events, _) = broadcast::channel(256);
        let shared = Arc::new(SharedState {
            start: Instant::now(),
            peers: Mutex::new(HashMap::new()),
            events,
            alpn: config.alpn.clone(),
            packet_handler: RwLock::new(None),
        });

        let use_relay = config.relay && config.bind_addr.is_none();
        let endpoint = if use_relay {
            runtime.block_on(async {
                Endpoint::builder(presets::N0)
                    .alpns(vec![config.alpn.clone()])
                    .bind()
                    .await
            })?
        } else {
            // Local-only mode: no relay, no pkarr/DNS.  Bind to the specific
            // address supplied, or any local port if None.  Used for netem
            // tests over a veth pair where all traffic must go through one
            // interface and the relay would bypass the shaping.
            let mut builder = Endpoint::builder(presets::Minimal)
                .clear_ip_transports()
                .alpns(vec![config.alpn.clone()]);
            if let Some(addr) = config.bind_addr {
                builder = builder.bind_addr(addr).context("bind local address")?;
            }
            runtime.block_on(async { builder.bind().await })?
        };
        if use_relay {
            runtime.block_on(endpoint.online());
        }

        let (stop_tx, stop_rx) = oneshot::channel();
        let accept_task = runtime.spawn(run_accept_loop(endpoint.clone(), shared.clone(), stop_rx));

        Ok(Self {
            runtime,
            endpoint,
            shared,
            stop_tx: Some(stop_tx),
            accept_task: Some(accept_task),
        })
    }

    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    pub fn endpoint_id(&self) -> EndpointId {
        self.endpoint.id()
    }

    pub fn endpoint_addr(&self) -> EndpointAddr {
        self.endpoint.addr()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<VoiceEvent> {
        self.shared.events.subscribe()
    }

    pub fn connect(&self, addr: EndpointAddr) -> Result<RemotePeer> {
        let alpn = self.shared.alpn.clone();
        let shared = self.shared.clone();
        self.runtime.block_on(async move {
            let connection = self
                .endpoint
                .connect(addr, &alpn)
                .await
                .context("connect voice peer over iroh")?;
            let event = shared.insert_connection(connection.clone());
            let _ = shared.events.send(event.clone());
            spawn_datagram_task(connection, shared);
            let peer = match event {
                VoiceEvent::PeerConnected { peer } | VoiceEvent::PeerReplaced { peer } => peer,
                _ => unreachable!(),
            };
            Ok(peer)
        })
    }

    pub fn send_datagram(&self, peer: RemotePeer, bytes: Bytes) -> Result<()> {
        let connection = {
            let peers = self.shared.peers.lock().expect("peer map poisoned");
            peers.get(&peer.id).cloned()
        }
        .ok_or_else(|| anyhow!("unknown peer {}", peer.id))?;
        connection
            .send_datagram(bytes)
            .map_err(|err| anyhow!("send voice datagram to {}: {err}", peer.id))
    }

    /// Return a clone of the active `Connection` for `peer`, if one exists.
    /// `Connection` is internally reference-counted so cloning is cheap.
    pub fn get_connection(&self, peer: RemotePeer) -> Option<Connection> {
        let peers = self.shared.peers.lock().expect("peer map poisoned");
        peers.get(&peer.id).cloned()
    }

    pub fn peer_rtt(&self, peer: RemotePeer) -> Option<Duration> {
        let connection = {
            let peers = self.shared.peers.lock().expect("peer map poisoned");
            peers.get(&peer.id).cloned()
        }?;
        connection.rtt(PathId::ZERO)
    }

    /// Install a direct packet handler that is called from the iroh receive task
    /// instead of going through the broadcast channel. Connection events
    /// (peer connected/disconnected) still go through the broadcast.
    pub fn set_packet_handler(&self, handler: PacketHandler) {
        *self
            .shared
            .packet_handler
            .write()
            .expect("packet_handler lock poisoned") = Some(handler);
    }

    pub fn shutdown(&mut self) -> Result<()> {
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }
        if let Some(task) = self.accept_task.take() {
            self.runtime.block_on(async {
                let _ = task.await;
            });
        }
        self.runtime.block_on(self.endpoint.close());
        Ok(())
    }
}

impl Drop for VoiceIrohService {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

async fn run_accept_loop(
    endpoint: Endpoint,
    shared: Arc<SharedState>,
    mut stop_rx: oneshot::Receiver<()>,
) {
    loop {
        tokio::select! {
            _ = &mut stop_rx => break,
            incoming = endpoint.accept() => {
                let Some(incoming) = incoming else {
                    break;
                };
                let shared = shared.clone();
                tokio::spawn(async move {
                    let mut accepting = match incoming.accept() {
                        Ok(accepting) => accepting,
                        Err(_) => return,
                    };
                    let alpn = match accepting.alpn().await {
                        Ok(alpn) => alpn,
                        Err(_) => return,
                    };
                    if alpn != shared.alpn {
                        return;
                    }
                    let connection = match accepting.await {
                        Ok(connection) => connection,
                        Err(_) => return,
                    };
                    let event = shared.insert_connection(connection.clone());
                    let _ = shared.events.send(event);
                    spawn_datagram_task(connection, shared);
                });
            }
        }
    }
}

fn spawn_datagram_task(connection: Connection, shared: Arc<SharedState>) {
    tokio::spawn(async move {
        let peer = connection.remote_id();
        while let Ok(bytes) = connection.read_datagram().await {
            let received_at_mono_us = shared.now_mono_us();
            // Fast path: push directly to audio queue without going through the
            // broadcast channel or Godot's main-thread _process() cadence.
            let handler = shared
                .packet_handler
                .read()
                .expect("packet_handler lock poisoned")
                .clone();
            if let Some(ref h) = handler {
                h(RemotePeer { id: peer }, bytes.clone(), received_at_mono_us);
            }
            // Always broadcast so the GDScript packet_received signal and stats
            // counter still work regardless of whether a handler is installed.
            let _ = shared.events.send(VoiceEvent::PacketReceived {
                peer: RemotePeer { id: peer },
                bytes,
                received_at_mono_us,
            });
        }
        if let Some(event) = shared.remove_connection(peer) {
            let _ = shared.events.send(event);
        }
    });
}
