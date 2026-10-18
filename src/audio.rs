//! A cable's audio path: a delay line addressed by USB bus frame.
//!
//! Both endpoints of a cable run on the same bus frame clock. Windows sends
//! the playback endpoint exactly one 48-sample packet per 1 ms frame, and the
//! capture endpoint returns one per frame, so there is no clock drift to
//! correct. Each playback packet is stored under the frame it is scheduled
//! for, and the capture packet of frame F is the playback packet of frame
//! F - delay. Audio passes through bit for bit with a constant latency, and a
//! frame whose audio never arrived is silent for exactly that frame.

use std::sync::Mutex;

/// Bytes per PCM frame: stereo, 24-bit.
pub const FRAME_BYTES: usize = 6;
/// Bytes per 1 ms packet at 48 kHz: 48 PCM frames.
pub const PACKET_BYTES: usize = 48 * FRAME_BYTES;
/// Packets the delay line holds beyond the delay, for playback that Windows
/// queues ahead of time.
const QUEUE_SLOTS: u64 = 1024;
/// Marks a slot that holds no audio.
const EMPTY: u64 = u64::MAX;
/// How far a stream that was moved later must have caught up before it is
/// moved back, skipping the audio in between: a URB's length, so that it is
/// not moved for every millisecond it gains.
const CATCH_UP_FRAMES: u64 = 10;

/// The gain the capture side applies: the product of the cable's playback
/// and recording volume controls.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Gain {
    /// 0 dB on both controls: samples are copied unchanged.
    Unity,
    /// A linear factor; 0 for mute.
    Scale(f32),
}

pub struct DelayLine {
    delay: u64,
    slots: u64,
    state: Mutex<LineState>,
}

struct LineState {
    /// One packet per slot; the packet of frame F is in slot F % slots.
    audio: Vec<u8>,
    /// The frame whose audio each slot holds, or `EMPTY`.
    frames: Vec<u64>,
    playback: Option<Segment>,
    /// The first frame the running playback stream placed audio in.
    playback_first: Option<u64>,
    /// The latest frame the capture side has read.
    last_read: Option<u64>,
    gain: Gain,
    missing: u64,
    realigned: u64,
    caught_up: u64,
}

/// A run of contiguously scheduled playback packets.
#[derive(Clone, Copy, Debug)]
struct Segment {
    /// The scheduled frame the next packet continues from.
    next: u64,
    /// Added to scheduled frames once audio arrived too late to play at its
    /// own frame.
    offset: u64,
}

/// Counters for the log.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LineStatus {
    /// Frames of silence the capture side produced while playback was
    /// running: audio that arrived too late or never arrived.
    pub missing: u64,
    /// Times playback audio arrived after its frame had been played, and the
    /// rest of its stream was moved later to keep playing.
    pub realigned: u64,
    /// Times a stream that had been moved later caught up and was moved back
    /// toward its own frames.
    pub caught_up: u64,
}

