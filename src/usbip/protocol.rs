//! USB/IP wire format (protocol version 1.1.1).

use std::io::{self, Read};

pub const PROTOCOL_VERSION: u16 = 0x0111;

pub const OP_REQ_IMPORT: u16 = 0x8003;
pub const OP_REP_IMPORT: u16 = 0x0003;
pub const OP_REQ_DEVLIST: u16 = 0x8005;
pub const OP_REP_DEVLIST: u16 = 0x0005;

pub const CMD_SUBMIT: u32 = 0x0000_0001;
pub const CMD_UNLINK: u32 = 0x0000_0002;
pub const RET_SUBMIT: u32 = 0x0000_0003;
pub const RET_UNLINK: u32 = 0x0000_0004;

pub const DIRECTION_OUT: u32 = 0;
pub const DIRECTION_IN: u32 = 1;

pub const SPEED_FULL: u32 = 2;

/// The USB/IP wire protocol uses 0xffffffff for non-isochronous URBs. It is
/// not an error and must never be interpreted as a negative count.
pub const NO_ISO_PACKETS: u32 = 0xffff_ffff;

pub const STATUS_OK: i32 = 0;
#[allow(dead_code)]
pub const STATUS_NO_ENTRY: i32 = -2;
pub const STATUS_INVALID: i32 = -22;
pub const STATUS_PIPE: i32 = -32;
pub const STATUS_CONN_RESET: i32 = -104;
/// EXDEV: an isochronous packet was not transferred, because its frame had
/// passed. usbip-win2 reports it to Windows as USBD_STATUS_ISO_TD_ERROR.
pub const STATUS_ISO_LATE: i32 = -18;

pub const BASIC_HEADER_SIZE: usize = 20;
/// Transfer fields and setup packet after the basic header.
pub const SUBMIT_BODY_SIZE: usize = 28;
/// Unlink sequence number and padding.
pub const UNLINK_BODY_SIZE: usize = 28;
pub const ISO_DESCRIPTOR_SIZE: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OpHeader {
    pub version: u16,
    pub code: u16,
    pub status: u32,
}

pub fn read_op_header(r: &mut impl Read) -> io::Result<OpHeader> {
    let mut b = [0u8; 8];
    r.read_exact(&mut b)?;
    let h = OpHeader {
        version: u16::from_be_bytes([b[0], b[1]]),
        code: u16::from_be_bytes([b[2], b[3]]),
        status: u32::from_be_bytes([b[4], b[5], b[6], b[7]]),
    };
    if h.version != PROTOCOL_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unsupported USB/IP version 0x{:04x}", h.version),
        ));
    }
    Ok(h)
}

pub fn append_op_header(dst: &mut Vec<u8>, code: u16, status: u32) {
    dst.extend_from_slice(&PROTOCOL_VERSION.to_be_bytes());
    dst.extend_from_slice(&code.to_be_bytes());
    dst.extend_from_slice(&status.to_be_bytes());
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BasicHeader {
    pub command: u32,
    pub sequence: u32,
    pub device_id: u32,
    pub direction: u32,
    pub endpoint: u32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SubmitRequest {
    pub basic: BasicHeader,
    pub transfer_flags: u32,
    pub transfer_buffer_length: u32,
    pub start_frame: u32,
    pub number_of_packets: u32,
    pub interval: u32,
    pub setup: [u8; 8],
}

impl SubmitRequest {
    pub fn is_isochronous(&self) -> bool {
        self.number_of_packets != NO_ISO_PACKETS && self.number_of_packets != 0
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IsoPacket {
    pub offset: u32,
    pub length: u32,
    pub actual_length: u32,
    pub status: i32,
}

fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// Buffered reads from a connection. `read_fixed` returns the next bytes
/// without copying them out of the buffer, which keeps decoding the per-URB
/// headers allocation-free.
pub struct ConnReader<R> {
    inner: R,
    buf: Box<[u8]>,
    pos: usize,
    end: usize,
}

impl<R: Read> ConnReader<R> {
    pub fn new(inner: R, capacity: usize) -> ConnReader<R> {
        ConnReader { inner, buf: vec![0; capacity].into_boxed_slice(), pos: 0, end: 0 }
    }

    pub fn get_ref(&self) -> &R {
        &self.inner
    }

    /// Returns the next `n` bytes, valid until the next read. A stream that
    /// ends before any of them is `UnexpectedEof` with no bytes consumed
    /// (`is_clean_eof`); one that ends part-way is a plain `UnexpectedEof`.
    pub fn read_fixed(&mut self, n: usize) -> io::Result<&[u8]> {
        assert!(n <= self.buf.len());
        if self.end - self.pos < n {
            self.buf.copy_within(self.pos..self.end, 0);
            self.end -= self.pos;
            self.pos = 0;
            while self.end < n {
                match self.inner.read(&mut self.buf[self.end..]) {
                    Ok(0) if self.end == 0 => return Err(clean_eof()),
                    Ok(0) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "unexpected EOF")),
                    Ok(read) => self.end += read,
                    Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                    Err(err) => return Err(err),
                }
            }
        }
        let start = self.pos;
        self.pos += n;
        Ok(&self.buf[start..start + n])
    }
}

impl<R: Read> Read for ConnReader<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.pos == self.end {
            if out.len() >= self.buf.len() {
                return self.inner.read(out);
            }
            self.pos = 0;
            self.end = self.inner.read(&mut self.buf)?;
        }
        let n = out.len().min(self.end - self.pos);
        out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

/// The marker error for a stream that ended cleanly between messages.
struct CleanEof;

impl std::fmt::Debug for CleanEof {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("EOF")
    }
}

impl std::fmt::Display for CleanEof {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("EOF")
    }
}

