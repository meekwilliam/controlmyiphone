//! the phone's screen as h.264 instead of hevc, every windows pc can decode h.264 while hevc needs a paid
//! microsoft store extension on many, idevice's offer lists both and the phone picks hevc, so this builds the same
//! offer device hub sends with only h.264 in it, and unpacks the rtp the phone then sends

use std::{
    io::Write,
    time::{SystemTime, UNIX_EPOCH},
};

use flate2::{Compression, write::ZlibEncoder};
use idevice::core_device::CallInfoBlob;

use crate::{Error, Result};

/// the rtp payload type the phone uses for h.264
const PAYLOAD_TYPE: u8 = 123;

/// the `negotiatorOffer` for a screen video stream with h.264 as its only codec, a copy of idevice's
/// `build_screen_video_offer` minus the hevc entry
pub fn offer(call_id: &str, call_info: &CallInfoBlob, ssrc: u32) -> Result<Vec<u8>> {
    let blob = media_blob(ssrc, ntp_now());
    let mut zlib = ZlibEncoder::new(Vec::new(), Compression::default());
    zlib.write_all(&blob)?;
    let blob = zlib.finish()?;

    let mut offer = plist::Dictionary::new();
    offer.insert("avcMediaStreamOptionCallID".into(), call_id.into());
    offer.insert("avcMediaStreamNegotiatorMediaBlob".into(), plist::Value::Data(blob));
    // 5 is coredevice screen sharing, the audio offer uses 6
    offer.insert("avcMediaStreamNegotiatorMode".into(), 5.into());
    offer.insert(
        "avcMediaStreamOptionRemoteEndpointInfo".into(),
        plist::Value::Data(call_info.encode()),
    );
    let mut out = Vec::new();
    plist::to_writer_binary(&mut out, &plist::Value::Dictionary(offer)).map_err(|e| Error::Other(e.to_string()))?;
    Ok(out)
}

/// the protobuf media blob, field numbers and values are what device hub sends, see idevice's negotiation.rs
fn media_blob(ssrc: u32, ntp_time: u64) -> Vec<u8> {
    fn format(e: &mut Proto, variant: u64) {
        e.message(2, |m| {
            m.uint(1, 1);
            m.uint(2, variant);
            m.uint(3, 0xc3c3);
            m.uint(4, 0);
        });
    }
    fn setting(e: &mut Proto, id: u64, value: u64, flags: Option<u64>) {
        e.message(9, |m| {
            m.uint(1, id);
            m.uint(2, value);
            if let Some(flags) = flags {
                m.uint(3, flags);
            }
        });
    }

    let mut e = Proto::default();
    e.uint(1, 1);
    e.uint(2, 1);
    e.message(5, |m| {
        m.uint(1, ssrc as u64);
        m.uint(2, 0);
        m.message(3, |c| {
            c.uint(1, PAYLOAD_TYPE as u64);
            format(c, 1);
            format(c, 2);
            format(c, 1);
            format(c, 2);
            c.bytes(3, b"FLS;SW:1;");
            c.uint(4, 1);
        });
        m.uint(7, 1);
        m.uint(8, 63);
        m.uint(12, 1);
    });
    e.bytes(6, b"Viceroy 1.7.0");
    e.uint(8, 0);
    setting(&mut e, 0, 20_000_000, Some(0x18000));
    setting(&mut e, 4074, 0, Some(0x4000));
    setting(&mut e, 0, 60_000_000, Some(0x40000));
    setting(&mut e, 0, 40_000_000, Some(0x3000));
    setting(&mut e, 16, 4100, None);
    setting(&mut e, 0, 6_000_000, Some(0x20000));
    setting(&mut e, 4, 6500, None);
    setting(&mut e, 0, 100_000_000, Some(0x100000));
    setting(&mut e, 1, 299, None);
    setting(&mut e, 0, 75_000_000, Some(0x80000));
    e.uint(13, ntp_time);
    e.uint(14, 2);
    e.uint(16, 0);
    e.uint(18, 1);
    e.0
}

fn ntp_now() -> u64 {
    // seconds between 1900, where ntp starts, and 1970
    const UNIX_TO_NTP: u64 = 2_208_988_800;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    ((now.as_secs() + UNIX_TO_NTP) << 32) | ((now.subsec_nanos() as u64) << 32) / 1_000_000_000
}

