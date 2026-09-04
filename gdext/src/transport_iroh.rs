use std::collections::{HashMap, HashSet, VecDeque};
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use bytes::Bytes;
use godot::builtin::{Array, GString, PackedByteArray, VarDictionary, Variant};
use godot::classes::{INode, Node};
use godot::obj::WithBaseField;
use godot::prelude::*;
use godot_network_audio_iroh::{RemotePeer, VoiceEvent, VoiceIrohConfig, VoiceIrohService};
use iroh::endpoint::Connection;
use iroh::{EndpointAddr, EndpointId, RelayUrl};
use iroh_base::TransportAddr;
use tokio::sync::broadcast;
use voice_core::VoicePacket;

use crate::sender::NetworkAudioSender;
use crate::stream::{AudioStreamNetwork, LoopbackTarget};

const PENDING_PACKETS_PER_PEER: usize = 64;

#[derive(Default)]
struct SendRouter {
    connections: HashMap<EndpointId, Connection>,
    selected_peers: Option<HashSet<EndpointId>>,
}

impl SendRouter {
    fn targets(&self) -> Vec<Connection> {
        self.connections
            .iter()
            .filter(|(peer, _)| {
                self.selected_peers
                    .as_ref()
                    .is_none_or(|selected| selected.contains(*peer))
            })
            .map(|(_, connection)| connection.clone())
            .collect()
    }
}

struct PendingPacket {
    packet: VoicePacket,
    received_at_mono_us: u64,
}

#[derive(Default)]
struct PeerIngress {
    sink: Option<LoopbackTarget>,
    pending: VecDeque<PendingPacket>,
}

#[derive(Default)]
struct ReceiveRouter {
    peers: RwLock<HashMap<EndpointId, PeerIngress>>,
    default_sink: RwLock<Option<LoopbackTarget>>,
    pending_drops: AtomicU64,
}

impl ReceiveRouter {
    fn route(&self, peer: EndpointId, packet: VoicePacket, received_at_mono_us: u64) {
        let default_sink = self
            .default_sink
            .read()
            .expect("default receive sink poisoned")
            .clone();
        let sink = {
            let mut peers = self.peers.write().expect("receive router poisoned");
            let ingress = peers.entry(peer).or_default();
            if let Some(sink) = default_sink.clone().or_else(|| ingress.sink.clone()) {
                Some(sink)
            } else {
                if ingress.pending.len() == PENDING_PACKETS_PER_PEER {
                    ingress.pending.pop_front();
                    self.pending_drops.fetch_add(1, Ordering::Relaxed);
                }
                ingress.pending.push_back(PendingPacket {
                    packet: packet.clone(),
                    received_at_mono_us,
                });
                None
            }
        };
        if let Some(sink) = sink {
            sink.enqueue_with_timestamp(packet, received_at_mono_us);
        }
    }

    fn register(&self, peer: EndpointId, sink: LoopbackTarget) {
        let pending = {
            let mut peers = self.peers.write().expect("receive router poisoned");
            let ingress = peers.entry(peer).or_default();
            ingress.sink = Some(sink.clone());
            std::mem::take(&mut ingress.pending)
        };
        for queued in pending {
            sink.enqueue_with_timestamp(queued.packet, queued.received_at_mono_us);
        }
    }

    fn unregister(&self, peer: EndpointId) {
        self.peers
            .write()
            .expect("receive router poisoned")
            .remove(&peer);
    }

    fn pending_packet_count(&self) -> usize {
        self.peers
            .read()
            .expect("receive router poisoned")
            .values()
            .map(|ingress| ingress.pending.len())
            .sum()
    }
}

#[derive(GodotClass)]
#[class(base=Node)]
pub struct IrohVoiceTransport {
    base: Base<Node>,
    service: Option<VoiceIrohService>,
    receiver: Option<broadcast::Receiver<VoiceEvent>>,
    receive_streams: HashMap<EndpointId, Gd<AudioStreamNetwork>>,
    receive_router: Arc<ReceiveRouter>,
    send_router: Arc<RwLock<SendRouter>>,
    direct_packets_sent: Arc<AtomicU64>,
    direct_send_errors: Arc<AtomicU64>,
    process_mode_enabled: bool,
    last_error: GString,
    packets_sent: i64,
    packets_received: i64,
    peers_connected: i64,
}