impl std::error::Error for CleanEof {}

fn clean_eof() -> io::Error {
    io::Error::new(io::ErrorKind::UnexpectedEof, CleanEof)
}

/// Reports whether the stream ended cleanly, between messages.
pub fn is_clean_eof(err: &io::Error) -> bool {
    err.get_ref().is_some_and(|inner| inner.is::<CleanEof>())
}

pub fn read_basic_header<R: Read>(r: &mut ConnReader<R>) -> io::Result<BasicHeader> {
    let b = r.read_fixed(BASIC_HEADER_SIZE)?;
    Ok(BasicHeader {
        command: be32(b, 0),
        sequence: be32(b, 4),
        device_id: be32(b, 8),
        direction: be32(b, 12),
        endpoint: be32(b, 16),
    })
}

pub fn read_submit_body<R: Read>(r: &mut ConnReader<R>, basic: BasicHeader) -> io::Result<SubmitRequest> {
    let b = r.read_fixed(SUBMIT_BODY_SIZE).map_err(partial_eof)?;
    let mut setup = [0u8; 8];
    setup.copy_from_slice(&b[20..28]);
    Ok(SubmitRequest {
        basic,
        transfer_flags: be32(b, 0),
        transfer_buffer_length: be32(b, 4),
        start_frame: be32(b, 8),
        number_of_packets: be32(b, 12),
        interval: be32(b, 16),
        setup,
    })
}

/// Inside a message, any end of stream is unexpected.
pub fn partial_eof(err: io::Error) -> io::Error {
    if is_clean_eof(&err) { io::Error::new(io::ErrorKind::UnexpectedEof, "unexpected EOF") } else { err }
}

/// Decodes the isochronous packet descriptors of a URB into `dst`, reusing its
/// storage. A non-isochronous URB leaves `dst` empty.
pub fn read_iso_packets<R: Read>(r: &mut ConnReader<R>, count: u32, dst: &mut Vec<IsoPacket>) -> io::Result<()> {
    dst.clear();
    if count == NO_ISO_PACKETS || count == 0 {
        return Ok(());
    }
    if count > 4096 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("invalid isochronous packet count {count}")));
    }
    for _ in 0..count {
        let b = r.read_fixed(ISO_DESCRIPTOR_SIZE).map_err(partial_eof)?;
        dst.push(IsoPacket {
            offset: be32(b, 0),
            length: be32(b, 4),
            actual_length: be32(b, 8),
            status: be32(b, 12) as i32,
        });
    }
    Ok(())
}

/// Writes one complete USBIP_RET_SUBMIT frame.
#[cfg(test)]
pub fn write_ret_submit(
    w: &mut impl std::io::Write,
    req: &SubmitRequest,
    status: i32,
    actual_length: u32,
    data: &[u8],
    packets: &[IsoPacket],
    error_count: u32,
) -> io::Result<()> {
    let mut frame = Vec::new();
    append_ret_submit(&mut frame, req, status, actual_length, data, packets, error_count);
    w.write_all(&frame)
}

/// Appends one complete USBIP_RET_SUBMIT frame to `dst`. The USB/IP
/// specification requires devid, direction and endpoint to be zero in
/// replies; only command and sequence identify the response.
pub fn append_ret_submit(
    dst: &mut Vec<u8>,
    req: &SubmitRequest,
    status: i32,
    mut actual_length: u32,
    mut data: &[u8],
    packets: &[IsoPacket],
    error_count: u32,
) {
    if status != STATUS_OK {
        actual_length = 0;
        data = &[];
    }
    let (number_of_packets, start_frame) =
        if req.is_isochronous() { (packets.len() as u32, req.start_frame) } else { (NO_ISO_PACKETS, 0) };

    dst.extend_from_slice(&RET_SUBMIT.to_be_bytes());
    dst.extend_from_slice(&req.basic.sequence.to_be_bytes());
    dst.extend_from_slice(&[0; 12]); // devid, direction, endpoint
    dst.extend_from_slice(&(status as u32).to_be_bytes());
    dst.extend_from_slice(&actual_length.to_be_bytes());
    dst.extend_from_slice(&start_frame.to_be_bytes());
    dst.extend_from_slice(&number_of_packets.to_be_bytes());
    dst.extend_from_slice(&error_count.to_be_bytes());
    dst.extend_from_slice(&[0; 8]); // padding
    dst.extend_from_slice(data);
    for p in packets {
        dst.extend_from_slice(&p.offset.to_be_bytes());
        dst.extend_from_slice(&p.length.to_be_bytes());
        dst.extend_from_slice(&p.actual_length.to_be_bytes());
        dst.extend_from_slice(&(p.status as u32).to_be_bytes());
    }
}

