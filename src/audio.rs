//! Elastic PCM ring buffer between a cable's playback and capture endpoints.

use std::sync::Mutex;

/// A bounded, thread-safe byte ring buffer. When the frame size is greater
/// than one, writes and overrun recovery preserve complete PCM sample frames.
pub struct Ring {
    frame_size: usize,
    capacity: usize,
    state: Mutex<RingState>,
}

struct RingState {
    buf: Vec<u8>,
    read: usize,
    write: usize,
    size: usize,
    dropped: u64,
    underruns: u64,
    phase: f64,
    last_output: [i32; 2],
    fade_from: [i32; 2],
    fade_in: usize,
    fade_out: usize,
    underrun: bool,
    needs_fade: bool,
    filtered_fill: f64,
    correction: f64,
    correction_integral: f64,
    started: bool,
    primed: bool,
    /// The reserve the reader keeps. It grows when stalls repeat, and steps
    /// back toward the configured reserve once the stream has been stable.
    reserve_frames: usize,
    /// Frames the reader has produced since it was first primed, silence
    /// included: the clock for stall timing.
    clock: u64,
    /// Silent frames produced in the current outage, until audio arrives again.
    outage: Option<u64>,
    last_stall: Option<u64>,
    settled_since: u64,
    stalls: u64,
    overruns: u64,
}

/// A snapshot of the ring's health, for the log.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RingStatus {
    pub buffered_frames: usize,
    /// The reserve the reader keeps, in frames; 0 before its first read.
    pub reserve_frames: usize,
    pub correction_ppm: i64,
    /// Underruns the reader recovered from: stalls, not the source stopping.
    pub underruns: u64,
    /// Writes that discarded unread audio because the ring was full.
    pub overruns: u64,
}

const CLICK_FADE_FRAMES: usize = 48;

/// An outage up to this long is a stall; a longer one is the source stopping,
/// which says nothing about whether the reserve is large enough.
const MAX_STALL_FRAMES: u64 = 48000;

/// Two stalls this close together mean the reserve is too small for the
/// current system load, rather than a one-off hiccup.
const REPEAT_STALL_FRAMES: u64 = 10 * 48000;

/// After this long without a stall, a grown reserve steps back toward the
/// configured one.
const RELAX_FRAMES: u64 = 120 * 48000;

impl Ring {
    /// Creates a byte-oriented ring, for callers that do not carry framed audio.
    pub fn new(capacity: usize) -> Ring {
        Ring::new_framed(capacity, 1)
    }

