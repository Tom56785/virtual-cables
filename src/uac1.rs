//! A full-speed USB Audio Class 1.0 cable: its descriptors, control requests
//! and the ring buffer that loops playback back to capture.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::audio::Ring;

pub const DESCRIPTOR_DEVICE: u8 = 0x01;
pub const DESCRIPTOR_CONFIGURATION: u8 = 0x02;
pub const DESCRIPTOR_STRING: u8 = 0x03;
pub const DESCRIPTOR_INTERFACE: u8 = 0x04;
pub const DESCRIPTOR_ENDPOINT: u8 = 0x05;
pub const DESCRIPTOR_CS_INTERFACE: u8 = 0x24;
pub const DESCRIPTOR_CS_ENDPOINT: u8 = 0x25;

/// Describes one full-speed USB Audio Class 1.0 cable. Windows uses its in-box
/// usbaudio.sys class driver when this descriptor set enumerates.
pub struct Descriptors {
    pub cable_number: u32,
    pub product: String,
    pub serial: String,
    pub device: Vec<u8>,
    pub config: Vec<u8>,
    pub strings: HashMap<u8, Vec<u8>>,
}

impl Descriptors {
    /// Cable `cable_number` with its default name and serial number.
    #[cfg(test)]
    pub fn new(cable_number: u32) -> Result<Descriptors, String> {
        let name = format!("Virtual Cable {cable_number:02}");
        Descriptors::named(cable_number, &format!("VCABLE-{cable_number:03}"), &name)
    }

    /// Descriptors for cable `cable_number`, whose product string, the name
    /// Windows Sound shows, is `name`. The number sets the product ID; with
    /// the serial number it identifies the device to Windows.
    pub fn named(cable_number: u32, serial: &str, name: &str) -> Result<Descriptors, String> {
        if !(1..=255).contains(&cable_number) {
            return Err("cable number must be 1..255".into());
        }
        let product = name.to_string();
        let serial = serial.to_string();
        let mut strings = HashMap::new();
        strings.insert(0, vec![4, DESCRIPTOR_STRING, 0x09, 0x04]); // en-US
        strings.insert(1, string_descriptor("Virtual Cables"));
        strings.insert(2, string_descriptor(&product));
        strings.insert(3, string_descriptor(&serial));
        let d = Descriptors {
            cable_number,
            device: device_descriptor(cable_number),
            config: configuration_descriptor(),
            product,
            serial,
            strings,
        };
        d.validate()?;
        Ok(d)
    }