impl DelayLine {
    /// A delay line that plays each frame's audio `delay` frames (ms) later.
    pub fn new(delay: u64) -> DelayLine {
        let slots = delay + QUEUE_SLOTS;
        DelayLine {
            delay,
            slots,
            state: Mutex::new(LineState {
                audio: vec![0; slots as usize * PACKET_BYTES],
                frames: vec![EMPTY; slots as usize],
                playback: None,
                playback_first: None,
                last_read: None,
                gain: Gain::Unity,
                missing: 0,
                realigned: 0,
                caught_up: 0,
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, LineState> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn delay(&self) -> u64 {
        self.delay
    }

    pub fn status(&self) -> LineStatus {
        let s = self.lock();
        LineStatus { missing: s.missing, realigned: s.realigned, caught_up: s.caught_up }
    }

    pub fn set_gain(&self, gain: Gain) {
        self.lock().gain = gain;
    }

    /// Stores playback packets scheduled on frames `start`, `start + 1`, ...
    /// An empty packet carries no audio for its frame.
    ///
    /// A packet that arrives after its frame has been played cannot play on
    /// time. Rather than drop it and every later packet of a stream that has
    /// fallen behind, the stream is moved later, so that it plays again with
    /// half the delay to spare; the frames in between are silent. Once the
    /// stream catches up, it is moved back toward its own frames, skipping
    /// the audio in between, so the cable returns to its normal latency.
    pub fn write<'a>(&self, start: u64, packets: impl IntoIterator<Item = &'a [u8]>) {
        let mut guard = self.lock();
        let s = &mut *guard;
        // A schedule that does not continue the previous URB is a new
        // segment, played at its own frames again.
        let mut segment = match s.playback {
            Some(segment) if segment.next == start => segment,
            _ => Segment { next: start, offset: 0 },
        };
        let mut scheduled = start;
        for packet in packets {
            let own = scheduled;
            scheduled += 1;
            let mut frame = own + segment.offset;
            if let Some(read) = s.last_read {
                // The earliest frame that leaves half the delay to spare, and
                // never earlier than the packet's own frame.
                let earliest = (read + 1 + self.delay / 2).max(own);
                if frame <= read {
                    segment.offset = earliest - own;
                    frame = earliest;
                    s.realigned += 1;
                } else if segment.offset > 0 && frame >= earliest + CATCH_UP_FRAMES {
                    segment.offset = earliest - own;
                    frame = earliest;
                    s.caught_up += 1;
                }
                // Never overwrite audio the capture side has yet to read.
                if frame >= read + self.slots {
                    continue;
                }
            }
            s.playback_first.get_or_insert(frame);
            s.store(self.slots, frame, packet);
        }
        segment.next = scheduled;
        s.playback = Some(segment);
    }

    /// Fills capture packets for frames `start`, `start + 1`, ..., laid out
    /// back to back in `out` with the given lengths, with the playback audio
    /// of `delay` frames earlier. While `playback_active`, frames the
    /// playback stream did not supply are counted as missing.
    pub fn read(&self, start: u64, out: &mut [u8], lengths: impl IntoIterator<Item = usize>, playback_active: bool) {
        let mut guard = self.lock();
        let s = &mut *guard;
        let mut pos = 0;
        for (capture_frame, length) in (start..).zip(lengths) {
            let length = length.min(out.len() - pos);
            let dst = &mut out[pos..pos + length];
            pos += length;
            let Some(frame) = capture_frame.checked_sub(self.delay) else {
                dst.fill(0);
                continue;
            };
            s.last_read = Some(s.last_read.map_or(frame, |read| read.max(frame)));
            let slot = (frame % self.slots) as usize;
            if s.frames[slot] == frame {
                let src = &s.audio[slot * PACKET_BYTES..(slot + 1) * PACKET_BYTES];
                let n = length.min(PACKET_BYTES) / FRAME_BYTES * FRAME_BYTES;
                apply_gain(s.gain, &src[..n], &mut dst[..n]);
                dst[n..].fill(0);
            } else {
                dst.fill(0);
                if playback_active && s.playback_first.is_some_and(|first| frame >= first) {
                    s.missing += 1;
                }
            }
        }
        out[pos..].fill(0);
    }

    /// The playback stream has stopped. Its queued audio still plays.
    pub fn end_playback(&self) {
        let mut s = self.lock();
        s.playback = None;
        s.playback_first = None;
    }

    /// The capture stream has stopped.
    pub fn end_capture(&self) {
        self.lock().last_read = None;
    }
}

impl LineState {
    fn store(&mut self, slots: u64, frame: u64, packet: &[u8]) {
        if packet.is_empty() {
            return;
        }
        let slot = (frame % slots) as usize;
        let dst = &mut self.audio[slot * PACKET_BYTES..(slot + 1) * PACKET_BYTES];
        let n = packet.len().min(PACKET_BYTES) / FRAME_BYTES * FRAME_BYTES;
        dst[..n].copy_from_slice(&packet[..n]);
        dst[n..].fill(0);
        self.frames[slot] = frame;
    }
}

/// Copies packed 24-bit samples from `src` to `dst`, scaled by `gain`.
fn apply_gain(gain: Gain, src: &[u8], dst: &mut [u8]) {
    let factor = match gain {
        Gain::Unity => {
            dst.copy_from_slice(src);
            return;
        }
        Gain::Scale(factor) => factor,
    };
    for (from, to) in src.as_chunks::<3>().0.iter().zip(dst.as_chunks_mut::<3>().0) {
        let sample = decode_int24(from[0], from[1], from[2]);
        encode_int24(to, (sample as f32 * factor).round() as i32);
    }
}

fn decode_int24(low: u8, middle: u8, high: u8) -> i32 {
    let mut value = low as i32 | (middle as i32) << 8 | (high as i32) << 16;
    if value & 0x80_0000 != 0 {
        value |= !0xff_ffff;
    }
    value
}

fn encode_int24(dst: &mut [u8], value: i32) {
    let value = value.clamp(-0x80_0000, 0x7f_ffff);
    dst[0] = value as u8;
    dst[1] = (value >> 8) as u8;
    dst[2] = (value >> 16) as u8;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A packet whose every byte identifies its playback frame.
    fn packet(frame: u64) -> Vec<u8> {
        vec![(frame % 251) as u8 + 1; PACKET_BYTES]
    }

    /// Reads `count` capture packets from frame `start` and returns them.
    fn read(line: &DelayLine, start: u64, count: usize, playback_active: bool) -> Vec<Vec<u8>> {
        let mut out = vec![0xEE; count * PACKET_BYTES];
        line.read(start, &mut out, std::iter::repeat_n(PACKET_BYTES, count), playback_active);
        out.chunks(PACKET_BYTES).map(<[u8]>::to_vec).collect()
    }

    fn write(line: &DelayLine, start: u64, count: u64) {
        let packets: Vec<Vec<u8>> = (start..start + count).map(packet).collect();
        line.write(start, packets.iter().map(Vec::as_slice));
    }

    #[test]
    fn passes_audio_through_unchanged_after_the_delay() {
        let line = DelayLine::new(40);
        for urb in 0..20 {
            write(&line, 1000 + urb * 10, 10);
        }
        // Capture frame 1050 plays playback frame 1010.
        let got = read(&line, 1050, 10, true);
        for (i, packet_read) in got.iter().enumerate() {
            assert_eq!(packet_read, &packet(1010 + i as u64), "capture frame {}", 1050 + i);
        }
        assert_eq!(line.status(), LineStatus::default());
    }

    #[test]
    fn frames_without_audio_are_silent_and_counted_only_while_playing() {
        let line = DelayLine::new(10);
        write(&line, 1000, 10);
        // Before the stream: silent, not missing.
        assert_eq!(read(&line, 1005, 5, true), vec![vec![0; PACKET_BYTES]; 5]);
        assert_eq!(line.status().missing, 0);
        // A lost URB: exactly its frames are silent.
        write(&line, 1020, 10);
        let got = read(&line, 1015, 20, true);
        assert_eq!(got[4], packet(1009));
        assert!(got[5..15].iter().all(|p| p == &vec![0; PACKET_BYTES]));
        assert_eq!(got[15], packet(1020));
        assert_eq!(line.status().missing, 10);
        // Nothing is missing when no playback stream is running.
        line.end_playback();
        read(&line, 1040, 10, false);
        assert_eq!(line.status().missing, 10);
    }

    #[test]
    fn late_stream_is_realigned_instead_of_dropped() {
        let line = DelayLine::new(20);
        write(&line, 1000, 10);
        read(&line, 1020, 10, true); // plays frames 1000-1009
        read(&line, 1030, 10, true); // frames 1010-1019: nothing arrived
        // The stream continues contiguously but late: frame 1010 was already
        // played. It now plays from frame 1030: after the last frame read,
        // 1019, with half the delay to spare.
        write(&line, 1010, 10);
        assert_eq!(line.status().realigned, 1);
        let gap = read(&line, 1040, 10, true);
        assert!(gap.iter().all(|p| p == &vec![0; PACKET_BYTES]), "gap before the realigned stream");
        // Its next URB arrives just as late, so it follows on.
        write(&line, 1020, 10);
        let got = read(&line, 1050, 20, true);
        for (i, packet_read) in got.iter().enumerate() {
            assert_eq!(packet_read, &packet(1010 + i as u64), "realigned frame {i}");
        }
        assert_eq!(line.status(), LineStatus { missing: 20, realigned: 1, caught_up: 0 });
    }

    /// A stream that was moved later returns to its own frames once it
    /// catches up, skipping the audio in between, so the latency recovers.
    #[test]
    fn caught_up_stream_returns_to_its_own_frames() {
        let line = DelayLine::new(20);
        write(&line, 1000, 10);
        read(&line, 1020, 10, true);
        read(&line, 1030, 10, true); // frames 1010-1019: nothing arrived
        write(&line, 1010, 10); // late: moved to frame 1030
        // While it stays as late, it stays where it was moved.
        read(&line, 1040, 10, true);
        write(&line, 1020, 10);
        assert_eq!(read(&line, 1050, 10, true)[0], packet(1010));
        assert_eq!(line.status().caught_up, 0);

        // The host catches up: its next URBs arrive well ahead of their frames.
        // The stream moves back in steps of at least a URB: first as far as
        // the frames already read allow, then to its own frames.
        write(&line, 1030, 10);
        write(&line, 1040, 10);
        write(&line, 1050, 10);
        assert_eq!(line.status().caught_up, 2);
        // Frames 1040-1049 still hold the stream as moved; then it jumps to
        // its own frames, skipping packets 1030-1049.
        let got = read(&line, 1060, 20, true);
        assert_eq!(got[0], packet(1020));
        assert_eq!(got[10], packet(1050), "back at its own frame");
        write(&line, 1060, 10);
        assert_eq!(read(&line, 1080, 1, true)[0], packet(1060), "and stays there");
    }

    #[test]
    fn a_new_stream_starts_at_its_own_frames() {
        let line = DelayLine::new(20);
        write(&line, 1000, 10);
        read(&line, 1020, 10, true);
        read(&line, 1030, 10, true);
        write(&line, 1010, 10); // late: moved 20 frames later
        // A restarted stream is not contiguous, so it drops the old offset.
        write(&line, 1100, 10);
        assert_eq!(read(&line, 1120, 1, true)[0], packet(1100));
    }

    #[test]
    fn stale_slots_are_not_replayed() {
        let line = DelayLine::new(10);
        write(&line, 1000, 10);
        // The same slot one full cycle later holds a different frame.
        let later = 1000 + line.slots;
        assert_eq!(read(&line, later + 10, 1, false)[0], vec![0; PACKET_BYTES]);
    }

    #[test]
    fn unread_audio_is_never_overwritten() {
        let line = DelayLine::new(10);
        read(&line, 1010, 1, false); // last read: frame 1000
        write(&line, 1001, 1);
        write(&line, 1001 + line.slots, 1); // would land in frame 1001's slot
        assert_eq!(read(&line, 1011, 1, false)[0], packet(1001));
    }

    #[test]
    fn short_and_empty_packets() {
        let line = DelayLine::new(10);
        let short = vec![7; 100]; // 16 whole PCM frames and a partial one
        line.write(1000, [short.as_slice(), &[]]);
        let got = read(&line, 1010, 2, false);
        assert!(got[0][..96].iter().all(|&b| b == 7) && got[0][96..].iter().all(|&b| b == 0));
        assert_eq!(got[1], vec![0; PACKET_BYTES], "an empty packet carries no audio");
    }

    #[test]
    fn capture_packet_layout_follows_the_lengths() {
        let line = DelayLine::new(10);
        write(&line, 1000, 2);
        let mut out = vec![0xEE; 400];
        line.read(1010, &mut out, [PACKET_BYTES, 60], false);
        assert_eq!(&out[..PACKET_BYTES], packet(1000).as_slice());
        assert_eq!(&out[PACKET_BYTES..PACKET_BYTES + 60], &packet(1001)[..60]);
        assert!(out[PACKET_BYTES + 60..].iter().all(|&b| b == 0), "the remainder is silence");
    }

    #[test]
    fn gain_scales_samples_and_unity_is_exact() {
        let line = DelayLine::new(1);
        let mut samples = Vec::new();
        for value in [0x40_0000, -0x40_0000, 1, -1, 0x7f_ffff] {
            let mut b = [0; 3];
            encode_int24(&mut b, value);
            samples.extend_from_slice(&b);
        }
        samples.resize(PACKET_BYTES, 0);
        line.write(1000, [samples.as_slice()]);
        assert_eq!(read(&line, 1001, 1, false)[0], samples, "unity gain");
        line.set_gain(Gain::Scale(0.5));
        let half = read(&line, 1001, 1, false).remove(0);
        let value = |i: usize| decode_int24(half[i * 3], half[i * 3 + 1], half[i * 3 + 2]);
        assert_eq!((value(0), value(1), value(4)), (0x20_0000, -0x20_0000, 0x40_0000));
        line.set_gain(Gain::Scale(0.0));
        assert_eq!(read(&line, 1001, 1, false)[0], vec![0; PACKET_BYTES], "mute");
    }

    #[test]
    fn int24_round_trip_and_clamp() {
        let mut b = [0; 3];
        for v in [0, 1, -1, 0x7f_ffff, -0x80_0000, 12345, -54321] {
            encode_int24(&mut b, v);
            assert_eq!(decode_int24(b[0], b[1], b[2]), v);
        }
        encode_int24(&mut b, 0x100_0000);
        assert_eq!(decode_int24(b[0], b[1], b[2]), 0x7f_ffff);
    }
}
