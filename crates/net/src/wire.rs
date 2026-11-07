//! Packet layout and byte-level encoding.
//!
//! Every packet starts with the same header:
//!
//! | field | size | |
//! |---|---|---|
//! | magic | u32 | `HRD1` |
//! | version | u16 | [`VERSION`] |
//! | kind | u8 | [`Kind`] |
//! | seq | u32 | sender's packet number |
//! | ack | u32 | newest packet number received from the other side |
//! | ack_bits | u32 | bit `i` set: packet `ack - 1 - i` was received too |
//!
//! All integers are little-endian.

use crate::{EntityState, PlayerInput};
use std::f32::consts::TAU;

pub const MAGIC: u32 = u32::from_le_bytes(*b"HRD1");
pub const VERSION: u16 = 1;
/// Largest datagram either side sends; stays under a typical 1500-byte Ethernet MTU.
pub const MAX_PACKET: usize = 1200;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    /// client → server: player name (u8 length + bytes).
    Connect = 1,
    /// server → client: client id u16, controlled entity u32 (u32::MAX if none), tick rate u16,
    /// world size f32.
    Accept = 2,
    /// server → client: reason u8.
    Reject = 3,
    /// client → server: newest input seq u32, count u8, then `count` inputs, newest first.
    Input = 4,
    /// server → client: server tick u32, newest input applied u32, record count u16, then records.
    Snapshot = 5,
    /// either way: goodbye.
    Disconnect = 6,
}

impl Kind {
    fn from_u8(v: u8) -> Option<Kind> {
        Some(match v {
            1 => Kind::Connect,
            2 => Kind::Accept,
            3 => Kind::Reject,
            4 => Kind::Input,
            5 => Kind::Snapshot,
            6 => Kind::Disconnect,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Header {
    pub kind: Kind,
    pub seq: u32,
    pub ack: u32,
    pub ack_bits: u32,
}

pub fn write_header(buf: &mut Vec<u8>, h: &Header) {
    buf.clear();
    buf.extend_from_slice(&MAGIC.to_le_bytes());
    buf.extend_from_slice(&VERSION.to_le_bytes());
    buf.push(h.kind as u8);
    buf.extend_from_slice(&h.seq.to_le_bytes());
    buf.extend_from_slice(&h.ack.to_le_bytes());
    buf.extend_from_slice(&h.ack_bits.to_le_bytes());
}

/// Reads and checks a header; `None` for anything that isn't a packet of this protocol and version.
pub fn read_header(r: &mut Reader<'_>) -> Option<Header> {
    if r.u32()? != MAGIC || r.u16()? != VERSION {
        return None;
    }
    let kind = Kind::from_u8(r.u8()?)?;
    Some(Header { kind, seq: r.u32()?, ack: r.u32()?, ack_bits: r.u32()? })
}

pub fn put_u16(buf: &mut Vec<u8>, v: u16) {
    buf.extend_from_slice(&v.to_le_bytes());
}

pub fn put_u32(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_le_bytes());
}

pub fn put_f32(buf: &mut Vec<u8>, v: f32) {
    buf.extend_from_slice(&v.to_le_bytes());
}

/// LEB128: 7 bits per byte, low bits first.
pub fn put_varint(buf: &mut Vec<u8>, mut v: u32) {
    while v >= 0x80 {
        buf.push(v as u8 | 0x80);
        v >>= 7;
    }
    buf.push(v as u8);
}

pub fn varint_len(v: u32) -> usize {
    match v {
        0..0x80 => 1,
        0x80..0x4000 => 2,
        0x4000..0x20_0000 => 3,
        0x20_0000..0x1000_0000 => 4,
        _ => 5,
    }
}

/// Reads from a received datagram; every read returns `None` past the end, so malformed packets can't
/// panic the reader.
pub struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Reader<'a> {
        Reader { data, pos: 0 }
    }

    pub fn bytes(&mut self, n: usize) -> Option<&'a [u8]> {
        let out = self.data.get(self.pos..self.pos.checked_add(n)?)?;
        self.pos += n;
        Some(out)
    }

    pub fn u8(&mut self) -> Option<u8> {
        Some(self.bytes(1)?[0])
    }

    pub fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes(self.bytes(2)?.try_into().ok()?))
    }

    pub fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.bytes(4)?.try_into().ok()?))
    }

    pub fn f32(&mut self) -> Option<f32> {
        Some(f32::from_le_bytes(self.bytes(4)?.try_into().ok()?))
    }

    pub fn varint(&mut self) -> Option<u32> {
        let mut v = 0u32;
        for shift in (0..35).step_by(7) {
            let b = self.u8()?;
            v |= u32::from(b & 0x7F).checked_shl(shift)?;
            if b & 0x80 == 0 {
                return Some(v);
            }
        }
        None
    }
}