    pub fn get_descriptor(&self, descriptor_type: u8, index: u8) -> Option<Vec<u8>> {
        match descriptor_type {
            DESCRIPTOR_DEVICE if index == 0 => Some(self.device.clone()),
            DESCRIPTOR_CONFIGURATION if index == 0 => Some(self.config.clone()),
            DESCRIPTOR_STRING => self.strings.get(&index).cloned(),
            _ => None,
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.device.len() != 18 || self.device[0] != 18 || self.device[1] != DESCRIPTOR_DEVICE {
            return Err("invalid device descriptor".into());
        }
        if self.config.len() < 9 || self.config[1] != DESCRIPTOR_CONFIGURATION {
            return Err("invalid configuration descriptor".into());
        }
        let total = u16::from_le_bytes([self.config[2], self.config[3]]) as usize;
        if total != self.config.len() {
            return Err(format!("configuration wTotalLength={total}, actual={}", self.config.len()));
        }
        let mut pos = 0;
        while pos < self.config.len() {
            let length = self.config[pos] as usize;
            if length < 2 || pos + length > self.config.len() {
                return Err(format!("invalid descriptor at offset {pos}"));
            }
            pos += length;
        }
        Ok(())
    }
}

fn device_descriptor(cable_number: u32) -> Vec<u8> {
    let pid = (0xCA00 + cable_number) as u16;
    let mut b = vec![0u8; 18];
    b[0] = 18;
    b[1] = DESCRIPTOR_DEVICE;
    b[2..4].copy_from_slice(&0x0110u16.to_le_bytes()); // USB 1.1 full-speed device; no qualifier/BOS required
    b[4] = 0x00; // class defined per interface
    b[5] = 0x00;
    b[6] = 0x00;
    b[7] = 64; // EP0 packet size
    b[8..10].copy_from_slice(&0xFFFFu16.to_le_bytes()); // experimental local VID
    b[10..12].copy_from_slice(&pid.to_le_bytes());
    b[12..14].copy_from_slice(&0x0001u16.to_le_bytes());
    b[14] = 1; // manufacturer
    b[15] = 2; // product
    b[16] = 3; // serial
    b[17] = 1; // configurations
    b
}

fn configuration_descriptor() -> Vec<u8> {
    let mut b: Vec<u8> = Vec::new();
    let c = DESCRIPTOR_CS_INTERFACE;

    // Configuration header; wTotalLength is patched after construction.
    b.extend_from_slice(&[9, DESCRIPTOR_CONFIGURATION, 0, 0, 3, 1, 0, 0x80, 50]);

    // AudioControl interface 0.
    b.extend_from_slice(&[9, DESCRIPTOR_INTERFACE, 0, 0, 0, 0x01, 0x01, 0x00, 0]);
    // UAC1 AC Header: two AudioStreaming interfaces (1 and 2).
    b.extend_from_slice(&[10, c, 0x01, 0x00, 0x01, 72, 0, 2, 1, 2]);
    // Playback path: USB streaming input terminal -> feature -> speaker output.
    b.extend_from_slice(&[12, c, 0x02, 1, 0x01, 0x01, 0, 2, 0x03, 0x00, 0, 0]);
    b.extend_from_slice(&[10, c, 0x06, 2, 1, 1, 0x03, 0x00, 0x00, 0]);
    b.extend_from_slice(&[9, c, 0x03, 3, 0x01, 0x03, 0, 2, 0]);
    // Capture path: microphone input -> feature -> USB streaming output terminal.
    b.extend_from_slice(&[12, c, 0x02, 4, 0x01, 0x02, 0, 2, 0x03, 0x00, 0, 0]);
    b.extend_from_slice(&[10, c, 0x06, 5, 4, 1, 0x03, 0x00, 0x00, 0]);
    b.extend_from_slice(&[9, c, 0x03, 6, 0x01, 0x01, 0, 5, 0]);

    // Playback AudioStreaming interface 1, alternate setting 0 (zero bandwidth).
    b.extend_from_slice(&[9, DESCRIPTOR_INTERFACE, 1, 0, 0, 0x01, 0x02, 0x00, 0]);
    // Alternate setting 1, stereo PCM 48 kHz / 24 bit, OUT endpoint 1.
    b.extend_from_slice(&[9, DESCRIPTOR_INTERFACE, 1, 1, 1, 0x01, 0x02, 0x00, 0]);
    b.extend_from_slice(&[7, c, 0x01, 1, 1, 0x01, 0x00]);
    b.extend_from_slice(&[11, c, 0x02, 1, 2, 3, 24, 1, 0x80, 0xBB, 0x00]);
    b.extend_from_slice(&[9, DESCRIPTOR_ENDPOINT, 0x01, 0x09, 0x20, 0x01, 1, 0, 0]);
    b.extend_from_slice(&[7, DESCRIPTOR_CS_ENDPOINT, 0x01, 0, 0, 0, 0]);

    // Capture AudioStreaming interface 2, alternate setting 0.
    b.extend_from_slice(&[9, DESCRIPTOR_INTERFACE, 2, 0, 0, 0x01, 0x02, 0x00, 0]);
    // Alternate setting 1, stereo PCM 48 kHz / 24 bit, synchronous IN endpoint 2.
    b.extend_from_slice(&[9, DESCRIPTOR_INTERFACE, 2, 1, 1, 0x01, 0x02, 0x00, 0]);
    b.extend_from_slice(&[7, c, 0x01, 6, 1, 0x01, 0x00]);
    b.extend_from_slice(&[11, c, 0x02, 1, 2, 3, 24, 1, 0x80, 0xBB, 0x00]);
    b.extend_from_slice(&[9, DESCRIPTOR_ENDPOINT, 0x82, 0x0D, 0x20, 0x01, 1, 0, 0]);
    b.extend_from_slice(&[7, DESCRIPTOR_CS_ENDPOINT, 0x01, 0, 0, 0, 0]);

    let total = b.len() as u16;
    b[2..4].copy_from_slice(&total.to_le_bytes());
    b
}

fn string_descriptor(s: &str) -> Vec<u8> {
    let mut encoded: Vec<u16> = s.encode_utf16().collect();
    let mut length = 2 + encoded.len() * 2;
    if length > 255 {
        encoded.truncate(126);
        length = 254;
    }
    let mut b = vec![0u8; length];
    b[0] = length as u8;
    b[1] = DESCRIPTOR_STRING;
    for (i, v) in encoded.iter().enumerate() {
        b[2 + i * 2..4 + i * 2].copy_from_slice(&v.to_le_bytes());
    }
    b
}

/// The default elastic reserve the capture side keeps in the ring. It covers
/// the 10 ms packet sawtooth and the short gaps of routine host scheduling
/// hiccups while keeping the cable's added latency low.
pub const DEFAULT_RESERVE_MS: i64 = 40;

/// The smallest usable reserve: below it, the 10 ms granularity of USB
/// transfers alone would underrun the ring.
pub const MIN_RESERVE_MS: i64 = 10;

/// The smallest ring-buffer capacity a cable accepts: the reserve may use at
/// most half the ring, so this leaves room for the smallest reserve.
pub const MIN_BUFFER_MS: i64 = 2 * MIN_RESERVE_MS;

/// The largest ring-buffer capacity a cable accepts.
pub const MAX_BUFFER_MS: i64 = 2000;

pub const REQUEST_GET_STATUS: u8 = 0x00;
pub const REQUEST_CLEAR_FEATURE: u8 = 0x01;
pub const REQUEST_SET_FEATURE: u8 = 0x03;
pub const REQUEST_SET_ADDRESS: u8 = 0x05;
pub const REQUEST_GET_DESCRIPTOR: u8 = 0x06;
#[allow(dead_code)] // listed for completeness; stalled like any unsupported request
pub const REQUEST_SET_DESCRIPTOR: u8 = 0x07;
pub const REQUEST_GET_CONFIGURATION: u8 = 0x08;
pub const REQUEST_SET_CONFIGURATION: u8 = 0x09;
pub const REQUEST_GET_INTERFACE: u8 = 0x0A;
pub const REQUEST_SET_INTERFACE: u8 = 0x0B;
pub const REQUEST_SYNCH_FRAME: u8 = 0x0C;

pub const AUDIO_SET_CUR: u8 = 0x01;
pub const AUDIO_GET_CUR: u8 = 0x81;
pub const AUDIO_GET_MIN: u8 = 0x82;
pub const AUDIO_GET_MAX: u8 = 0x83;
pub const AUDIO_GET_RES: u8 = 0x84;

/// EPIPE: the request is stalled.
const STALL: i32 = -32;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SetupPacket {
    pub request_type: u8,
    pub request: u8,
    pub value: u16,
    pub index: u16,
    pub length: u16,
}

pub fn parse_setup(b: &[u8]) -> Result<SetupPacket, String> {
    if b.len() < 8 {
        return Err("setup packet too short".into());
    }
    Ok(SetupPacket {
        request_type: b[0],
        request: b[1],
        value: u16::from_le_bytes([b[2], b[3]]),
        index: u16::from_le_bytes([b[4], b[5]]),
        length: u16::from_le_bytes([b[6], b[7]]),
    })
}

/// The USB/IP bus ID of the cable with ID `number`.
pub fn bus_id(number: u32) -> String {
    format!("1-{number}")
}

pub struct Device {
    pub number: u32,
    pub bus_id: String,
    pub descriptors: Descriptors,
    pub buffer: Ring,
    /// The elastic reserve, in PCM frames, the capture side keeps in `buffer`.
    reserve_frames: usize,
    state: Mutex<DeviceState>,
}

struct DeviceState {
    configuration: u8,
    alt_setting: HashMap<u8, u8>,
    sample_rate: u32,
    mute: bool,
    volume: i16,
}

impl Device {
    /// Cable `number` with its default name and serial number.
    #[cfg(test)]
    pub fn new(number: u32, latency_ms: i64, reserve_ms: i64) -> Result<Device, String> {
        let (serial, name) = (format!("VCABLE-{number:03}"), format!("Virtual Cable {number:02}"));
        Device::named(number, &serial, &name, latency_ms, reserve_ms)
    }

