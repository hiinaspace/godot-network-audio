use godot::builtin::PackedByteArray;
use voice_core::{Result, VoicePacket};

pub fn decode_packet_bytes(bytes: &PackedByteArray) -> Result<VoicePacket> {
    VoicePacket::decode_from_bytes(&bytes.to_vec())
}

pub fn encode_packet_bytes(packet: &VoicePacket) -> PackedByteArray {
    PackedByteArray::from(packet.to_bytes().as_slice())
}
