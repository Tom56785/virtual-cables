use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::*;
use crate::audio::LineStatus;
use crate::uac2::{
    DEFAULT_LATENCY_MS, Device, REQUEST_CLEAR_FEATURE, REQUEST_SET_CONFIGURATION, REQUEST_SET_INTERFACE,
};
use crate::usbip::busclock::TICKS_PER_FRAME;

fn test_bus_time(frame: u64) -> BusTime {
    BusTime { wall: Instant::now(), coarse: frame * TICKS_PER_FRAME, precise: frame * TICKS_PER_FRAME }
}

/// A writer whose output the test can inspect.
#[derive(Clone, Default)]
struct SharedBuf(Arc<Mutex<Vec<u8>>>);

impl Write for SharedBuf {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl SharedBuf {
    fn bytes(&self) -> Vec<u8> {
        self.0.lock().unwrap().clone()
    }
}

fn discard_state() -> ConnState {
    ConnState::new(Box::new(io::sink()))
}

fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

const CLEAR_HALT: SetupPacket =
    SetupPacket { request_type: 0x02, request: REQUEST_CLEAR_FEATURE, value: 0, index: 0x01, length: 0 };

fn token() -> Arc<CancelToken> {
    Arc::new(CancelToken::new(None))
}

#[test]
fn iso_completions_are_serialized_per_endpoint() {
    let state = discard_state();
    let now = test_bus_time(1000);
    let first = state.reserve_iso_completion(2, 10, now);
    let second = state.reserve_iso_completion(2, 10, now);
    let other_endpoint = state.reserve_iso_completion(1, 10, now);

    // A new stream starts on the next frame and completes when its last frame ends.
    assert_eq!(first.start_frame, 1001);
    assert_eq!(first.deadline.unwrap() - now.wall, Duration::from_millis(11));
    assert_eq!(second.start_frame, 1011);
    assert_eq!(second.deadline.unwrap() - now.wall, Duration::from_millis(21));
    assert_eq!(other_endpoint.start_frame, 1001, "independent endpoint");
}

#[test]
fn late_iso_submit_stays_contiguous() {
    let state = discard_state();
    state.reserve_iso_completion(1, 10, test_bus_time(1000)); // frames 1001-1010

    // Submitted 190 frames after the previous URB ended: keep the stream
    // position and complete as soon as possible.
    let late = state.reserve_iso_completion(1, 10, test_bus_time(1200));
    assert_eq!((late.start_frame, late.resynced), (1011, false));

    // Far behind: the stream stopped, so restart on the next bus frame.
    let stalled = state.reserve_iso_completion(1, 10, test_bus_time(5000));
    assert_eq!((stalled.start_frame, stalled.resynced), (5001, true));
}

#[test]
fn unlinked_urb_gets_only_ret_unlink() {
    let out = SharedBuf::default();
    let state = ConnState::new(Box::new(out.clone()));
    let mut submit = ParsedSubmit::from_request(SubmitRequest {
        basic: BasicHeader { sequence: 7, endpoint: 1, ..Default::default() },
        number_of_packets: 10,
        ..Default::default()
    });
    submit.tracked = true;
    state.add_pending(7, 1, token());

    let (found, result) = state.unlink(&BasicHeader { sequence: 8, ..Default::default() }, 7, test_bus_time(1000));
    result.unwrap();
    assert!(found);
    let unlink_reply = out.bytes().len();
    state.write_submit(&submit, STATUS_CONN_RESET, 0, &[], &[], 0).unwrap();
    assert_eq!(out.bytes().len(), unlink_reply, "RET_SUBMIT was sent for an unlinked URB");
    let wire = out.bytes();
    assert_eq!(be32(&wire, 0), RET_UNLINK);
    assert_eq!(be32(&wire, 20) as i32, STATUS_CONN_RESET);

    // Unlinking an already completed URB succeeds with status 0.
    let (found, _) = state.unlink(&BasicHeader { sequence: 9, ..Default::default() }, 7, test_bus_time(1000));
    assert!(!found, "completed URB reported as pending");
}

#[test]
fn unlink_resets_idle_iso_timeline() {
    let state = discard_state();
    let now = test_bus_time(1000);
    for seq in 1..=4 {
        state.reserve_iso_completion(1, 10, now);
        state.add_pending(seq, 1, token());
    }
    for seq in 1..=4 {
        state.unlink(&BasicHeader { sequence: 100 + seq, ..Default::default() }, seq, now).1.unwrap();
    }
    // The aborted URBs reserved 40 frames; the restarted stream must not wait for it.
    assert_eq!(state.reserve_iso_completion(1, 10, now).start_frame, 1001);
}

#[test]
fn clear_halt_resets_iso_timeline() {
    let state = discard_state();
    let now = test_bus_time(1000);
    state.reserve_iso_completion(1, 10, now);
    state.reserve_iso_completion(1, 10, now);
    reset_timelines_after_control(&state, &CLEAR_HALT, now);
    assert_eq!(state.reserve_iso_completion(1, 10, now).start_frame, 1001);
}

/// After a stall, usbaudio can reset the pipe after every URB with one URB in
/// flight. The schedule must still advance one frame per frame, or playback
/// runs slower than real time and capture underruns permanently.
#[test]
fn clear_halt_after_each_urb_keeps_real_time() {
    let state = discard_state();
    let mut slot = state.reserve_iso_completion(1, 10, test_bus_time(1000)); // frames 1001-1010
    for i in 0..100 {
        // The URB completes, the host clears the halt and resubmits 2 frames later.
        reset_timelines_after_control(&state, &CLEAR_HALT, test_bus_time(slot.end_frame + 1));
        let next = state.reserve_iso_completion(1, 10, test_bus_time(slot.end_frame + 2));
        assert_eq!(next.start_frame as u64, slot.end_frame, "URB {i} not contiguous");
        slot = next;
    }
}

#[test]
fn clear_halt_restarts_stale_iso_timeline() {
    let state = discard_state();
    state.reserve_iso_completion(1, 10, test_bus_time(1000)); // frames 1001-1010
    reset_timelines_after_control(&state, &CLEAR_HALT, test_bus_time(1100));
    assert_eq!(state.reserve_iso_completion(1, 10, test_bus_time(1100)).start_frame, 1101);
}

#[test]
fn set_interface_restarts_idle_iso_timeline() {
    let state = discard_state();
    let now = test_bus_time(1000);
    state.reserve_iso_completion(1, 10, now);
    let set_interface =
        SetupPacket { request_type: 0x01, request: REQUEST_SET_INTERFACE, value: 1, index: 1, length: 0 };
    reset_timelines_after_control(&state, &set_interface, test_bus_time(1003));
    assert_eq!(state.reserve_iso_completion(1, 10, test_bus_time(1003)).start_frame, 1004);
}

#[test]
fn device_list_encoding() {
    let dev = Arc::new(Device::new(1, DEFAULT_LATENCY_MS).unwrap());
    let s = Server::new("127.0.0.1:0", vec![dev], Logger::discard());
    let b = s.device_list();
    // 8-byte OP header, 4-byte count, 312-byte device, 12-byte interfaces.
    assert_eq!(b.len(), 336);
    assert_eq!(be32(&b, 8), 1, "device count");
    assert!(b.windows(3).any(|w| w == b"1-1"), "bus ID missing");
}

#[test]
fn diagnostic_traces_follow_setting() {
    let dev = Device::new(1, DEFAULT_LATENCY_MS).unwrap();
    let submit = ParsedSubmit::from_request(SubmitRequest {
        basic: BasicHeader { endpoint: 1, ..Default::default() },
        number_of_packets: 10,
        ..Default::default()
    });
    for enabled in [false, true] {
        let (logger, out) = Logger::memory();
        let mut s = Server::new("", Vec::new(), logger);
        s.diagnostics = enabled;
        s.trace_submit(&dev, &submit);
        let state = discard_state();
        let done = IsoEvent::Done { seq: 1, bus_us: 0, end: 0, queued: 0, cancelled: false };
        s.trace_iso_event(&dev, &state, 1, done);
        s.trace_endpoint_halt(&dev, &state, &CLEAR_HALT, test_bus_time(1000));
        let logged = !out.lock().unwrap().is_empty();
        assert_eq!(logged, enabled, "diagnostics={enabled}");
    }
}

/// URB events are logged only around a logged pipe reset: the recent ones
/// before it, then the next ones as they happen.
#[test]
fn iso_trace_surrounds_logged_pipe_reset() {
    let dev = Device::new(1, DEFAULT_LATENCY_MS).unwrap();
    let state = discard_state();
    let (logger, out) = Logger::memory();
    let mut s = Server::new("", Vec::new(), logger);
    s.diagnostics = true;
    let lines = || String::from_utf8(out.lock().unwrap().clone()).unwrap();
    let submit = |seq| IsoEvent::Submit {
        seq,
        packets: 10,
        frame: 1000,
        bus_us: 1_000_000,
        start_frame: 1001,
        end: 1011,
        in_flight: 0,
    };

    for seq in 1..=10 {
        s.trace_iso_event(&dev, &state, 1, submit(seq));
    }
    assert_eq!(lines(), "", "traced without a pipe reset");

    s.trace_endpoint_halt(&dev, &state, &CLEAR_HALT, test_bus_time(1012));
    let logged = lines();
    assert!(!logged.contains("seq=2 "), "kept more than the recent history: {logged}");
    assert!(logged.contains("1-1 ISO TRACE ep=1 before submit seq=3 bus_us=1000000 frame=1000"), "{logged}");
    assert!(logged.contains("start_frame=1001 end_frame=1011 lead_frames=1 in_flight=0"), "{logged}");
    assert!(logged.contains("HALT CLEAR #1 ep=1") && logged.contains("bus_us=1012000"), "{logged}");

    let done = IsoEvent::Done { seq: 11, bus_us: 1_011_200, end: 1011, queued: 0, cancelled: false };
    s.trace_iso_event(&dev, &state, 1, done);
    let logged = lines();
    assert!(
        logged.contains("1-1 ISO TRACE ep=1 after done seq=11 bus_us=1011200 end_frame=1011 after_end_us=200 queued=0"),
        "{logged}"
    );
}

/// The audio health report is always on, logs only changes since the last
/// line, and adds at most one line per interval.
#[test]
fn audio_health_reports_changes() {
    let device = active_device(DEFAULT_LATENCY_MS);
    let state = discard_state();
    let (logger, out) = Logger::memory();
    let s = Server::new("", Vec::new(), logger);
    let lines = || String::from_utf8(out.lock().unwrap().clone()).unwrap();
    let allow_check = || lock(&state.health).next_check = None;

    s.report_audio_health(&state, &device);
    allow_check();
    s.report_audio_health(&state, &device);
    assert_eq!(lines(), "", "logged without a change");

    state.reserve_iso_completion(1, 10, test_bus_time(1000));
    state.reserve_iso_completion(1, 10, test_bus_time(5000)); // too late: resynced
    reset_timelines_after_control(&state, &CLEAR_HALT, test_bus_time(5000));
    s.report_audio_health(&state, &device);
    assert_eq!(lines(), "", "logged again within the interval");
    allow_check();
    s.report_audio_health(&state, &device);
    let logged = lines();
    assert_eq!(logged.lines().count(), 1, "{logged}");
    assert!(
        logged.contains("1-1 AUDIO HEALTH missing_ms=0 (+0) realigned=0 (+0) caught_up=0 (+0) latency_ms=40"),
        "{logged}"
    );
    assert!(logged.contains("playback_resyncs=1 capture_resyncs=0 halt_clears=1"), "{logged}");
}

/// Encodes one isochronous CMD_SUBMIT of `packets` 288-byte packets.
fn iso_submit(seq: u32, direction: u32, endpoint: u32, packets: u32) -> Vec<u8> {
    const PACKET_SIZE: u32 = 288;
    let length = packets * PACKET_SIZE;
    let mut b = Vec::new();
    for v in [CMD_SUBMIT, seq, 0x10001, direction, endpoint, 0x2, length, 0, packets, 1] {
        b.extend_from_slice(&v.to_be_bytes());
    }
    b.extend_from_slice(&[0; 8]);
    if direction == DIRECTION_OUT {
        b.resize(b.len() + length as usize, 0);
    }
    for i in 0..packets {
        for v in [i * PACKET_SIZE, PACKET_SIZE, 0, 0] {
            b.extend_from_slice(&v.to_be_bytes());
        }
    }
    b
}

/// Returns the same bytes forever, as a stream of identical URBs.
struct RepeatReader {
    frame: Vec<u8>,
    pos: usize,
}

impl Read for RepeatReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let mut n = 0;
        while n < out.len() {
            let c = (out.len() - n).min(self.frame.len() - self.pos);
            out[n..n + c].copy_from_slice(&self.frame[self.pos..self.pos + c]);
            n += c;
            self.pos = (self.pos + c) % self.frame.len();
        }
        Ok(n)
    }
}