/// An entity's state as sent on the wire: positions to 1/65,535 of the world (1.6 cm on a 1 km map),
/// facing to 1/256 of a turn, health to 1/255.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Quantized {
    pub x: u16,
    pub z: u16,
    pub yaw: u8,
    /// bit 0 alive, bits 1–2 team, bits 3–7 kind.
    pub flags: u8,
    pub health: u8,
}

/// Bytes a record takes after its entity-id varint.
pub const RECORD_BODY: usize = 7;

impl Quantized {
    pub fn from_state(s: &EntityState, world: f32) -> Quantized {
        let pos = |v: f32| ((v / world).clamp(0.0, 1.0) * 65535.0).round() as u16;
        Quantized {
            x: pos(s.x),
            z: pos(s.z),
            yaw: ((s.yaw.rem_euclid(TAU) / TAU * 256.0).round() as u32 & 0xFF) as u8,
            flags: u8::from(s.alive) | (s.team & 3) << 1 | (s.kind & 31) << 3,
            health: (s.health.clamp(0.0, 1.0) * 255.0).round() as u8,
        }
    }

    pub fn to_state(self, world: f32) -> EntityState {
        EntityState {
            x: f32::from(self.x) / 65535.0 * world,
            z: f32::from(self.z) / 65535.0 * world,
            yaw: f32::from(self.yaw) / 256.0 * TAU,
            team: self.flags >> 1 & 3,
            kind: self.flags >> 3,
            alive: self.flags & 1 == 1,
            health: f32::from(self.health) / 255.0,
        }
    }

    pub fn write(&self, buf: &mut Vec<u8>) {
        put_u16(buf, self.x);
        put_u16(buf, self.z);
        buf.extend_from_slice(&[self.yaw, self.flags, self.health]);
    }

    pub fn read(r: &mut Reader<'_>) -> Option<Quantized> {
        Some(Quantized { x: r.u16()?, z: r.u16()?, yaw: r.u8()?, flags: r.u8()?, health: r.u8()? })
    }
}

pub fn write_input(buf: &mut Vec<u8>, input: &PlayerInput) {
    let axis = |v: f32| (v.clamp(-1.0, 1.0) * 127.0).round() as i8 as u8;
    buf.push(axis(input.move_x));
    buf.push(axis(input.move_z));
    put_u16(buf, (input.yaw.rem_euclid(TAU) / TAU * 65536.0).round() as u32 as u16);
    buf.push(u8::from(input.fire));
}

pub fn read_input(r: &mut Reader<'_>) -> Option<PlayerInput> {
    let axis = |b: u8| f32::from(b as i8) / 127.0;
    Some(PlayerInput {
        move_x: axis(r.u8()?),
        move_z: axis(r.u8()?),
        yaw: f32::from(r.u16()?) / 65536.0 * TAU,
        fire: r.u8()? & 1 == 1,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varints_round_trip() {
        for v in [0, 1, 127, 128, 16_383, 16_384, 2_097_151, 2_097_152, u32::MAX] {
            let mut buf = Vec::new();
            put_varint(&mut buf, v);
            assert_eq!(buf.len(), varint_len(v));
            assert_eq!(Reader::new(&buf).varint(), Some(v));
        }
    }

    #[test]
    fn quantised_state_round_trips_within_precision() {
        let s = EntityState { x: 512.3, z: 1000.9, yaw: 4.0, team: 1, kind: 5, alive: true, health: 0.66 };
        let back = Quantized::from_state(&s, 1024.0).to_state(1024.0);
        assert!((back.x - s.x).abs() < 0.02 && (back.z - s.z).abs() < 0.02);
        assert!((back.yaw - s.yaw).abs() < TAU / 256.0);
        assert_eq!((back.team, back.kind, back.alive), (1, 5, true));
        assert!((back.health - s.health).abs() < 0.01);
    }

    #[test]
    fn truncated_packets_read_as_none() {
        let mut buf = Vec::new();
        write_header(&mut buf, &Header { kind: Kind::Snapshot, seq: 1, ack: 2, ack_bits: 3 });
        for len in 0..buf.len() {
            assert!(read_header(&mut Reader::new(&buf[..len])).is_none());
        }
        assert!(read_header(&mut Reader::new(&buf)).is_some());
    }
}