#[godot_api]
impl INode for IrohVoiceTransport {
    fn init(base: Base<Node>) -> Self {
        Self {
            base,
            service: None,
            receiver: None,
            receive_streams: HashMap::new(),
            receive_router: Arc::new(ReceiveRouter::default()),
            send_router: Arc::new(RwLock::new(SendRouter::default())),
            direct_packets_sent: Arc::new(AtomicU64::new(0)),
            direct_send_errors: Arc::new(AtomicU64::new(0)),
            process_mode_enabled: false,
            last_error: GString::new(),
            packets_sent: 0,
            packets_received: 0,
            peers_connected: 0,
        }
    }

    fn process(&mut self, _delta: f64) {
        self.drain_events();
    }

    fn exit_tree(&mut self) {
        self.receiver = None;
        self.receive_streams.clear();
        self.service = None;
    }
}

#[godot_api]
impl IrohVoiceTransport {
    #[signal]
    fn transport_error(message: GString);

    #[signal]
    fn peer_connected(peer_id: GString);

    #[signal]
    fn peer_replaced(peer_id: GString);

    #[signal]
    fn peer_disconnected(peer_id: GString);

    #[signal]
    fn packet_received(peer_id: GString, bytes: PackedByteArray, received_at_mono_us: i64);

    #[func]
    fn start_endpoint(&mut self) -> bool {
        // If GNA_IROH_BIND_ADDR is set, bind to that specific address in local-only
        // mode (no relay, no pkarr/DNS).  This is used for netem impairment tests
        // over a veth pair where the relay would bypass traffic shaping.
        let config = match std::env::var("GNA_IROH_BIND_ADDR") {
            Ok(addr_str) if !addr_str.is_empty() => {
                match addr_str.parse::<std::net::SocketAddr>() {
                    Ok(addr) => VoiceIrohConfig {
                        bind_addr: Some(addr),
                        relay: false,
                        ..Default::default()
                    },
                    Err(err) => {
                        self.record_error(format!(
                            "GNA_IROH_BIND_ADDR={addr_str:?} parse error: {err}"
                        ));
                        return false;
                    }
                }
            }
            _ => VoiceIrohConfig::default(),
        };
        match VoiceIrohService::bind(config) {
            Ok(service) => {
                self.receiver = Some(service.subscribe());
                self.service = Some(service);
                self.install_packet_router();
                self.enable_processing();
                true
            }
            Err(err) => {
                self.record_error(format!("start endpoint: {err:#}"));
                false
            }
        }
    }

    #[func]
    fn local_endpoint_info(&self) -> VarDictionary {
        let mut dict = VarDictionary::new();
        if let Some(service) = self.service.as_ref() {
            let addr = service.endpoint_addr();
            dict = endpoint_addr_to_dict(&addr);
        }
        dict
    }

    #[func]
    fn connect_to_peer(&mut self, info: VarDictionary) -> bool {
        let Some(service) = self.service.as_ref() else {
            self.record_error("connect_to_peer called before start_endpoint".to_string());
            return false;
        };
        let addr = match dict_to_endpoint_addr(&info) {
            Ok(addr) => addr,
            Err(err) => {
                self.record_error(format!("parse remote endpoint info: {err:#}"));
                return false;
            }
        };
        match service.connect(addr) {
            Ok(peer) => {
                if let Some(conn) = service.get_connection(peer) {
                    self.send_router
                        .write()
                        .expect("send router poisoned")
                        .connections
                        .entry(peer.id)
                        .or_insert(conn);
                }
                self.ensure_receive_stream_for(peer.id);
                true
            }
            Err(err) => {
                self.record_error(format!("connect voice peer: {err:#}"));
                false
            }
        }
    }