/// A steady isochronous stream must not allocate per URB: decoding reads from
/// the connection buffer, payloads come from the pool and replies are encoded
/// into a reused buffer.
#[test]
fn iso_urb_round_trip_does_not_allocate() {
    let mut r = ConnReader::new(RepeatReader { frame: iso_submit(1, DIRECTION_OUT, 1, 10), pos: 0 }, 64 * 1024);
    let pool = UrbPool::default();
    let mut reply = Vec::new();
    let mut round_trip = || {
        let basic = read_basic_header(&mut r).unwrap();
        let mut submit = read_submit(&mut r, basic, &pool).unwrap();
        let actual = submit.out.len();
        mark_iso_packets(&mut submit.packets, actual);
        reply.clear();
        append_ret_submit(&mut reply, &submit.req, STATUS_OK, actual as u32, &[], &submit.packets, 0);
        submit.release(&pool);
    };
    round_trip(); // warm up the pool and the reply buffer
    let before = crate::alloc_counter::allocations();
    for _ in 0..1000 {
        round_trip();
    }
    let allocations = crate::alloc_counter::allocations() - before;
    assert_eq!(allocations, 0, "{allocations} allocations in 1000 URBs, want none");
}

fn socket_pair() -> (TcpStream, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let host = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (server, _) = listener.accept().unwrap();
    host.set_nodelay(true).unwrap();
    server.set_nodelay(true).unwrap();
    (host, server)
}

