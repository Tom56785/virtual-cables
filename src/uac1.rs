//! A full-speed USB Audio Class 1.0 cable: its descriptors, control requests
//! and the ring buffer that loops playback back to capture.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::audio::{DelayLine, Gain};

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

/// The default delay between a cable's playback and recording sides. Windows
/// sends playback about 10 ms ahead of its frames, so in steady streaming any
/// delay works; this much also covers playback that arrives up to about 50 ms
/// late after a system stall.
pub const DEFAULT_LATENCY_MS: i64 = 40;

/// The delay range a cable accepts, in milliseconds (bus frames).
pub const MIN_LATENCY_MS: i64 = 1;
pub const MAX_LATENCY_MS: i64 = 1000;

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
    /// Carries the playback endpoint's audio to the capture endpoint.
    pub audio: DelayLine,
    state: Mutex<DeviceState>,
}

struct DeviceState {
    configuration: u8,
    alt_setting: HashMap<u8, u8>,
    sample_rate: u32,
    /// The playback (unit 2) and recording (unit 5) feature units.
    features: [FeatureUnit; 2],
}

/// A feature unit's mute and volume controls, as Windows set them.
#[derive(Clone, Copy, Default)]
struct FeatureUnit {
    mute: bool,
    /// In 1/256 dB; `VOLUME_SILENT` is minus infinity.
    volume: i16,
}

/// The feature unit IDs in the descriptors: playback, then recording.
const FEATURE_UNITS: [u8; 2] = [2, 5];
/// The volume range the feature units report, in 1/256 dB.
const VOLUME_MIN: i16 = -60 * 256;
const VOLUME_MAX: i16 = 0;
const VOLUME_RES: i16 = 256;
/// UAC1's volume value for silence (minus infinity dB).
const VOLUME_SILENT: i16 = i16::MIN;

impl Device {
    /// Cable `number` with its default name and serial number.
    #[cfg(test)]
    pub fn new(number: u32, latency_ms: i64) -> Result<Device, String> {
        let (serial, name) = (format!("VCABLE-{number:03}"), format!("Virtual Cable {number:02}"));
        Device::named(number, &serial, &name, latency_ms)
    }

    /// Creates the cable with ID `number`, serial number `serial`, called
    /// `name`, whose recording side plays its playback side's audio
    /// `latency_ms` later.
    pub fn named(number: u32, serial: &str, name: &str, latency_ms: i64) -> Result<Device, String> {
        let descriptors = Descriptors::named(number, serial, name)?;
        let latency_ms = latency_ms.clamp(MIN_LATENCY_MS, MAX_LATENCY_MS);
        Ok(Device {
            number,
            bus_id: bus_id(number),
            descriptors,
            audio: DelayLine::new(latency_ms as u64),
            state: Mutex::new(DeviceState {
                configuration: 0,
                alt_setting: HashMap::from([(0, 0), (1, 0), (2, 0)]),
                sample_rate: 48000,
                features: [FeatureUnit::default(); 2],
            }),
        })
    }