    /// Creates the cable with ID `number`, serial number `serial`, called
    /// `name`, whose ring holds `latency_ms` of audio, of which the capture
    /// side keeps `reserve_ms` as its elastic reserve. The reserve is limited
    /// to half the ring.
    pub fn named(number: u32, serial: &str, name: &str, latency_ms: i64, reserve_ms: i64) -> Result<Device, String> {
        let descriptors = Descriptors::named(number, serial, name)?;
        let latency_ms = latency_ms.clamp(MIN_BUFFER_MS, MAX_BUFFER_MS);
        // 48 kHz * 2 channels * 3 bytes/sample (24-bit PCM).
        let capacity = 48000 * 2 * 3 * latency_ms as usize / 1000;
        let mut reserve_ms = reserve_ms.max(MIN_RESERVE_MS);
        if reserve_ms > latency_ms / 2 {
            reserve_ms = latency_ms / 2;
        }
        Ok(Device {
            number,
            bus_id: bus_id(number),
            descriptors,
            buffer: Ring::new_framed(capacity, 6),
            reserve_frames: 48 * reserve_ms as usize,
            state: Mutex::new(DeviceState {
                configuration: 0,
                alt_setting: HashMap::from([(0, 0), (1, 0), (2, 0)]),
                sample_rate: 48000,
                mute: false,
                volume: 0,
            }),
        })
    }