fn start_session(device: Arc<Device>, server_side: TcpStream) -> std::thread::JoinHandle<()> {
    let server = Arc::new(Server::new("", vec![Arc::clone(&device)], Logger::discard()));
    std::thread::spawn(move || {
        let reader = ConnReader::new(server_side.try_clone().unwrap(), 64 * 1024);
        server.handle_urbs(reader, Box::new(server_side), device);
    })
}

fn active_device(latency_ms: i64) -> Arc<Device> {
    let device = Device::new(1, latency_ms).unwrap();
    device.handle_control(SetupPacket { request: REQUEST_SET_CONFIGURATION, value: 1, ..Default::default() }, &[]);
    for iface in [1, 2] {
        let setup =
            SetupPacket { request_type: 0x01, request: REQUEST_SET_INTERFACE, index: iface, value: 1, length: 0 };
        device.handle_control(setup, &[]);
    }
    Arc::new(device)
}

#[derive(Debug)]
struct RetSubmit {
    command: u32,
    seq: u32,
    status: i32,
    start_frame: u32,
    packets: u32,
    error_count: u32,
    /// The IN payload.
    data: Vec<u8>,
}

fn read_reply(r: &mut impl Read, in_length: impl Fn(u32) -> usize) -> io::Result<RetSubmit> {
    let mut header = [0u8; 48];
    r.read_exact(&mut header)?;
    let mut reply = RetSubmit {
        command: be32(&header, 0),
        seq: be32(&header, 4),
        status: be32(&header, 20) as i32,
        start_frame: be32(&header, 28),
        packets: be32(&header, 32),
        error_count: be32(&header, 36),
        data: Vec::new(),
    };
    if reply.command == RET_SUBMIT {
        let payload = if reply.status == 0 { in_length(reply.seq) } else { 0 };
        let mut rest = vec![0u8; payload + reply.packets as usize * 16];
        r.read_exact(&mut rest)?;
        rest.truncate(payload);
        reply.data = rest;
    }
    Ok(reply)
}

