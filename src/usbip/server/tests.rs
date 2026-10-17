use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::*;
use crate::uac1::{
    DEFAULT_RESERVE_MS, Device, REQUEST_CLEAR_FEATURE, REQUEST_SET_CONFIGURATION, REQUEST_SET_INTERFACE,
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

/// A URB whose slot has passed starts on the next bus frame, never in the past.
#[test]
fn late_iso_submit_starts_on_next_frame() {
    let state = discard_state();
    state.reserve_iso_completion(1, 10, test_bus_time(1000)); // frames 1001-1010
    state.add_pending(1, 1, token());

    // Its slot is still ahead: the stream stays contiguous.
    let on_time = state.reserve_iso_completion(1, 10, test_bus_time(1010));
    assert_eq!((on_time.start_frame, on_time.resynced), (1011, false));

    // Frame 1021 has begun, so the slot after the queue has expired.
    let now = test_bus_time(1021);
    let late = state.reserve_iso_completion(1, 10, now);
    assert_eq!((late.start_frame, late.resynced), (1022, true));
    assert_eq!(late.deadline.unwrap() - now.wall, Duration::from_millis(11));
}

/// With nothing queued, the previous stream has ended: the next URB starts a
/// new one on the next bus frame.
#[test]
fn iso_submit_after_idle_starts_new_stream() {
    let state = discard_state();
    state.reserve_iso_completion(1, 10, test_bus_time(1000)); // frames 1001-1010
    let next = state.reserve_iso_completion(1, 10, test_bus_time(1012));
    assert_eq!((next.start_frame, next.resynced), (1013, false));
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

    let (found, result) = state.unlink(&BasicHeader { sequence: 8, ..Default::default() }, 7);
    result.unwrap();
    assert!(found);
    let unlink_reply = out.bytes().len();
    state.write_submit(&submit, STATUS_CONN_RESET, 0, &[], &[], 0).unwrap();
    assert_eq!(out.bytes().len(), unlink_reply, "RET_SUBMIT was sent for an unlinked URB");
    let wire = out.bytes();
    assert_eq!(be32(&wire, 0), RET_UNLINK);
    assert_eq!(be32(&wire, 20) as i32, STATUS_CONN_RESET);

    // Unlinking an already completed URB succeeds with status 0.
    let (found, _) = state.unlink(&BasicHeader { sequence: 9, ..Default::default() }, 7);
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
        state.unlink(&BasicHeader { sequence: 100 + seq, ..Default::default() }, seq).1.unwrap();
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
    reset_timelines_after_control(&state, &CLEAR_HALT);
    assert_eq!(state.reserve_iso_completion(1, 10, now).start_frame, 1001);
}

/// The first ASAP transfer after a pipe reset begins on the next frame, as
/// the Windows USB stack documents, even right after the previous one ended:
/// it must never be reported as starting before it was submitted.
#[test]
fn clear_halt_restarts_on_next_frame() {
    let state = discard_state();
    let slot = state.reserve_iso_completion(1, 10, test_bus_time(1000)); // frames 1001-1010
    // The URB completes, the host clears the halt and resubmits 2 frames later.
    reset_timelines_after_control(&state, &CLEAR_HALT);
    let next = state.reserve_iso_completion(1, 10, test_bus_time(slot.end_frame + 2));
    assert_eq!((next.start_frame as u64, next.resynced), (slot.end_frame + 3, false));
}

#[test]
fn set_interface_restarts_idle_iso_timeline() {
    let state = discard_state();
    let now = test_bus_time(1000);
    state.reserve_iso_completion(1, 10, now);
    let set_interface =
        SetupPacket { request_type: 0x01, request: REQUEST_SET_INTERFACE, value: 1, index: 1, length: 0 };
    reset_timelines_after_control(&state, &set_interface);
    assert_eq!(state.reserve_iso_completion(1, 10, test_bus_time(1003)).start_frame, 1004);
}

#[test]
fn device_list_encoding() {
    let dev = Arc::new(Device::new(1, 100, DEFAULT_RESERVE_MS).unwrap());
    let s = Server::new("127.0.0.1:0", vec![dev], Logger::discard());
    let b = s.device_list();
    // 8-byte OP header, 4-byte count, 312-byte device, 12-byte interfaces.
    assert_eq!(b.len(), 336);
    assert_eq!(be32(&b, 8), 1, "device count");
    assert!(b.windows(3).any(|w| w == b"1-1"), "bus ID missing");
}

#[test]
fn diagnostic_traces_follow_setting() {
    let dev = Device::new(1, 100, DEFAULT_RESERVE_MS).unwrap();
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
        s.trace_endpoint_halt(&dev, &discard_state(), &CLEAR_HALT, test_bus_time(1000));
        let logged = !out.lock().unwrap().is_empty();
        assert_eq!(logged, enabled, "diagnostics={enabled}");
    }
}

