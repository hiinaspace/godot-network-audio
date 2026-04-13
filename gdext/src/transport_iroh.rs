use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;

use bytes::Bytes;
use godot::builtin::{Array, GString, PackedByteArray, VarDictionary, Variant};
use godot::classes::{INode, Node};
use godot::obj::WithBaseField;
use godot::prelude::*;
use godot_network_audio_iroh::{RemotePeer, VoiceEvent, VoiceIrohService};
use iroh::{EndpointAddr, EndpointId, RelayUrl};
use iroh_base::TransportAddr;
use tokio::sync::broadcast;
use voice_core::VoicePacket;

use crate::stream::{AudioStreamNetwork, LoopbackTarget};

#[derive(GodotClass)]
#[class(base=Node)]
pub struct IrohVoiceTransport {
    base: Base<Node>,
    service: Option<VoiceIrohService>,
    receiver: Option<broadcast::Receiver<VoiceEvent>>,
    receive_sink: Option<LoopbackTarget>,
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
            receive_sink: None,
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
        self.receive_sink = None;
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
        match VoiceIrohService::bind_default() {
            Ok(service) => {
                self.receiver = Some(service.subscribe());
                self.service = Some(service);
                self.maybe_install_packet_handler();
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
            Ok(_peer) => true,
            Err(err) => {
                self.record_error(format!("connect voice peer: {err:#}"));
                false
            }
        }
    }

    /// Attach an AudioStreamNetwork as the receive target. Packets are pushed
    /// directly into its internal queue from the iroh receive thread, bypassing
    /// Godot's _process() cadence entirely.
    #[func]
    fn set_receive_stream(&mut self, stream: Gd<AudioStreamNetwork>) {
        self.receive_sink = Some(stream.bind().loopback_target());
        self.maybe_install_packet_handler();
        self.enable_processing();
    }

    #[func]
    fn clear_receive_stream(&mut self) {
        self.receive_sink = None;
        // Handler stays installed in the service until a new one is set or the
        // service shuts down — harmless, it will just decode and drop packets.
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
        dict.set("packets_sent", self.packets_sent);
        dict.set("packets_received", self.packets_received);
        dict.set("peers_connected", self.peers_connected);
        dict.set("last_error", &self.last_error);
        dict
    }

    fn enable_processing(&mut self) {
        if !self.process_mode_enabled {
            self.base_mut().set_process(true);
            self.process_mode_enabled = true;
        }
    }

    fn maybe_install_packet_handler(&mut self) {
        let (Some(service), Some(sink)) = (self.service.as_ref(), self.receive_sink.as_ref()) else {
            return;
        };
        let sink = sink.clone();
        service.set_packet_handler(Arc::new(move |bytes: Bytes, received_at_mono_us: u64| {
            if let Ok(packet) = VoicePacket::decode_from_bytes(&bytes) {
                sink.enqueue_with_timestamp(packet, received_at_mono_us);
            }
        }));
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
                self.base_mut()
                    .emit_signal("peer_connected", &[peer.id.to_string().to_variant()]);
            }
            VoiceEvent::PeerReplaced { peer } => {
                self.base_mut()
                    .emit_signal("peer_replaced", &[peer.id.to_string().to_variant()]);
            }
            VoiceEvent::PeerDisconnected { peer } => {
                self.peers_connected = self.peers_connected.saturating_sub(1);
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