/// The playback packet numbered `n` in the streaming test: distinct for every
/// packet, and never all zero, so it cannot be mistaken for silence.
fn numbered_packet(n: u32) -> Vec<u8> {
    let mut packet = vec![(n % 251) as u8 + 1; 288];
    packet[..4].copy_from_slice(&n.to_le_bytes());
    packet
}

/// Drives a full-duplex stream as Windows does, keeping several 10 ms URBs
/// queued per endpoint. Each endpoint advances contiguously on the bus frame
/// clock, and the recording side returns the playback audio bit for bit,
/// exactly the cable's latency later, with nothing missing.
#[test]
fn streaming_loops_audio_back_unchanged() {
    let device = active_device(DEFAULT_LATENCY_MS);
    let (mut host, server_side) = socket_pair();
    let session = start_session(Arc::clone(&device), server_side);

    const URBS: usize = 40; // 400 ms per endpoint
    const QUEUED: usize = 3;
    let in_lengths = Arc::new(Mutex::new(HashMap::<u32, usize>::new()));
    let endpoint_of = std::cell::RefCell::new(HashMap::new());
    let mut seq = 1u32;
    let mut played = 0u32;
    let mut submit = |host: &mut TcpStream, direction: u32, endpoint: u32| {
        let mut urb = iso_submit(seq, direction, endpoint, 10);
        if direction == DIRECTION_IN {
            in_lengths.lock().unwrap().insert(seq, 2880);
        } else {
            // The payload follows the 48-byte header.
            for packet in urb[48..48 + 2880].chunks_mut(288) {
                played += 1;
                packet.copy_from_slice(&numbered_packet(played));
            }
        }
        endpoint_of.borrow_mut().insert(seq, endpoint);
        host.write_all(&urb).unwrap();
        seq += 1;
    };

    let (replies_tx, replies) = mpsc::channel();
    let mut reader = host.try_clone().unwrap();
    let lengths = Arc::clone(&in_lengths);
    std::thread::spawn(move || {
        while let Ok(reply) = read_reply(&mut reader, |seq| lengths.lock().unwrap().get(&seq).copied().unwrap_or(0)) {
            if replies_tx.send(reply).is_err() {
                break;
            }
        }
    });

    let mut submitted = HashMap::from([(1u32, 0usize), (2, 0)]);
    for _ in 0..QUEUED {
        submit(&mut host, DIRECTION_OUT, 1);
        submit(&mut host, DIRECTION_IN, 2);
        *submitted.get_mut(&1).unwrap() += 1;
        *submitted.get_mut(&2).unwrap() += 1;
    }
    let mut next_frame = HashMap::new();
    let mut first_played_frame = None;
    // Each captured packet with the bus frame it was captured on.
    let mut captured = Vec::new();
    let mut completed = 0;
    let deadline = Instant::now() + Duration::from_secs(5);
    while completed < 2 * URBS {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let reply =
            replies.recv_timeout(remaining).unwrap_or_else(|_| panic!("stream stalled after {completed} completions"));
        assert_eq!((reply.status, reply.error_count), (0, 0), "seq {}", reply.seq);
        let endpoint = endpoint_of.borrow()[&reply.seq];
        if let Some(&want) = next_frame.get(&endpoint) {
            assert_eq!(reply.start_frame, want, "ep {endpoint} start_frame not contiguous");
        }
        next_frame.insert(endpoint, reply.start_frame + reply.packets);
        if endpoint == 1 {
            first_played_frame.get_or_insert(reply.start_frame);
        } else {
            for (i, packet) in reply.data.chunks(288).enumerate() {
                captured.push((reply.start_frame + i as u32, packet.to_vec()));
            }
        }
        completed += 1;
        if submitted[&endpoint] < URBS {
            let direction = if endpoint == 2 { DIRECTION_IN } else { DIRECTION_OUT };
            submit(&mut host, direction, endpoint);
            *submitted.get_mut(&endpoint).unwrap() += 1;
        }
    }

    // Silence until the latency has passed, then every playback packet in
    // order, starting exactly the latency after the first was played.
    let start = captured.iter().position(|(_, p)| p.iter().any(|&b| b != 0)).expect("no audio came back");
    let first_played_frame = first_played_frame.unwrap();
    assert_eq!(captured[start].0.wrapping_sub(first_played_frame), DEFAULT_LATENCY_MS as u32, "latency in frames");
    let looped = &captured[start..];
    assert!(looped.len() > 300, "only {} packets came back", looped.len());
    for (n, (frame, packet)) in looped.iter().enumerate() {
        assert!(packet == &numbered_packet(n as u32 + 1), "packet {} captured on frame {frame} differs", n + 1);
    }
    assert_eq!(device.audio.status(), LineStatus::default(), "missing audio or realignments");

    host.shutdown(std::net::Shutdown::Both).unwrap();
    session.join().unwrap();
}