    /// Creates a ring that preserves complete frames when dropping data under
    /// pressure. For stereo 24-bit PCM, the frame size is 6 bytes.
    pub fn new_framed(capacity: usize, frame_size: usize) -> Ring {
        let frame_size = frame_size.max(1);
        let capacity = capacity.max(frame_size) / frame_size * frame_size;
        Ring {
            frame_size,
            capacity,
            state: Mutex::new(RingState {
                buf: vec![0; capacity],
                read: 0,
                write: 0,
                size: 0,
                dropped: 0,
                underruns: 0,
                phase: 0.0,
                last_output: [0; 2],
                fade_from: [0; 2],
                fade_in: 0,
                fade_out: 0,
                underrun: false,
                needs_fade: false,
                filtered_fill: 0.0,
                correction: 0.0,
                correction_integral: 0.0,
                started: false,
                primed: false,
                reserve_frames: 0,
                clock: 0,
                outage: None,
                last_stall: None,
                settled_since: 0,
                stalls: 0,
                overruns: 0,
            }),
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, RingState> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn available(&self) -> usize {
        self.lock().size
    }

    /// Returns the buffered byte count and the dropped and underrun byte totals.
    pub fn stats(&self) -> (usize, u64, u64) {
        let r = self.lock();
        (r.size, r.dropped, r.underruns)
    }

    pub fn correction_ppm(&self) -> i64 {
        (self.lock().correction * 1_000_000.0).round() as i64
    }

    pub fn status(&self) -> RingStatus {
        let r = self.lock();
        RingStatus {
            buffered_frames: r.size / self.frame_size,
            reserve_frames: r.reserve_frames,
            correction_ppm: (r.correction * 1_000_000.0).round() as i64,
            underruns: r.stalls,
            overruns: r.overruns,
        }
    }

    /// Keeps the newest complete frames. If the ring is full, old data is
    /// discarded in whole frames; dropping individual bytes corrupts 24-bit PCM
    /// sample/channel alignment and can cause audible crackling.
    pub fn write(&self, p: &[u8]) -> usize {
        let frame_size = self.frame_size;
        let mut r = self.lock();
        let r = &mut *r;

        let complete_len = p.len() / frame_size * frame_size;
        let mut discontinuity = complete_len != p.len();
        if complete_len != p.len() {
            r.dropped += (p.len() - complete_len) as u64;
        }
        let mut p = &p[..complete_len];
        let mut overran = false;
        if p.is_empty() {
            if discontinuity {
                r.needs_fade = true;
            }
            return 0;
        }

        // If a single write exceeds capacity, retain only its newest whole frames.
        let capacity = r.buf.len();
        if p.len() > capacity {
            let discard = p.len() - capacity;
            p = &p[discard..];
            r.dropped += discard as u64;
            discontinuity = true;
            overran = true;
        }

        // Make room in complete frames, keeping both read position and buffer
        // size aligned to PCM frame boundaries.
        while capacity - r.size < p.len() {
            r.read = (r.read + frame_size) % capacity;
            r.size -= frame_size;
            r.dropped += frame_size as u64;
            discontinuity = true;
            overran = true;
        }
        // A ring nobody reads yet fills up as a matter of course.
        if overran && r.primed {
            r.overruns += 1;
        }

        for &b in p {
            let write = r.write;
            r.buf[write] = b;
            r.write = (write + 1) % capacity;
        }
        r.size += p.len();
        if discontinuity {
            r.needs_fade = true;
        }
        p.len()
    }

    /// Reads complete frames and fills unavailable frames with digital silence.
    /// The caller should request a whole number of frames, as USB PCM transfers
    /// normally do. Any non-frame tail is silenced rather than returned as a
    /// partial sample.
    pub fn read_silence(&self, p: &mut [u8]) -> usize {
        let frame_size = self.frame_size;
        let mut r = self.lock();
        read_silence_locked(&mut r, frame_size, p)
    }

    /// Reads stereo packed 24-bit PCM from an elastic buffer. Occupancy
    /// feedback tracks clock drift without reacting sharply to packet jitter.
    /// Both startup and recovery wait for the full reserve: `target_frames`,
    /// grown for as long as repeated stalls keep emptying the ring.
    pub fn read_adaptive_pcm24(&self, p: &mut [u8], target_frames: usize) -> usize {
        const FRAME_SIZE: usize = 6;
        const FADE_FRAMES: usize = CLICK_FADE_FRAMES;
        let mut guard = self.lock();
        let r = &mut *guard;
        if self.frame_size != FRAME_SIZE {
            return read_silence_locked(r, self.frame_size, p);
        }

        let complete_len = p.len() / FRAME_SIZE * FRAME_SIZE;
        p[complete_len..].fill(0);
        if complete_len == 0 {
            r.underruns += p.len() as u64;
            return 0;
        }

        let capacity = r.buf.len();
        let max_target_frames = (capacity / FRAME_SIZE / 2).max(2);
        let base_frames = target_frames.clamp(2, max_target_frames);
        r.reserve_frames = r.reserve_frames.clamp(base_frames, max_target_frames);
        let frames = (complete_len / FRAME_SIZE) as u64;

        let mut available_frames = r.size / FRAME_SIZE;
        if !r.started {
            // Audio is arriving again. If the outage was a stall rather than
            // the source stopping, grow the reserve before refilling it.
            if available_frames > 0
                && let Some(silence) = r.outage.take()
                && silence <= MAX_STALL_FRAMES
            {
                r.note_stall(max_target_frames);
            }
            if available_frames < r.reserve_frames {
                p[..complete_len].fill(0);
                if r.primed {
                    // Count recovery silence as well as frames lost after the ring empties.
                    r.underruns += complete_len as u64;
                    if let Some(silence) = &mut r.outage {
                        *silence += frames;
                    }
                    r.tick(frames, base_frames);
                }
                return 0;
            }
            // Maintain the target reserve after recovery to absorb the same timing jitter.
            r.started = true;
            r.primed = true;
            // The controller wound up against a supply shortfall it could not
            // correct, so its estimate says nothing about clock drift. Discard it
            // rather than unwinding it slowly once the supply is back to real time.
            r.correction = 0.0;
            r.correction_integral = 0.0;
            r.filtered_fill = available_frames as f64;
            r.phase = 0.0;
            r.needs_fade = true;
        } else {
            // Filter packet-level queue jitter, then use a PI controller: the
            // proportional term responds to fill changes while the integral term
            // learns sustained clock offsets without forcing the queue toward empty.
            let frames = (complete_len / FRAME_SIZE) as f64;
            let dt = frames / 48000.0;
            let fill_alpha = 1.0 - (-dt / 0.5).exp();
            r.filtered_fill += (available_frames as f64 - r.filtered_fill) * fill_alpha;
            let target_frames = r.reserve_frames as f64;
            let error_ratio = (r.filtered_fill - target_frames) / target_frames;
            const PROPORTIONAL_GAIN: f64 = 0.01;
            const INTEGRAL_GAIN: f64 = 0.0005;
            const MAX_RATE_CORRECTION: f64 = 0.005;

            let integral = (r.correction_integral + error_ratio * INTEGRAL_GAIN * dt)
                .clamp(-MAX_RATE_CORRECTION, MAX_RATE_CORRECTION);
            let mut desired_correction = error_ratio * PROPORTIONAL_GAIN + integral;
            if desired_correction > MAX_RATE_CORRECTION {
                desired_correction = MAX_RATE_CORRECTION;
                if error_ratio <= 0.0 {
                    r.correction_integral = integral;
                }
            } else if desired_correction < -MAX_RATE_CORRECTION {
                desired_correction = -MAX_RATE_CORRECTION;
                if error_ratio >= 0.0 {
                    r.correction_integral = integral;
                }
            } else {
                r.correction_integral = integral;
            }

            let correction_alpha = 1.0 - (-dt / 0.25).exp();
            r.correction += (desired_correction - r.correction) * correction_alpha;
        }
        let input_per_output = 1.0 + r.correction;
        let mut consumed = 0;

        let mut out = 0;
        while out < complete_len {
            available_frames = r.size / FRAME_SIZE;
            if available_frames == 0 {
                r.started = false;
                if !r.underrun {
                    r.underrun = true;
                    r.fade_out = FADE_FRAMES;
                    r.outage = Some(0);
                }
                if let Some(silence) = &mut r.outage {
                    *silence += 1;
                }
                let mut gain = 0.0;
                if r.fade_out > 0 {
                    gain = (r.fade_out - 1) as f64 / FADE_FRAMES as f64;
                    r.fade_out -= 1;
                }
                for channel in 0..2 {
                    let value = (r.last_output[channel] as f64 * gain).round() as i32;
                    r.last_output[channel] = value;
                    encode_int24(&mut p[out + channel * 3..out + channel * 3 + 3], value);
                }
                r.underruns += FRAME_SIZE as u64;
                if r.fade_out == 0 {
                    r.needs_fade = true;
                }
                r.phase = 0.0;
                out += FRAME_SIZE;
                continue;
            }
            if r.underrun {
                r.underrun = false;
                r.fade_out = 0;
                r.needs_fade = true;
            }
            if r.needs_fade && r.fade_in == 0 {
                r.fade_from = r.last_output;
                r.fade_in = FADE_FRAMES;
                r.needs_fade = false;
            }
            let mut fade_gain = -1.0;
            if r.fade_in > 0 {
                fade_gain = (FADE_FRAMES - r.fade_in + 1) as f64 / FADE_FRAMES as f64;
                r.fade_in -= 1;
            }

            let first = r.read;
            let second = (first + FRAME_SIZE) % capacity;
            for channel in 0..2 {
                let offset = channel * 3;
                let a = decode_int24(r.buf[first + offset], r.buf[first + offset + 1], r.buf[first + offset + 2]);
                let mut b = a;
                if available_frames > 1 {
                    b = decode_int24(r.buf[second + offset], r.buf[second + offset + 1], r.buf[second + offset + 2]);
                }
                let mut value = (a as f64 + (b - a) as f64 * r.phase).round() as i32;
                if fade_gain >= 0.0 {
                    let from = r.fade_from[channel] as f64;
                    value = (from + (value as f64 - from) * fade_gain).round() as i32;
                }
                r.last_output[channel] = value;
                encode_int24(&mut p[out + offset..out + offset + 3], value);
            }

            let position = r.phase + input_per_output;
            let mut advance = position as usize;
            r.phase = position - advance as f64;
            if advance > available_frames {
                advance = available_frames;
                r.phase = 0.0;
            }
            if advance > 0 {
                r.read = (r.read + advance * FRAME_SIZE) % capacity;
                r.size -= advance * FRAME_SIZE;
                consumed += advance * FRAME_SIZE;
            }
            out += FRAME_SIZE;
        }
        if complete_len < p.len() {
            r.underruns += (p.len() - complete_len) as u64;
        }
        r.tick(frames, base_frames);
        consumed
    }

    /// Clears the stream. The reserve grown by repeated stalls is kept: it
    /// reflects the system's load rather than this stream.
    pub fn reset(&self) {
        let mut r = self.lock();
        r.read = 0;
        r.write = 0;
        r.size = 0;
        r.phase = 0.0;
        r.last_output = [0; 2];
        r.fade_from = [0; 2];
        r.fade_in = 0;
        r.fade_out = 0;
        r.underrun = false;
        r.needs_fade = false;
        r.filtered_fill = 0.0;
        r.correction = 0.0;
        r.correction_integral = 0.0;
        r.started = false;
        r.primed = false;
        r.outage = None;
        r.last_stall = None;
    }
}

impl RingState {
    /// Records a stall the reader recovered from. A second stall within
    /// `REPEAT_STALL_FRAMES` grows the reserve by half, up to `max_frames`.
    fn note_stall(&mut self, max_frames: usize) {
        self.stalls += 1;
        if self.last_stall.is_some_and(|at| self.clock - at <= REPEAT_STALL_FRAMES) {
            self.reserve_frames = (self.reserve_frames + self.reserve_frames / 2).min(max_frames);
        }
        self.last_stall = Some(self.clock);
        self.settled_since = self.clock;
    }

    /// Advances the stall clock by `frames` the reader produced. Each
    /// `RELAX_FRAMES` without a stall undoes one growth step, down to
    /// `base_frames`; the rate controller then drains the excess gently.
    fn tick(&mut self, frames: u64, base_frames: usize) {
        self.clock += frames;
        if self.reserve_frames > base_frames && self.clock - self.settled_since >= RELAX_FRAMES {
            self.reserve_frames = (self.reserve_frames * 2 / 3).max(base_frames);
            self.settled_since = self.clock;
        }
    }
}

fn read_silence_locked(r: &mut RingState, frame_size: usize, p: &mut [u8]) -> usize {
    let complete_len = p.len() / frame_size * frame_size;
    p[complete_len..].fill(0);
    let capacity = r.buf.len();
    let mut read = 0;
    let mut i = 0;
    while i < complete_len {
        if r.size < frame_size {
            p[i..complete_len].fill(0);
            r.underruns += (complete_len - i) as u64;
            break;
        }
        for j in 0..frame_size {
            p[i + j] = r.buf[r.read];
            r.read = (r.read + 1) % capacity;
        }
        r.size -= frame_size;
        read += frame_size;
        i += frame_size;
    }
    if complete_len < p.len() {
        r.underruns += (p.len() - complete_len) as u64;
    }
    read
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

    #[test]
    fn adaptive_read_discards_correction_after_underrun() {
        const TARGET: usize = 480; // 10 ms
        let r = Ring::new_framed(48000 * 6, 6);
        r.write(&vec![0; TARGET * 6]);
        let mut out = vec![0; 48 * 6]; // 1 ms
        // Starve the reader so the controller winds up toward its limit.
        for i in 0..2000 {
            if i % 2 == 0 {
                r.write(&vec![0; 48 * 6]);
            }
            r.read_adaptive_pcm24(&mut out, TARGET);
        }
        assert_ne!(r.correction_ppm(), 0, "controller did not respond to the shortfall");
        // The repeated stalls have also grown the reserve.
        r.write(&vec![0; r.status().reserve_frames * 6]);
        r.read_adaptive_pcm24(&mut out, TARGET);
        assert_eq!(r.correction_ppm(), 0, "correction after recovery");
    }

    /// Streams `ms` milliseconds in real time, 1 ms per packet, with the
    /// writer skipping the packets where `gap` holds.
    fn stream(r: &Ring, target: usize, ms: usize, gap: impl Fn(usize) -> bool) {
        let mut out = vec![0; 48 * 6];
        for i in 0..ms {
            if !gap(i) {
                r.write(&[0; 48 * 6]);
            }
            r.read_adaptive_pcm24(&mut out, target);
        }
    }

    #[test]
    fn repeated_stalls_grow_reserve() {
        const TARGET: usize = 480; // 10 ms
        let r = Ring::new_framed(48000 * 6, 6);
        stream(&r, TARGET, 1000, |_| false);
        assert_eq!(r.status().underruns, 0, "underran while streaming in real time");

        // A one-off stall is recovered at the configured reserve.
        stream(&r, TARGET, 2000, |i| i < 30);
        assert_eq!((r.status().underruns, r.status().reserve_frames), (1, TARGET));

        // A second one within a few seconds grows the reserve, and the ring refills to it.
        stream(&r, TARGET, 2000, |i| i < 30);
        let status = r.status();
        assert_eq!((status.underruns, status.reserve_frames), (2, TARGET * 3 / 2));
        assert!(status.buffered_frames.abs_diff(TARGET * 3 / 2) <= 48, "buffered {} frames", status.buffered_frames);

        // The grown reserve now absorbs the same stall.
        stream(&r, TARGET, 2000, |i| i < 12);
        assert_eq!(r.status().underruns, 2);
    }

    #[test]
    fn stable_stream_relaxes_reserve() {
        const TARGET: usize = 480;
        let r = Ring::new_framed(48000 * 6, 6);
        stream(&r, TARGET, 1000, |_| false);
        stream(&r, TARGET, 1000, |i| i < 30);
        stream(&r, TARGET, 1000, |i| i < 30);
        assert_eq!(r.status().reserve_frames, 720);
        stream(&r, TARGET, 119_000, |_| false);
        assert_eq!(r.status().reserve_frames, 720, "relaxed early");
        stream(&r, TARGET, 2000, |_| false);
        assert_eq!(r.status().reserve_frames, TARGET);
    }

    #[test]
    fn source_stopping_is_not_a_stall() {
        const TARGET: usize = 480;
        let r = Ring::new_framed(48000 * 6, 6);
        stream(&r, TARGET, 1000, |_| false);
        // A paused source: playback stops for longer than any stall.
        stream(&r, TARGET, 3000, |i| i < 1500);
        stream(&r, TARGET, 3000, |i| i < 1500);
        let status = r.status();
        assert_eq!((status.underruns, status.reserve_frames), (0, TARGET));
    }

    #[test]
    fn overruns_count_only_while_read() {
        let r = Ring::new_framed(48 * 6 * 20, 6); // 20 ms
        r.write(&[0; 48 * 6 * 30]);
        assert_eq!(r.status().overruns, 0, "nothing reads the ring yet");
        stream(&r, 240, 10, |_| false);
        r.write(&[0; 48 * 6 * 30]);
        assert_eq!(r.status().overruns, 1);
    }

    #[test]
    fn ring_round_trip() {
        let r = Ring::new(16);
        let input = [1, 2, 3, 4];
        r.write(&input);
        let mut out = [0; 4];
        r.read_silence(&mut out);
        assert_eq!(out, input);
    }

    #[test]
    fn ring_overrun_keeps_newest() {
        let r = Ring::new(4);
        r.write(&[1, 2, 3, 4, 5, 6]);
        let mut out = [0; 4];
        r.read_silence(&mut out);
        assert_eq!(out, [3, 4, 5, 6]);
    }

    #[test]
    fn ring_pads_silence() {
        let r = Ring::new(4);
        r.write(&[7, 8]);
        let mut out = [0; 4];
        r.read_silence(&mut out);
        assert_eq!(out, [7, 8, 0, 0]);
    }

    #[test]
    fn frame_ring_overrun_preserves_frame_alignment() {
        let r = Ring::new_framed(12, 6);
        r.write(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]);
        r.write(&[13, 14, 15, 16, 17, 18]);
        let mut out = [0; 12];
        r.read_silence(&mut out);
        assert_eq!(out, [7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18]);
    }

    #[test]
    fn frame_ring_underrun_pads_whole_frames() {
        let r = Ring::new_framed(12, 6);
        r.write(&[1, 2, 3, 4, 5, 6]);
        let mut out = [0; 12];
        r.read_silence(&mut out);
        assert_eq!(out, [1, 2, 3, 4, 5, 6, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn frame_ring_discards_incomplete_input_tail() {
        let r = Ring::new_framed(12, 6);
        assert_eq!(r.write(&[1, 2, 3, 4, 5, 6, 7]), 6);
        let mut out = [0; 6];
        r.read_silence(&mut out);
        assert_eq!(out, [1, 2, 3, 4, 5, 6]);
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