pub fn append_ret_unlink(dst: &mut Vec<u8>, request: &BasicHeader, status: i32) {
    dst.extend_from_slice(&RET_UNLINK.to_be_bytes());
    dst.extend_from_slice(&request.sequence.to_be_bytes());
    dst.extend_from_slice(&[0; 12]); // devid, direction, endpoint
    dst.extend_from_slice(&(status as u32).to_be_bytes());
    dst.extend_from_slice(&[0; 24]); // padding
}

/// Copies `value` into a fixed-size, zero-padded field.
pub fn fixed_string(dst: &mut [u8], value: &str) {
    dst.fill(0);
    let n = value.len().min(dst.len());
    dst[..n].copy_from_slice(&value.as_bytes()[..n]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn op_header_round_trip() {
        let mut b = Vec::new();
        append_op_header(&mut b, OP_REP_DEVLIST, 0);
        let h = read_op_header(&mut b.as_slice()).unwrap();
        assert_eq!(h, OpHeader { version: PROTOCOL_VERSION, code: OP_REP_DEVLIST, status: 0 });
    }

    #[test]
    fn no_iso_packet_sentinel_is_accepted() {
        let mut r = ConnReader::new(io::empty(), 64);
        let mut packets = vec![IsoPacket::default()];
        read_iso_packets(&mut r, NO_ISO_PACKETS, &mut packets).unwrap();
        assert!(packets.is_empty());
    }

    #[test]
    fn ret_submit_non_iso_wire_layout() {
        let req = SubmitRequest {
            basic: BasicHeader {
                sequence: 7,
                device_id: 0x0001_0001,
                direction: DIRECTION_IN,
                endpoint: 3,
                command: 0,
            },
            number_of_packets: NO_ISO_PACKETS,
            ..Default::default()
        };
        let mut wire = Vec::new();
        write_ret_submit(&mut wire, &req, STATUS_OK, 4, &[1, 2, 3, 4], &[], 0).unwrap();
        // 20-byte basic header + 28-byte body + 4-byte payload.
        assert_eq!(wire.len(), 52);
        assert_eq!(be32(&wire, 0), RET_SUBMIT);
        assert_eq!(be32(&wire, 4), 7);
        // Server replies must zero devid, direction and endpoint.
        for offset in (8..20).step_by(4) {
            assert_eq!(be32(&wire, offset), 0, "reply basic field at offset {offset}");
        }
        assert_eq!(be32(&wire, 32), NO_ISO_PACKETS);
    }

    #[test]
    fn ret_submit_iso_wire_layout() {
        let req = SubmitRequest {
            basic: BasicHeader { sequence: 9, direction: DIRECTION_IN, endpoint: 2, ..Default::default() },
            start_frame: 123,
            number_of_packets: 2,
            ..Default::default()
        };
        let packets = [
            IsoPacket { offset: 0, length: 192, actual_length: 192, status: 0 },
            IsoPacket { offset: 192, length: 192, actual_length: 192, status: 0 },
        ];
        let mut wire = Vec::new();
        write_ret_submit(&mut wire, &req, STATUS_OK, 384, &[0; 384], &packets, 0).unwrap();
        assert_eq!(be32(&wire, 28), 123, "start_frame");
        assert_eq!(be32(&wire, 32), 2, "number_of_packets");
        assert_eq!(wire.len(), 48 + 384 + 2 * 16);
    }

    #[test]
    fn ret_unlink_wire_layout() {
        let mut wire = Vec::new();
        append_ret_unlink(&mut wire, &BasicHeader { sequence: 5, ..Default::default() }, STATUS_CONN_RESET);
        assert_eq!(wire.len(), 48);
        assert_eq!(be32(&wire, 0), RET_UNLINK);
        assert_eq!(be32(&wire, 4), 5);
        assert_eq!(be32(&wire, 20) as i32, STATUS_CONN_RESET);
    }

    #[test]
    fn read_fixed_spans_short_reads_and_reports_eof() {
        // A reader that returns one byte per call.
        struct Trickle(Vec<u8>);
        impl Read for Trickle {
            fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
                if self.0.is_empty() || out.is_empty() {
                    return Ok(0);
                }
                out[0] = self.0.remove(0);
                Ok(1)
            }
        }
        let mut r = ConnReader::new(Trickle((1..=6).collect()), 8);
        assert_eq!(r.read_fixed(4).unwrap(), &[1, 2, 3, 4]);
        let err = r.read_fixed(4).unwrap_err();
        assert!(!is_clean_eof(&err), "partial message must not be a clean EOF");
        let mut r = ConnReader::new(Trickle(Vec::new()), 8);
        assert!(is_clean_eof(&r.read_fixed(4).unwrap_err()));
    }
}