/// A playback URB completes after its last frame, as a host controller
/// completes it, even when nothing is queued behind it.
#[test]
fn one_playback_urb_at_a_time_completes_after_its_last_frame() {
    let device = active_device(DEFAULT_LATENCY_MS);
    let (mut host, server_side) = socket_pair();
    let session = start_session(Arc::clone(&device), server_side);
    for seq in 1..=6 {
        host.write_all(&iso_submit(seq, DIRECTION_OUT, 1, 10)).unwrap();
        let reply = read_reply(&mut host, |_| 0).unwrap();
        let completed_on = current_bus_time().frame() as u32;
        assert_eq!((reply.seq, reply.status), (seq, 0));
        let end = reply.start_frame + reply.packets;
        assert!(
            completed_on.wrapping_sub(end) as i32 >= 0,
            "URB {seq} completed on frame {completed_on}, before {end}"
        );
    }
    host.shutdown(std::net::Shutdown::Both).unwrap();
    session.join().unwrap();
}

/// A playback URB that continues its stream on frames that have already
/// begun gets those packets reported as not sent, as a host controller
/// reports them; the rest are sent.
#[test]
fn late_playback_packets_are_reported_as_not_sent() {
    let device = active_device(DEFAULT_LATENCY_MS);
    let (mut host, server_side) = socket_pair();
    let session = start_session(Arc::clone(&device), server_side);

    host.write_all(&iso_submit(1, DIRECTION_OUT, 1, 10)).unwrap();
    let on_time = read_reply(&mut host, |_| 0).unwrap();
    assert_eq!((on_time.status, on_time.error_count), (0, 0), "a URB sent ahead of its frames");

    // The next URB continues the stream, but arrives a few frames late.
    std::thread::sleep(Duration::from_millis(4));
    host.write_all(&iso_submit(2, DIRECTION_OUT, 1, 10)).unwrap();
    let late = read_reply(&mut host, |_| 0).unwrap();
    assert_eq!(late.start_frame, on_time.start_frame + 10, "the stream continues");
    assert_eq!(late.status, 0);
    assert!((1..10).contains(&late.error_count), "{} packets reported late", late.error_count);
    host.shutdown(std::net::Shutdown::Both).unwrap();
    session.join().unwrap();
}

