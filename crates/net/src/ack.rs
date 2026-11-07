//! Packet sequence numbers and acknowledgement bitfields.

/// True if sequence number `a` is newer than `b`, allowing for wraparound.
pub fn newer(a: u32, b: u32) -> bool {
    a != b && a.wrapping_sub(b) < u32::MAX / 2
}

/// Which of the other side's packets have arrived: the newest, plus a bit for each of the 32 before it.
#[derive(Clone, Copy, Debug, Default)]
pub struct Received {
    latest: u32,
    bits: u32,
    any: bool,
}

impl Received {
    /// Records packet `seq`. Returns false for a duplicate or one too old to track (drop it).
    pub fn record(&mut self, seq: u32) -> bool {
        if !self.any {
            *self = Received { latest: seq, bits: 0, any: true };
            return true;
        }
        if newer(seq, self.latest) {
            let shift = seq.wrapping_sub(self.latest);
            let previous = if shift <= 32 { 1u32 << (shift - 1) } else { 0 };
            self.bits = self.bits.checked_shl(shift).unwrap_or(0) | previous;
            self.latest = seq;
            return true;
        }
        let back = self.latest.wrapping_sub(seq);
        if back == 0 || back > 32 {
            return false;
        }
        let mask = 1u32 << (back - 1);
        if self.bits & mask != 0 {
            return false;
        }
        self.bits |= mask;
        true
    }

    /// (ack, ack_bits) for the next outgoing header.
    pub fn ack(&self) -> (u32, u32) {
        (self.latest, self.bits)
    }
}

/// Calls `f` with every packet number an (ack, ack_bits) pair acknowledges.
pub fn for_each_acked(ack: u32, bits: u32, mut f: impl FnMut(u32)) {
    f(ack);
    for i in 0..32 {
        if bits >> i & 1 == 1 {
            f(ack.wrapping_sub(i + 1));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracks_out_of_order_and_duplicates() {
        let mut r = Received::default();
        assert!(r.record(10));
        assert!(r.record(12));
        assert!(r.record(11));
        assert!(!r.record(11));
        assert!(!r.record(12));
        let mut acked = Vec::new();
        let (ack, bits) = r.ack();
        for_each_acked(ack, bits, |s| acked.push(s));
        assert_eq!(acked, vec![12, 11, 10]);
    }

    #[test]
    fn handles_wraparound_and_big_jumps() {
        let mut r = Received::default();
        assert!(r.record(u32::MAX));
        assert!(r.record(1));
        assert!(newer(1, u32::MAX));
        let mut acked = Vec::new();
        let (ack, bits) = r.ack();
        for_each_acked(ack, bits, |s| acked.push(s));
        assert_eq!(acked, vec![1, u32::MAX]);
        assert!(r.record(100));
        assert_eq!(r.ack(), (100, 0));
    }
}