    /// Wire encoded output directly to all connected peers, or the subset set
    /// with `set_send_peers`. This never depends on Godot's `_process()` cadence.
    #[func]
    fn attach_sender(&mut self, mut sender: Gd<NetworkAudioSender>) {
        let router = self.send_router.clone();
        let sent = self.direct_packets_sent.clone();
        let errors = self.direct_send_errors.clone();
        let handler = Arc::new(move |bytes: Vec<u8>| {
            let targets = router.read().expect("send router poisoned").targets();
            let bytes = Bytes::from(bytes);
            for connection in targets {
                match connection.send_datagram(bytes.clone()) {
                    Ok(()) => {
                        sent.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(_) => {
                        errors.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        });
        sender.bind_mut().install_direct_send_handler(handler);
    }

    /// Restrict direct sender output to `peer_ids`. An empty array intentionally
    /// sends to no peers. Call `send_to_all_peers` to remove the filter.
    #[func]
    fn set_send_peers(&mut self, peer_ids: Array<GString>) -> bool {
        let mut selected = HashSet::new();
        for peer_id in peer_ids.iter_shared() {
            let Ok(peer) = EndpointId::from_str(peer_id.to_string().as_str()) else {
                self.record_error(format!("invalid send peer id: {peer_id}"));
                return false;
            };
            selected.insert(peer);
        }
        self.send_router
            .write()
            .expect("send router poisoned")
            .selected_peers = Some(selected);
        true
    }

    #[func]
    fn send_to_all_peers(&mut self) {
        self.send_router
            .write()
            .expect("send router poisoned")
            .selected_peers = None;
    }

    /// Legacy single-stream route. Multi-peer games should call
    /// `get_or_create_receive_stream(peer_id)` and assign one stream to each
    /// AudioStreamPlayer/AudioStreamPlayer3D.
    #[func]
    fn set_receive_stream(&mut self, stream: Gd<AudioStreamNetwork>) {
        *self
            .receive_router
            .default_sink
            .write()
            .expect("default receive sink poisoned") = Some(stream.bind().loopback_target());
        self.enable_processing();
    }

    #[func]
    fn clear_receive_stream(&mut self) {
        *self
            .receive_router
            .default_sink
            .write()
            .expect("default receive sink poisoned") = None;
    }

    /// Return the stable stream resource for `peer_id`, creating it when first
    /// requested. Incoming Iroh packets are routed directly into this stream's
    /// bounded queue from the network thread; each playback owns its own NetEq.
    #[func]
    fn get_or_create_receive_stream(&mut self, peer_id: GString) -> Option<Gd<AudioStreamNetwork>> {
        let peer = match EndpointId::from_str(peer_id.to_string().as_str()) {
            Ok(peer) => peer,
            Err(err) => {
                self.record_error(format!("invalid receive peer id: {err}"));
                return None;
            }
        };
        Some(self.ensure_receive_stream_for(peer))
    }

    #[func]
    fn remove_receive_stream(&mut self, peer_id: GString) -> bool {
        let Ok(peer) = EndpointId::from_str(peer_id.to_string().as_str()) else {
            self.record_error(format!("invalid receive peer id: {peer_id}"));
            return false;
        };
        self.receive_router.unregister(peer);
        self.receive_streams.remove(&peer).is_some()
    }

    #[func]
    fn receive_stream_count(&self) -> i64 {
        self.receive_streams.len() as i64
    }

    #[func]
    fn send_packet(&mut self, peer_id: GString, bytes: PackedByteArray) -> bool {
        let Some(service) = self.service.as_ref() else {
            self.record_error("send_packet called before start_endpoint".to_string());
            return false;
        };
        let peer = match EndpointId::from_str(peer_id.to_string().as_str()) {
            Ok(id) => RemotePeer { id },
            Err(err) => {
                self.record_error(format!("invalid peer id: {err}"));
                return false;
            }
        };
        let bytes = Bytes::from(bytes.to_vec());
        match service.send_datagram(peer, bytes) {
            Ok(()) => {
                self.packets_sent += 1;
                true
            }
            Err(err) => {
                self.record_error(format!("send packet: {err:#}"));
                false
            }
        }
    }

    #[func]
    fn get_stats(&self) -> VarDictionary {
        let mut dict = VarDictionary::new();
        dict.set(
            "packets_sent",
            self.packets_sent + self.direct_packets_sent.load(Ordering::Relaxed) as i64,
        );
        dict.set(
            "send_errors",
            self.direct_send_errors.load(Ordering::Relaxed) as i64,
        );
        dict.set("packets_received", self.packets_received);
        dict.set("peers_connected", self.peers_connected);
        dict.set("receive_streams", self.receive_streams.len() as i64);
        dict.set(
            "pending_receive_packets",
            self.receive_router.pending_packet_count() as i64,
        );
        dict.set(
            "pending_receive_drops",
            self.receive_router.pending_drops.load(Ordering::Relaxed) as i64,
        );
        dict.set("last_error", &self.last_error);
        dict
    }

    fn enable_processing(&mut self) {
        if !self.process_mode_enabled {
            self.base_mut().set_process(true);
            self.process_mode_enabled = true;
        }
    }

    fn install_packet_router(&mut self) {
        let Some(service) = self.service.as_ref() else {
            return;
        };
        let router = self.receive_router.clone();
        service.set_packet_handler(Arc::new(
            move |peer, bytes: Bytes, received_at_mono_us: u64| {
                if let Ok(packet) = VoicePacket::decode_from_bytes(&bytes) {
                    router.route(peer.id, packet, received_at_mono_us);
                }
            },
        ));
    }

    fn ensure_receive_stream_for(&mut self, peer: EndpointId) -> Gd<AudioStreamNetwork> {
        if let Some(stream) = self.receive_streams.get(&peer) {
            return stream.clone();
        }
        let stream = AudioStreamNetwork::new_gd();
        self.receive_router
            .register(peer, stream.bind().loopback_target());
        self.receive_streams.insert(peer, stream.clone());
        stream
    }

    fn drain_events(&mut self) {
        let Some(receiver) = self.receiver.as_mut() else {
            return;
        };
        let mut events = Vec::new();
        loop {
            match receiver.try_recv() {
                Ok(event) => events.push(event),
                Err(broadcast::error::TryRecvError::Empty) => break,
                Err(broadcast::error::TryRecvError::Closed) => break,
                Err(broadcast::error::TryRecvError::Lagged(_)) => continue,
            }
        }
        for event in events {
            self.handle_event(event);
        }
    }

    fn handle_event(&mut self, event: VoiceEvent) {
        match event {
            VoiceEvent::PeerConnected { peer } => {
                self.peers_connected += 1;
                if let Some(service) = self.service.as_ref() {
                    if let Some(conn) = service.get_connection(peer) {
                        self.send_router
                            .write()
                            .expect("send router poisoned")
                            .connections
                            .insert(peer.id, conn);
                    }
                }
                self.ensure_receive_stream_for(peer.id);
                self.base_mut()
                    .emit_signal("peer_connected", &[peer.id.to_string().to_variant()]);
            }
            VoiceEvent::PeerReplaced { peer } => {
                if let Some(service) = self.service.as_ref() {
                    if let Some(conn) = service.get_connection(peer) {
                        self.send_router
                            .write()
                            .expect("send router poisoned")
                            .connections
                            .insert(peer.id, conn);
                    }
                }
                self.ensure_receive_stream_for(peer.id);
                self.base_mut()
                    .emit_signal("peer_replaced", &[peer.id.to_string().to_variant()]);
            }
            VoiceEvent::PeerDisconnected { peer } => {
                self.peers_connected = self.peers_connected.saturating_sub(1);
                self.send_router
                    .write()
                    .expect("send router poisoned")
                    .connections
                    .remove(&peer.id);
                self.base_mut()
                    .emit_signal("peer_disconnected", &[peer.id.to_string().to_variant()]);
            }
            VoiceEvent::PacketReceived {
                peer,
                bytes,
                received_at_mono_us,
            } => {
                // Packet was already pushed to the audio queue by the direct handler
                // in the iroh receive thread. Here we only update stats and emit the
                // informational GDScript signal.
                self.packets_received += 1;
                let packed = PackedByteArray::from(bytes.as_ref());
                self.base_mut().emit_signal(
                    "packet_received",
                    &[
                        peer.id.to_string().to_variant(),
                        packed.to_variant(),
                        (received_at_mono_us as i64).to_variant(),
                    ],
                );
            }
        }
    }

    fn record_error(&mut self, message: String) {
        self.last_error = GString::from(message.as_str());
        let error_variant = self.last_error.to_variant();
        self.base_mut()
            .emit_signal("transport_error", &[error_variant]);
    }
}

fn endpoint_addr_to_dict(addr: &EndpointAddr) -> VarDictionary {
    let mut dict = VarDictionary::new();
    let ip_addrs = Array::from_iter(
        addr.ip_addrs()
            .map(|a| GString::from(a.to_string().as_str())),
    );
    let relay_urls = Array::from_iter(
        addr.relay_urls()
            .map(|u| GString::from(u.to_string().as_str())),
    );
    dict.set("endpoint_id", addr.id.to_string());
    dict.set("ip_addrs", &ip_addrs);
    dict.set("relay_urls", &relay_urls);
    dict
}

fn dict_to_endpoint_addr(info: &VarDictionary) -> Result<EndpointAddr, anyhow::Error> {
    let endpoint_id = info.get_or_nil("endpoint_id").to::<GString>();
    let endpoint_id = EndpointId::from_str(endpoint_id.to_string().as_str())?;

    let mut addrs = Vec::new();
    let ip_addrs = info.get_or_nil("ip_addrs").to::<Array<Variant>>();
    for addr in ip_addrs.iter_shared() {
        let addr = addr.to::<GString>();
        let socket = SocketAddr::from_str(addr.to_string().as_str())?;
        addrs.push(TransportAddr::Ip(socket));
    }
    let relay_urls = info.get_or_nil("relay_urls").to::<Array<Variant>>();
    for relay in relay_urls.iter_shared() {
        let relay = relay.to::<GString>();
        let url = RelayUrl::from_str(relay.to_string().as_str())?;
        addrs.push(TransportAddr::Relay(url));
    }

    Ok(EndpointAddr::from_parts(endpoint_id, addrs))
}