/// The stall test holds every isochronous completion for its length once a
/// minute, after the first minute.
#[test]
fn stall_test_holds_completions() {
    let (logger, out) = Logger::memory();
    let mut server = Server::new("", Vec::new(), logger);
    let event = Event::new();
    server.stall_test = Some(Duration::from_millis(40));
    let held = |server: &Server| {
        let started = Instant::now();
        server.hold_for_stall_test(&event, &|| false).unwrap();
        started.elapsed()
    };
    assert!(held(&server) < Duration::from_millis(10), "held during the first minute");
    server.started = Instant::now() - STALL_TEST_PERIOD;
    assert!(held(&server) >= Duration::from_millis(35), "did not hold at the start of a minute");
    assert!(String::from_utf8(out.lock().unwrap().clone()).unwrap().contains("STALL TEST"));
    assert!(held(&server) < Duration::from_millis(10), "held after the stall ended");
    server.stall_test = None;
    server.started = Instant::now() - STALL_TEST_PERIOD;
    assert!(held(&server) < Duration::from_millis(10), "held while turned off");
}

/// An UNLINK must wake the endpoint worker waiting on the URB's frames: the
/// host gets RET_UNLINK at once and never a RET_SUBMIT for it.
#[test]
fn unlink_interrupts_waiting_urb() {
    let device = active_device(DEFAULT_LATENCY_MS);
    let (mut host, server_side) = socket_pair();
    let session = start_session(device, server_side);

    // A 255-packet capture URB would otherwise complete a quarter second
    // later. (Playback URBs complete at their first frame.)
    host.write_all(&iso_submit(1, DIRECTION_IN, 2, 255)).unwrap();
    std::thread::sleep(Duration::from_millis(20));
    let started = Instant::now();
    let mut unlink = Vec::new();
    for v in [CMD_UNLINK, 2, 0x10001, DIRECTION_IN, 2, 1] {
        unlink.extend_from_slice(&v.to_be_bytes());
    }
    unlink.extend_from_slice(&[0; 24]);
    host.write_all(&unlink).unwrap();

    let reply = read_reply(&mut host, |_| 0).unwrap();
    assert_eq!((reply.command, reply.seq, reply.status), (RET_UNLINK, 2, STATUS_CONN_RESET));
    assert!(started.elapsed() < Duration::from_millis(100), "unlink took {:?}", started.elapsed());

    // No RET_SUBMIT follows for the unlinked URB.
    host.set_read_timeout(Some(Duration::from_millis(400))).unwrap();
    let mut byte = [0u8; 1];
    match host.read(&mut byte) {
        Ok(0) => {}
        Ok(_) => panic!("unexpected reply after RET_UNLINK"),
        Err(err) => assert!(matches!(err.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut), "{err}"),
    }
    host.shutdown(std::net::Shutdown::Both).unwrap();
    session.join().unwrap();
}