    pub fn product(&self) -> &str {
        &self.descriptors.product
    }

    /// The elastic reserve, in PCM frames, the capture side keeps.
    pub fn reserve_frames(&self) -> usize {
        self.reserve_frames
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, DeviceState> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Processes the subset of standard and UAC1 class requests used during
    /// Windows enumeration and basic PCM streaming setup.
    pub fn handle_control(&self, setup: SetupPacket, out: &[u8]) -> (Vec<u8>, i32) {
        let mut d = self.lock();

        let standard = setup.request_type & 0x60 == 0x00;
        if standard {
            return match setup.request {
                REQUEST_GET_DESCRIPTOR => {
                    let typ = (setup.value >> 8) as u8;
                    let idx = setup.value as u8;
                    match self.descriptors.get_descriptor(typ, idx) {
                        Some(v) => (truncate(v, setup.length), 0),
                        None => (Vec::new(), STALL),
                    }
                }
                REQUEST_SET_ADDRESS => (Vec::new(), 0),
                REQUEST_SET_CONFIGURATION => {
                    let configuration = setup.value as u8;
                    if configuration > 1 {
                        return (Vec::new(), STALL);
                    }
                    d.configuration = configuration;
                    // Selecting a configuration resets every interface to alternate 0.
                    for iface in 0..3 {
                        d.alt_setting.insert(iface, 0);
                    }
                    self.buffer.reset();
                    (Vec::new(), 0)
                }
                REQUEST_GET_CONFIGURATION => (truncate(vec![d.configuration], setup.length), 0),
                REQUEST_SET_INTERFACE => {
                    let iface = setup.index as u8;
                    let alt = setup.value as u8;
                    if d.configuration != 1 || iface > 2 || alt > 1 || (iface == 0 && alt != 0) {
                        return (Vec::new(), STALL);
                    }
                    d.alt_setting.insert(iface, alt);
                    if alt == 0 && (iface == 1 || iface == 2) {
                        self.buffer.reset();
                    }
                    (Vec::new(), 0)
                }
                REQUEST_GET_INTERFACE => {
                    let iface = setup.index as u8;
                    match d.alt_setting.get(&iface) {
                        Some(&alt) => (truncate(vec![alt], setup.length), 0),
                        None => (Vec::new(), STALL),
                    }
                }
                REQUEST_GET_STATUS => (truncate(vec![0, 0], setup.length), 0),
                // Full-speed isochronous endpoints use one frame per millisecond.
                // A stable zero frame is sufficient for this synthetic device and is
                // preferable to stalling a host-side synchronization query.
                REQUEST_SYNCH_FRAME => (truncate(vec![0, 0], setup.length), 0),
                REQUEST_CLEAR_FEATURE | REQUEST_SET_FEATURE => (Vec::new(), 0),
                _ => (Vec::new(), STALL),
            };
        }

        // UAC1 endpoint sampling-frequency requests.
        let recipient = setup.request_type & 0x1f;
        let selector = (setup.value >> 8) as u8;
        let endpoint_address = setup.index as u8;
        if recipient == 0x02 && selector == 0x01 && (endpoint_address == 0x01 || endpoint_address == 0x82) {
            // endpoint, SAMPLING_FREQ_CONTROL
            match setup.request {
                AUDIO_SET_CUR => {
                    if out.len() >= 3 {
                        let rate = out[0] as u32 | (out[1] as u32) << 8 | (out[2] as u32) << 16;
                        if rate != 48000 {
                            return (Vec::new(), STALL);
                        }
                        d.sample_rate = rate;
                    }
                    return (Vec::new(), 0);
                }
                AUDIO_GET_CUR => return (truncate(rate24(d.sample_rate), setup.length), 0),
                AUDIO_GET_MIN | AUDIO_GET_MAX => return (truncate(rate24(48000), setup.length), 0),
                AUDIO_GET_RES => return (truncate(rate24(1), setup.length), 0),
                _ => {}
            }
        }

        // Feature-unit mute/volume requests. Windows may query these because the
        // topology advertises a basic feature unit.
        let entity_id = (setup.index >> 8) as u8;
        let interface_number = setup.index as u8;
        if recipient == 0x01 && interface_number == 0 && (entity_id == 2 || entity_id == 5) {
            // interface/entity
            let control_selector = (setup.value >> 8) as u8;
            match control_selector {
                0x01 => match setup.request {
                    // MUTE_CONTROL
                    AUDIO_SET_CUR => {
                        if let Some(&first) = out.first() {
                            d.mute = first != 0;
                        }
                        return (Vec::new(), 0);
                    }
                    AUDIO_GET_CUR => return (truncate(vec![d.mute as u8], setup.length), 0),
                    _ => {}
                },
                0x02 => match setup.request {
                    // VOLUME_CONTROL, signed 1/256 dB
                    AUDIO_SET_CUR => {
                        if out.len() >= 2 {
                            d.volume = i16::from_le_bytes([out[0], out[1]]);
                        }
                        return (Vec::new(), 0);
                    }
                    AUDIO_GET_CUR => return (truncate(d.volume.to_le_bytes().to_vec(), setup.length), 0),
                    AUDIO_GET_MIN => return (truncate((-60i16 * 256).to_le_bytes().to_vec(), setup.length), 0),
                    AUDIO_GET_MAX => return (truncate(0i16.to_le_bytes().to_vec(), setup.length), 0),
                    AUDIO_GET_RES => return (truncate(256i16.to_le_bytes().to_vec(), setup.length), 0),
                    _ => {}
                },
                _ => {}
            }
        }

        (Vec::new(), STALL)
    }

    pub fn playback_active(&self) -> bool {
        let d = self.lock();
        d.configuration == 1 && d.alt_setting.get(&1) == Some(&1)
    }

    pub fn capture_active(&self) -> bool {
        let d = self.lock();
        d.configuration == 1 && d.alt_setting.get(&2) == Some(&1)
    }

    pub fn write_playback(&self, p: &[u8]) -> usize {
        if !self.playback_active() {
            return 0;
        }
        self.buffer.write(p)
    }

    pub fn read_capture(&self, p: &mut [u8]) -> usize {
        if !self.capture_active() {
            p.fill(0);
            return 0;
        }
        // Keep an elastic reserve for scheduling jitter. The PI resampler learns
        // clock drift gradually rather than chasing individual USB packets.
        self.buffer.read_adaptive_pcm24(p, self.reserve_frames)
    }
}

fn truncate(mut b: Vec<u8>, n: u16) -> Vec<u8> {
    b.truncate(n as usize);
    b
}

fn rate24(rate: u32) -> Vec<u8> {
    vec![rate as u8, (rate >> 8) as u8, (rate >> 16) as u8]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn le16(b: &[u8]) -> u16 {
        u16::from_le_bytes([b[0], b[1]])
    }

    #[test]
    fn descriptors_validate() {
        let d = Descriptors::new(1).unwrap();
        d.validate().unwrap();
        assert_eq!(le16(&d.config[2..4]) as usize, d.config.len());
        assert_eq!(le16(&d.device[2..4]), 0x0110, "bcdUSB");
    }

    #[test]
    fn audio_endpoint_descriptors() {
        let d = Descriptors::new(1).unwrap();
        let mut endpoints = Vec::new();
        let mut pos = 0;
        while pos < d.config.len() {
            let length = d.config[pos] as usize;
            if d.config[pos + 1] == DESCRIPTOR_ENDPOINT {
                endpoints.push(&d.config[pos..pos + length]);
            }
            pos += length;
        }
        assert_eq!(endpoints.len(), 2);
        assert_eq!((endpoints[0][2], endpoints[0][3]), (0x01, 0x09), "playback endpoint");
        assert_eq!((endpoints[1][2], endpoints[1][3]), (0x82, 0x0D), "capture endpoint");
        for ep in endpoints {
            assert_eq!(le16(&ep[4..6]), 288, "wMaxPacketSize");
            assert_eq!(ep[6], 1, "bInterval");
        }
    }

    #[test]
    fn audio_format_is_24bit_pcm() {
        let d = Descriptors::new(1).unwrap();
        let mut found = false;
        let mut pos = 0;
        while pos < d.config.len() {
            let length = d.config[pos] as usize;
            if d.config[pos + 1] == DESCRIPTOR_CS_INTERFACE && length == 11 && d.config[pos + 2] == 0x02 {
                found = true;
                assert_eq!((d.config[pos + 5], d.config[pos + 6]), (3, 24), "subframe size / bit resolution");
            }
            pos += length;
        }
        assert!(found, "PCM format descriptor not found");
    }

    #[test]
    fn unique_product_id() {
        let a = Descriptors::new(1).unwrap();
        let b = Descriptors::new(2).unwrap();
        assert_ne!(le16(&a.device[10..12]), le16(&b.device[10..12]));
    }

    #[test]
    fn capture_endpoint_uses_windows_compatible_synchronous_mode() {
        let d = Descriptors::new(1).unwrap();
        let mut found = false;
        let mut pos = 0;
        while pos < d.config.len() {
            let length = d.config[pos] as usize;
            assert!(length >= 2 && pos + length <= d.config.len(), "invalid descriptor at {pos}");
            if d.config[pos + 1] == DESCRIPTOR_ENDPOINT && length >= 4 && d.config[pos + 2] == 0x82 {
                found = true;
                assert_eq!(d.config[pos + 3], 0x0D, "capture endpoint attributes");
            }
            pos += length;
        }
        assert!(found, "capture endpoint 0x82 not found");
    }

    #[test]
    fn get_device_descriptor() {
        let d = Device::new(1, 100, DEFAULT_RESERVE_MS).unwrap();
        let setup = SetupPacket {
            request_type: 0x80,
            request: REQUEST_GET_DESCRIPTOR,
            value: (DESCRIPTOR_DEVICE as u16) << 8,
            length: 18,
            ..Default::default()
        };
        let (data, status) = d.handle_control(setup, &[]);
        assert_eq!((status, data.len()), (0, 18));
    }

    #[test]
    fn set_and_get_interface() {
        let d = Device::new(1, 100, DEFAULT_RESERVE_MS).unwrap();
        let control = |request_type, request, index, value, length| {
            d.handle_control(SetupPacket { request_type, request, value, index, length }, &[])
        };
        control(0x00, REQUEST_SET_CONFIGURATION, 0, 1, 0);
        control(0x01, REQUEST_SET_INTERFACE, 1, 1, 0);
        let (data, status) = control(0x81, REQUEST_GET_INTERFACE, 1, 0, 1);
        assert_eq!((status, data), (0, vec![1]));
    }

    #[test]
    fn sampling_rate() {
        let d = Device::new(1, 100, DEFAULT_RESERVE_MS).unwrap();
        let setup = SetupPacket { request_type: 0xA2, request: AUDIO_GET_CUR, value: 0x0100, index: 0x82, length: 3 };
        let (data, status) = d.handle_control(setup, &[]);
        assert_eq!((status, data.len()), (0, 3));
        let rate = data[0] as u32 | (data[1] as u32) << 8 | (data[2] as u32) << 16;
        assert_eq!(rate, 48000);
    }

    #[test]
    fn reserve_is_limited_to_half_the_buffer() {
        assert_eq!(Device::new(1, 500, 40).unwrap().reserve_frames(), 48 * 40);
        assert_eq!(Device::new(1, 60, 40).unwrap().reserve_frames(), 48 * 30);
        assert_eq!(Device::new(1, 500, 1).unwrap().reserve_frames(), 48 * MIN_RESERVE_MS as usize);
        // A ring below the minimum is enlarged, so the smallest reserve still
        // fits in half of it.
        let small = Device::new(1, 10, 40).unwrap();
        assert_eq!(small.reserve_frames(), 48 * MIN_RESERVE_MS as usize);
        assert_eq!(small.buffer.capacity(), 48 * 6 * MIN_BUFFER_MS as usize);
    }
}