/// The audio health report is always on, logs only changes since the last
/// line, and adds at most one line per interval.
#[test]
fn audio_health_reports_changes() {
    let device = active_device(500);
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
    state.add_pending(1, 1, token());
    state.reserve_iso_completion(1, 10, test_bus_time(5000)); // too late: resynced
    state.unlink(&BasicHeader { sequence: 2, ..Default::default() }, 1).1.unwrap();
    reset_timelines_after_control(&state, &CLEAR_HALT);
    s.report_audio_health(&state, &device);
    assert_eq!(lines(), "", "logged again within the interval");
    allow_check();
    s.report_audio_health(&state, &device);
    let logged = lines();
    assert_eq!(logged.lines().count(), 1, "{logged}");
    assert!(logged.contains("1-1 AUDIO HEALTH underruns=0 (+0)"), "{logged}");
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

fn active_device(buffer_ms: i64) -> Arc<Device> {
    let device = Device::new(1, buffer_ms, DEFAULT_RESERVE_MS).unwrap();
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
}

fn read_reply(r: &mut impl Read, in_length: impl Fn(u32) -> usize) -> io::Result<RetSubmit> {
    let mut header = [0u8; 48];
    r.read_exact(&mut header)?;
    let reply = RetSubmit {
        command: be32(&header, 0),
        seq: be32(&header, 4),
        status: be32(&header, 20) as i32,
        start_frame: be32(&header, 28),
        packets: be32(&header, 32),
        error_count: be32(&header, 36),
    };
    if reply.command == RET_SUBMIT {
        let payload = if reply.status == 0 { in_length(reply.seq) } else { 0 };
        let mut rest = vec![0u8; payload + reply.packets as usize * 16];
        r.read_exact(&mut rest)?;
    }
    Ok(reply)
}

/// Drives a full-duplex stream as Windows does, keeping several 10 ms URBs
/// queued per endpoint, and checks that each endpoint advances contiguously on
/// the bus frame clock while the ring holds a steady level without underruns.
#[test]
fn streaming_schedule_on_bus_clock() {
    let device = active_device(500);
    let (mut host, server_side) = socket_pair();
    let session = start_session(Arc::clone(&device), server_side);

    const URBS: usize = 40; // 400 ms per endpoint
    const QUEUED: usize = 3;
    let in_lengths = Arc::new(Mutex::new(HashMap::<u32, usize>::new()));
    let endpoint_of = std::cell::RefCell::new(HashMap::new());
    let mut seq = 1u32;
    let mut submit = |host: &mut TcpStream, direction: u32, endpoint: u32| {
        if direction == DIRECTION_IN {
            in_lengths.lock().unwrap().insert(seq, 2880);
        }
        endpoint_of.borrow_mut().insert(seq, endpoint);
        host.write_all(&iso_submit(seq, direction, endpoint, 10)).unwrap();
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
        completed += 1;
        if submitted[&endpoint] < URBS {
            let direction = if endpoint == 2 { DIRECTION_IN } else { DIRECTION_OUT };
            submit(&mut host, direction, endpoint);
            *submitted.get_mut(&endpoint).unwrap() += 1;
        }
    }

    let (available, dropped, underruns) = device.buffer.stats();
    assert_eq!(dropped, 0, "dropped bytes");
    // Capture starts once the reserve is filled; from then on both endpoints
    // run on the same clock, so the level stays near the target.
    assert_eq!(underruns, 0, "underran, ring at {} ms", available / 288);
    let reserve_ms = device.reserve_frames() / 48;
    let ms = available / 288;
    assert!(
        ms + 20 >= reserve_ms && ms <= reserve_ms + 30,
        "ring level {ms} ms, want near the {reserve_ms} ms reserve"
    );

    host.shutdown(std::net::Shutdown::Both).unwrap();
    session.join().unwrap();
}

/// An UNLINK must wake the endpoint worker waiting on the URB's frames: the
/// host gets RET_UNLINK at once and never a RET_SUBMIT for it.
#[test]
fn unlink_interrupts_waiting_urb() {
    let device = active_device(500);
    let (mut host, server_side) = socket_pair();
    let session = start_session(device, server_side);

    // 255 packets: the URB would otherwise complete a quarter second later.
    host.write_all(&iso_submit(1, DIRECTION_OUT, 1, 255)).unwrap();
    std::thread::sleep(Duration::from_millis(20));
    let started = Instant::now();
    let mut unlink = Vec::new();
    for v in [CMD_UNLINK, 2, 0x10001, DIRECTION_OUT, 1, 1] {
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
    let device = |number| Arc::new(Device::new(number, 100, DEFAULT_RESERVE_MS).unwrap());
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
    let device = active_device(100);
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
