//! Offline replay of captured traffic: an iterator over raw Ethernet frames read
//! from a pcap or pcapng file (auto-detected by magic bytes), for feeding recorded
//! captures back through the codecs without a live socket.

use std::fs::File;
use std::io::{Cursor, Read};
use std::path::Path;

use pcap_file::pcap::PcapReader;
use pcap_file::pcapng::PcapNgReader;
use thiserror::Error;

/// Errors from opening or reading a pcap/pcapng capture.
#[derive(Debug, Error)]
pub enum CaptureError {
    /// The underlying file/reader failed.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// The capture's own container format is malformed.
    #[error("capture parse error: {0}")]
    Pcap(#[from] pcap_file::PcapError),
    /// The first 4 bytes match neither a pcap nor a pcapng magic number.
    #[error("unknown capture format (magic {0:02x?})")]
    UnknownFormat([u8; 4]),
}

type Peeked<R> = std::io::Chain<Cursor<Vec<u8>>, R>;

enum Inner<R: Read> {
    Pcap(PcapReader<Peeked<R>>),
    PcapNg(PcapNgReader<Peeked<R>>),
}

/// Iterator over raw Ethernet frames from a pcap or pcapng source (auto-detected).
pub struct PcapFrames<R: Read> {
    inner: Inner<R>,
}

impl PcapFrames<File> {
    /// Opens `path` and auto-detects its pcap/pcapng format from the leading magic bytes.
    pub fn open(path: &Path) -> Result<Self, CaptureError> {
        Self::from_reader(File::open(path)?)
    }
}

impl<R: Read> PcapFrames<R> {
    /// Wraps any `Read` source, auto-detecting its pcap/pcapng format from the
    /// leading 4 magic bytes. [`CaptureError::UnknownFormat`] if neither matches.
    pub fn from_reader(mut r: R) -> Result<Self, CaptureError> {
        let mut magic = [0u8; 4];
        r.read_exact(&mut magic)?;
        let chained = Cursor::new(magic.to_vec()).chain(r);
        let inner = match magic {
            // pcapng Section Header Block type
            [0x0a, 0x0d, 0x0d, 0x0a] => Inner::PcapNg(PcapNgReader::new(chained)?),
            // classic pcap magics (LE/BE, us/ns precision)
            [0xd4, 0xc3, 0xb2, 0xa1]
            | [0xa1, 0xb2, 0xc3, 0xd4]
            | [0x4d, 0x3c, 0xb2, 0xa1]
            | [0xa1, 0xb2, 0x3c, 0x4d] => Inner::Pcap(PcapReader::new(chained)?),
            other => return Err(CaptureError::UnknownFormat(other)),
        };
        Ok(Self { inner })
    }
}

impl<R: Read> Iterator for PcapFrames<R> {
    type Item = Result<Vec<u8>, CaptureError>;

    fn next(&mut self) -> Option<Self::Item> {
        match &mut self.inner {
            Inner::Pcap(rd) => match rd.next_packet() {
                Some(Ok(pkt)) => Some(Ok(pkt.data.into_owned())),
                Some(Err(e)) => Some(Err(e.into())),
                None => None,
            },
            Inner::PcapNg(rd) => loop {
                match rd.next_block() {
                    Some(Ok(block)) => {
                        use pcap_file::pcapng::Block;
                        let data = match block {
                            Block::EnhancedPacket(b) => Some(b.data.into_owned()),
                            Block::SimplePacket(b) => Some(b.data.into_owned()),
                            Block::Packet(b) => Some(b.data.into_owned()),
                            _ => None, // skip section/interface/option blocks
                        };
                        if let Some(d) = data {
                            return Some(Ok(d));
                        }
                    }
                    Some(Err(e)) => return Some(Err(e.into())),
                    None => return None,
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pcap_file::pcap::{PcapPacket, PcapWriter};
    use std::io::Cursor;
    use std::time::Duration;

    fn make_pcap(frames: &[&[u8]]) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut w = PcapWriter::new(&mut buf).unwrap();
            for f in frames {
                w.write_packet(&PcapPacket::new(Duration::ZERO, f.len() as u32, f))
                    .unwrap();
            }
        }
        buf
    }

    fn make_pcapng(frames: &[&[u8]]) -> Vec<u8> {
        use pcap_file::pcapng::blocks::enhanced_packet::EnhancedPacketBlock;
        use pcap_file::pcapng::blocks::interface_description::InterfaceDescriptionBlock;
        use pcap_file::pcapng::PcapNgWriter;
        use pcap_file::DataLink;
        use std::borrow::Cow;

        let mut buf = Vec::new();
        {
            let mut w = PcapNgWriter::new(&mut buf).unwrap();
            let iface = InterfaceDescriptionBlock {
                linktype: DataLink::ETHERNET,
                snaplen: 0xFFFF,
                options: vec![],
            };
            w.write_pcapng_block(iface).unwrap();
            for f in frames {
                let pkt = EnhancedPacketBlock {
                    interface_id: 0,
                    timestamp: Duration::ZERO,
                    original_len: f.len() as u32,
                    data: Cow::Borrowed(f),
                    options: vec![],
                };
                w.write_pcapng_block(pkt).unwrap();
            }
        }
        buf
    }

    fn collect(bytes: Vec<u8>) -> Vec<Vec<u8>> {
        PcapFrames::from_reader(Cursor::new(bytes))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    #[test]
    fn reads_all_pcap_frames_in_order() {
        let frames = collect(make_pcap(&[&[0xaa, 0xbb], &[0xcc]]));
        assert_eq!(frames, vec![vec![0xaa, 0xbb], vec![0xcc]]);
    }

    #[test]
    fn reads_pcapng_frames_in_order() {
        let bytes = make_pcapng(&[&[0xaa, 0xbb], &[0xcc]]);
        let frames = collect(bytes);
        assert_eq!(frames, vec![vec![0xaa, 0xbb], vec![0xcc]]);
    }

    #[test]
    fn unknown_format_errors() {
        let err = PcapFrames::from_reader(Cursor::new(vec![0x00, 0x01, 0x02, 0x03, 0x04]));
        assert!(matches!(err, Err(CaptureError::UnknownFormat(_))));
    }
}
