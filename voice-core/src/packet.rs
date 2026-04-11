use crate::error::{Error, Result};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PacketFlags(u8);

impl PacketFlags {
    pub const START_OF_TALKSPURT: u8 = 1 << 0;
    pub const END_OF_TALKSPURT: u8 = 1 << 1;
    pub const FEC_INCLUDED: u8 = 1 << 2;

    pub fn from_bits(bits: u8) -> Self {
        Self(bits)
    }

    pub fn bits(self) -> u8 {
        self.0
    }

    pub fn contains(self, bit: u8) -> bool {
        (self.0 & bit) != 0
    }

    pub fn set(&mut self, bit: u8, enabled: bool) {
        if enabled {
            self.0 |= bit;
        } else {
            self.0 &= !bit;
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoicePacket {
    pub seq: u16,
    pub timestamp: u32,
    pub flags: PacketFlags,
    pub payload: Vec<u8>,
}

impl VoicePacket {
    pub const HEADER_LEN: usize = 8;

    pub fn encode_to_bytes(&self, buf: &mut Vec<u8>) {
        buf.reserve(Self::HEADER_LEN + self.payload.len());
        buf.extend_from_slice(&self.seq.to_be_bytes());
        buf.extend_from_slice(&self.timestamp.to_be_bytes());
        buf.push(self.flags.bits());
        buf.push(0);
        buf.extend_from_slice(&self.payload);
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(Self::HEADER_LEN + self.payload.len());
        self.encode_to_bytes(&mut out);
        out
    }

    pub fn decode_from_bytes(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < Self::HEADER_LEN {
            return Err(Error::InvalidPacket("packet shorter than fixed header"));
        }

        let seq = u16::from_be_bytes([bytes[0], bytes[1]]);
        let timestamp = u32::from_be_bytes([bytes[2], bytes[3], bytes[4], bytes[5]]);
        let flags = PacketFlags::from_bits(bytes[6]);
        let payload = bytes[Self::HEADER_LEN..].to_vec();

        Ok(Self {
            seq,
            timestamp,
            flags,
            payload,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PacketArrival {
    pub received_at_mono_us: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_packet_bytes() {
        let pkt = VoicePacket {
            seq: 42,
            timestamp: 960,
            flags: PacketFlags::from_bits(PacketFlags::START_OF_TALKSPURT),
            payload: vec![1, 2, 3, 4],
        };

        let decoded = VoicePacket::decode_from_bytes(&pkt.to_bytes()).unwrap();
        assert_eq!(decoded, pkt);
    }
}