    pub fn product(&self) -> &str {
        &self.descriptors.product
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
                    self.audio.end_playback();
                    self.audio.end_capture();
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
                    match (iface, alt) {
                        (1, 0) => self.audio.end_playback(),
                        (2, 0) => self.audio.end_capture(),
                        _ => {}
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

        // Feature-unit mute and volume. Windows treats them as hardware
        // controls, so they are applied to the audio on its way from the
        // playback side to the recording side.
        let entity_id = (setup.index >> 8) as u8;
        let interface_number = setup.index as u8;
        if recipient == 0x01
            && interface_number == 0
            && let Some(unit) = FEATURE_UNITS.iter().position(|&id| id == entity_id)
        {
            let reply = |value: Vec<u8>| (truncate(value, setup.length), 0);
            let feature = d.features[unit];
            let control_selector = (setup.value >> 8) as u8;
            match (control_selector, setup.request) {
                // MUTE_CONTROL
                (0x01, AUDIO_SET_CUR) => {
                    if let Some(&first) = out.first() {
                        d.features[unit].mute = first != 0;
                        self.audio.set_gain(feature_gain(&d.features));
                    }
                    return (Vec::new(), 0);
                }
                (0x01, AUDIO_GET_CUR) => return reply(vec![feature.mute as u8]),
                // VOLUME_CONTROL, signed 1/256 dB
                (0x02, AUDIO_SET_CUR) => {
                    if out.len() >= 2 {
                        let volume = i16::from_le_bytes([out[0], out[1]]);
                        d.features[unit].volume =
                            if volume == VOLUME_SILENT { volume } else { volume.clamp(VOLUME_MIN, VOLUME_MAX) };
                        self.audio.set_gain(feature_gain(&d.features));
                    }
                    return (Vec::new(), 0);
                }
                (0x02, AUDIO_GET_CUR) => return reply(feature.volume.to_le_bytes().to_vec()),
                (0x02, AUDIO_GET_MIN) => return reply(VOLUME_MIN.to_le_bytes().to_vec()),
                (0x02, AUDIO_GET_MAX) => return reply(VOLUME_MAX.to_le_bytes().to_vec()),
                (0x02, AUDIO_GET_RES) => return reply(VOLUME_RES.to_le_bytes().to_vec()),
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

    /// Stores a playback URB's packets, scheduled from bus frame `start`.
    /// Ignored until the playback interface is active: Windows can queue the
    /// first URB just before its SET_INTERFACE.
    pub fn write_playback<'a>(&self, start: u64, packets: impl IntoIterator<Item = &'a [u8]>) {
        if self.playback_active() {
            self.audio.write(start, packets);
        }
    }

    /// Fills a capture URB's packets, scheduled from bus frame `start` and
    /// laid out back to back in `out` with the given lengths. Silence while
    /// the recording interface is inactive.
    pub fn read_capture(&self, start: u64, out: &mut [u8], lengths: impl IntoIterator<Item = usize>) {
        let (capturing, playing) = {
            let d = self.lock();
            let active = |iface| d.configuration == 1 && d.alt_setting.get(&iface) == Some(&1);
            (active(2), active(1))
        };
        if !capturing {
            out.fill(0);
            return;
        }
        self.audio.read(start, out, lengths, playing);
    }
}

/// The gain of the playback and recording feature units in series.
fn feature_gain(features: &[FeatureUnit; 2]) -> Gain {
    if features.iter().any(|f| f.mute || f.volume == VOLUME_SILENT) {
        return Gain::Scale(0.0);
    }
    let volume: i32 = features.iter().map(|f| f.volume as i32).sum();
    if volume == 0 { Gain::Unity } else { Gain::Scale(10f32.powf(volume as f32 / 256.0 / 20.0)) }
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
        let d = Device::new(1, DEFAULT_LATENCY_MS).unwrap();
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
        let d = Device::new(1, DEFAULT_LATENCY_MS).unwrap();
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
        let d = Device::new(1, DEFAULT_LATENCY_MS).unwrap();
        let setup = SetupPacket { request_type: 0xA2, request: AUDIO_GET_CUR, value: 0x0100, index: 0x82, length: 3 };
        let (data, status) = d.handle_control(setup, &[]);
        assert_eq!((status, data.len()), (0, 3));
        let rate = data[0] as u32 | (data[1] as u32) << 8 | (data[2] as u32) << 16;
        assert_eq!(rate, 48000);
    }

    #[test]
    fn latency_is_kept_in_range() {
        assert_eq!(Device::new(1, 40).unwrap().audio.delay(), 40);
        assert_eq!(Device::new(1, 0).unwrap().audio.delay(), MIN_LATENCY_MS as u64);
        assert_eq!(Device::new(1, 5000).unwrap().audio.delay(), MAX_LATENCY_MS as u64);
    }

    /// The playback and recording volume and mute controls are separate, and
    /// both apply to the audio on its way through the cable.
    #[test]
    fn volume_and_mute_apply_to_the_loopback() {
        let d = Device::new(1, 10).unwrap();
        let control = |request_type, request, value, index, data: &[u8]| {
            let setup = SetupPacket { request_type, request, value, index, length: data.len().max(2) as u16 };
            d.handle_control(setup, data)
        };
        control(0x00, REQUEST_SET_CONFIGURATION, 1, 0, &[]);
        control(0x01, REQUEST_SET_INTERFACE, 1, 1, &[]);
        control(0x01, REQUEST_SET_INTERFACE, 1, 2, &[]);
        // Plays one sample of 0x400000 through the next frame and returns it.
        let frame = std::cell::Cell::new(1000);
        let sample = |d: &Device| {
            let mut packet = vec![0u8; crate::audio::PACKET_BYTES];
            packet[..3].copy_from_slice(&[0x00, 0x00, 0x40]);
            d.write_playback(frame.get(), [packet.as_slice()]);
            let mut out = vec![0u8; crate::audio::PACKET_BYTES];
            d.read_capture(frame.get() + 10, &mut out, [crate::audio::PACKET_BYTES]);
            frame.set(frame.get() + 1);
            i32::from_le_bytes([out[0], out[1], out[2], 0])
        };
        const PLAYBACK: u16 = 2 << 8;
        const RECORDING: u16 = 5 << 8;
        assert_eq!(sample(&d), 0x40_0000, "0 dB passes through unchanged");

        let minus_6_db = (-6i16 * 256).to_le_bytes();
        assert_eq!(control(0x21, AUDIO_SET_CUR, 0x0200, PLAYBACK, &minus_6_db).1, 0);
        let scaled = sample(&d) as f64 / 0x40_0000 as f64;
        assert!((scaled - 0.501).abs() < 0.002, "playback volume -6 dB gave {scaled}");
        assert_eq!(control(0xA1, AUDIO_GET_CUR, 0x0200, PLAYBACK, &[]).0, minus_6_db);
        assert_eq!(control(0xA1, AUDIO_GET_CUR, 0x0200, RECORDING, &[]).0, [0, 0], "units are separate");

        control(0x21, AUDIO_SET_CUR, 0x0100, RECORDING, &[1]);
        assert_eq!(sample(&d), 0, "recording mute");
        control(0x21, AUDIO_SET_CUR, 0x0100, RECORDING, &[0]);
        control(0x21, AUDIO_SET_CUR, 0x0200, PLAYBACK, &[0, 0]);
        assert_eq!(sample(&d), 0x40_0000, "back to unity");
        control(0x21, AUDIO_SET_CUR, 0x0200, RECORDING, &VOLUME_SILENT.to_le_bytes());
        assert_eq!(sample(&d), 0, "minus infinity dB is silence");
    }
}