#[derive(Default)]
struct Proto(Vec<u8>);

impl Proto {
    fn varint(&mut self, mut v: u64) {
        while v >= 0x80 {
            self.0.push((v as u8) | 0x80);
            v >>= 7;
        }
        self.0.push(v as u8);
    }

    fn uint(&mut self, field: u64, v: u64) {
        self.varint(field << 3);
        self.varint(v);
    }

    fn bytes(&mut self, field: u64, v: &[u8]) {
        self.varint((field << 3) | 2);
        self.varint(v.len() as u64);
        self.0.extend_from_slice(v);
    }

    fn message(&mut self, field: u64, build: impl FnOnce(&mut Proto)) {
        let mut inner = Proto::default();
        build(&mut inner);
        self.bytes(field, &inner.0);
    }
}

/// turns the phone's h.264 rtp payloads back into annex-b, standard rfc 6184 packets plus one of apple's own,
/// nal type 18, which carries a quicktime `avc1` description with the sps and pps inside its `avcC` box
#[derive(Default)]
pub struct Depacketizer {
    out: Vec<u8>,
    fragment: Option<Vec<u8>>,
}

impl Depacketizer {
    pub fn push(&mut self, payload: &[u8]) {
        let Some(&header) = payload.first() else { return };
        match header & 0x1F {
            1..=23 => self.nal(payload),
            // stap-a, nal units each behind a 2 byte size
            24 => {
                let mut rest = &payload[1..];
                while rest.len() >= 2 {
                    let size = u16::from_be_bytes([rest[0], rest[1]]) as usize;
                    let Some(nal) = rest.get(2..2 + size) else { break };
                    self.nal(nal);
                    rest = &rest[2 + size..];
                }
            }
            // fu-a, one nal unit split over packets, the first carries its real type
            28 if payload.len() > 2 => {
                let fu = payload[1];
                if fu & 0x80 != 0 {
                    self.fragment = Some(vec![(header & 0xE0) | (fu & 0x1F)]);
                }
                if let Some(fragment) = self.fragment.as_mut() {
                    fragment.extend_from_slice(&payload[2..]);
                }
                if fu & 0x40 != 0
                    && let Some(fragment) = self.fragment.take()
                {
                    self.nal(&fragment);
                }
            }
            _ => {}
        }
    }

    /// the annex-b data since the last call
    pub fn take(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.out)
    }

    fn nal(&mut self, nal: &[u8]) {
        // apple's type 18 sits where the spec has a reserved type
        if nal[0] & 0x1F == 18 {
            for set in parameter_sets(nal) {
                self.nal(set);
            }
            return;
        }
        self.out.extend_from_slice(&[0, 0, 0, 1]);
        self.out.extend_from_slice(nal);
    }
}

/// the sps and pps from the `avcC` box inside apple's description packet
fn parameter_sets(packet: &[u8]) -> Vec<&[u8]> {
    let mut sets = Vec::new();
    let Some(at) = packet.windows(4).position(|w| w == b"avcC") else {
        return sets;
    };
    let avcc = &packet[at + 4..];
    let Some(&count) = avcc.get(5) else { return sets };
    let mut rest = &avcc[6..];
    // the sps count is in the low 5 bits, then one byte with the pps count
    let mut remaining = (count & 0x1F) as usize;
    let mut pps_next = true;
    loop {
        while remaining > 0 {
            if rest.len() < 2 {
                return sets;
            }
            let size = u16::from_be_bytes([rest[0], rest[1]]) as usize;
            let Some(set) = rest.get(2..2 + size) else { return sets };
            sets.push(set);
            rest = &rest[2 + size..];
            remaining -= 1;
        }
        if !pps_next || rest.is_empty() {
            return sets;
        }
        remaining = rest[0] as usize;
        rest = &rest[1..];
        pps_next = false;
    }
}

/// whether an annex-b access unit holds an idr picture
pub fn has_key_frame(data: &[u8]) -> bool {
    data.windows(4).any(|w| w[..3] == [0, 0, 1] && w[3] & 0x1F == 5)
}