/// The server answers a device-list request over TCP, and `shutdown` ends
/// `serve` and open sessions.
#[test]
fn cables_are_added_and_removed_while_serving() {
    let device = |number| Arc::new(Device::new(number, DEFAULT_LATENCY_MS).unwrap());
    let server = Arc::new(Server::new("127.0.0.1:0", vec![device(1), device(2)], Logger::discard()));
    let (ready_tx, ready) = mpsc::channel();
    let running = Arc::clone(&server);
    let serve = std::thread::spawn(move || running.serve(move |r| ready_tx.send(r).unwrap()));
    ready.recv_timeout(Duration::from_secs(3)).unwrap().unwrap();
    let addr = server.local_addr.lock().unwrap().unwrap();

    // Imports a bus ID and returns the session and the reply status.
    let import = |bus_id: &str| {
        let mut conn = TcpStream::connect(addr).unwrap();
        let mut request = Vec::new();
        append_op_header(&mut request, OP_REQ_IMPORT, 0);
        let mut bus = [0u8; 32];
        bus[..bus_id.len()].copy_from_slice(bus_id.as_bytes());
        request.extend_from_slice(&bus);
        conn.write_all(&request).unwrap();
        let mut header = [0u8; 8];
        conn.read_exact(&mut header).unwrap();
        let status = u32::from_be_bytes([header[4], header[5], header[6], header[7]]);
        if status == 0 {
            let mut device = [0u8; 312];
            conn.read_exact(&mut device).unwrap();
        }
        conn.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
        (conn, status)
    };
    let exported = |server: &Server| {
        let list = server.device_list();
        u32::from_be_bytes([list[8], list[9], list[10], list[11]])
    };
    let is_open = |conn: &mut TcpStream| {
        let mut byte = [0u8; 1];
        matches!(conn.read(&mut byte), Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut))
    };

    let (mut first, status) = import("1-1");
    assert_eq!(status, 0);
    let (mut second, status) = import("1-2");
    assert_eq!(status, 0);

    // Removing cable 2 closes only its session.
    assert!(server.remove_device("1-2").is_some());
    assert!(server.remove_device("1-2").is_none());
    assert!(!is_open(&mut second), "the removed cable's session is still open");
    assert!(is_open(&mut first), "the other cable's session was closed");
    assert_eq!(exported(&server), 1);
    assert_eq!(import("1-2").1, 1, "a removed cable can still be imported");

    // A new cable can be imported straight away; a bus ID cannot be added twice.
    server.add_device(device(3)).unwrap();
    assert!(server.add_device(device(3)).is_err());
    assert_eq!(exported(&server), 2);
    let (_third, status) = import("1-3");
    assert_eq!(status, 0);
    assert!(is_open(&mut first), "adding a cable closed another session");

    server.shutdown();
    assert!(serve.join().unwrap().is_ok());
}

#[test]
fn serve_lists_devices_and_shuts_down() {
    let device = active_device(DEFAULT_LATENCY_MS);
    let server = Arc::new(Server::new("127.0.0.1:0", vec![device], Logger::discard()));
    let (ready_tx, ready) = mpsc::channel();
    let running = Arc::clone(&server);
    let serve = std::thread::spawn(move || running.serve(move |r| ready_tx.send(r).unwrap()));
    ready.recv_timeout(Duration::from_secs(3)).unwrap().unwrap();
    let addr = server.local_addr.lock().unwrap().unwrap();

    let mut conn = TcpStream::connect(addr).unwrap();
    let mut request = Vec::new();
    append_op_header(&mut request, OP_REQ_DEVLIST, 0);
    conn.write_all(&request).unwrap();
    let mut reply = vec![0u8; 336];
    conn.read_exact(&mut reply).unwrap();
    assert_eq!(u16::from_be_bytes([reply[2], reply[3]]), OP_REP_DEVLIST);

    // An idle session is closed by shutdown.
    let mut idle = TcpStream::connect(addr).unwrap();
    std::thread::sleep(Duration::from_millis(50));
    server.shutdown();
    assert!(serve.join().unwrap().is_ok());
    idle.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    let mut byte = [0u8; 1];
    assert!(matches!(idle.read(&mut byte), Ok(0) | Err(_)), "session still open after shutdown");
}
