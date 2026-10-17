//! The USB/IP server: exports each cable as a USB device and services its URBs
//! on the host's bus-frame schedule.

use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::busclock::{BusTime, Cancelled, current_bus_time, wait_bus_frame};
use super::protocol::*;
use crate::event::Event;
use crate::logf;
use crate::logging::Logger;
use crate::uac1::{self, Device, SetupPacket};

const MAX_TRANSFER_LENGTH: u32 = 16 * 1024 * 1024;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Formats I/O errors as the log has always shown end-of-stream errors.
fn describe(err: &io::Error) -> String {
    if is_clean_eof(err) {
        "EOF".into()
    } else if err.kind() == io::ErrorKind::UnexpectedEof {
        "unexpected EOF".into()
    } else {
        err.to_string()
    }
}

pub struct Server {
    pub address: String,
    /// The exported cables. Cables can be added and removed while serving.
    devices: Mutex<Vec<Arc<Device>>>,
    pub logger: Logger,
    /// Enables the per-URB stream traces and the pipe-reset, unlink and resync
    /// events in the log. Set it before `serve`.
    pub diagnostics: bool,

    urb_count: AtomicU64,
    control_count: AtomicU64,
    unlink_count: AtomicU64,
    iso_resync_count: AtomicU64,
    halt_count: AtomicU64,
    halt_burst_start: AtomicU64,
    last_halt_nanos: AtomicI64,

    stopping: AtomicBool,
    local_addr: Mutex<Option<SocketAddr>>,
    /// The open connections, by connection number, with the bus ID of the
    /// device each one has imported.
    connections: Mutex<HashMap<u64, Connection>>,
    next_connection: AtomicU64,
}

/// An open connection: a clone of its stream, to close it, and the bus ID of
/// the device it has imported.
struct Connection {
    stream: TcpStream,
    bus_id: Option<String>,
}

impl Server {
    pub fn new(address: impl Into<String>, devices: Vec<Arc<Device>>, logger: Logger) -> Server {
        Server {
            address: address.into(),
            devices: Mutex::new(devices),
            logger,
            diagnostics: false,
            urb_count: AtomicU64::new(0),
            control_count: AtomicU64::new(0),
            unlink_count: AtomicU64::new(0),
            iso_resync_count: AtomicU64::new(0),
            halt_count: AtomicU64::new(0),
            halt_burst_start: AtomicU64::new(0),
            last_halt_nanos: AtomicI64::new(0),
            stopping: AtomicBool::new(false),
            local_addr: Mutex::new(None),
            connections: Mutex::new(HashMap::new()),
            next_connection: AtomicU64::new(1),
        }
    }

    /// Listens and serves connections until `shutdown`. `ready` is called once:
    /// with `Ok` after the TCP listener has been created, or with the error.
    pub fn serve(self: &Arc<Self>, ready: impl FnOnce(Result<(), String>)) -> Result<(), String> {
        let listener = match TcpListener::bind(&self.address) {
            Ok(listener) => listener,
            Err(err) => {
                let message = format!("listen tcp {}: {err}", self.address);
                ready(Err(message.clone()));
                return Err(message);
            }
        };
        let addr = listener.local_addr().map_err(|e| e.to_string())?;
        *lock(&self.local_addr) = Some(addr);
        ready(Ok(()));
        logf!(self.logger, "USB/IP server listening on {} with {} device(s)", addr, lock(&self.devices).len());

        for conn in listener.incoming() {
            if self.stopping.load(Ordering::SeqCst) {
                return Ok(());
            }
            let conn = match conn {
                Ok(conn) => conn,
                Err(err) => return Err(err.to_string()),
            };
            let _ = conn.set_nodelay(true);
            let keepalive =
                socket2::TcpKeepalive::new().with_time(Duration::from_secs(15)).with_interval(Duration::from_secs(15));
            let _ = socket2::SockRef::from(&conn).set_tcp_keepalive(&keepalive);
            let server = Arc::clone(self);
            std::thread::Builder::new()
                .name("usbip connection".into())
                .spawn(move || server.handle_connection(conn))
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    /// Stops accepting connections and closes the open ones, which ends their
    /// sessions. `serve` returns soon after.
    pub fn shutdown(&self) {
        if self.stopping.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Some(mut addr) = *lock(&self.local_addr) {
            // Wake the blocked accept; the loop sees `stopping` and returns.
            if addr.ip().is_unspecified() {
                addr.set_ip(match addr.ip() {
                    IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::LOCALHOST),
                    IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::LOCALHOST),
                });
            }
            let _ = TcpStream::connect_timeout(&addr, Duration::from_secs(1));
        }
        for conn in lock(&self.connections).values() {
            let _ = conn.stream.shutdown(Shutdown::Both);
        }
    }

    /// Exports another cable. Its bus ID must not be in use.
    pub fn add_device(&self, device: Arc<Device>) -> Result<(), String> {
        let mut devices = lock(&self.devices);
        if devices.iter().any(|d| d.bus_id == device.bus_id) {
            return Err(format!("bus ID {} is already exported", device.bus_id));
        }
        logf!(self.logger, "Exporting {} ({})", device.bus_id, device.product());
        devices.push(device);
        Ok(())
    }

    /// Stops exporting the cable with this bus ID and closes its session, if
    /// it has one. The other cables' sessions are unaffected.
    pub fn remove_device(&self, bus_id: &str) -> Option<Arc<Device>> {
        // The devices lock is held while closing, so no import of this bus ID
        // can slip in between.
        let mut devices = lock(&self.devices);
        let index = devices.iter().position(|d| d.bus_id == bus_id)?;
        let device = devices.remove(index);
        for conn in lock(&self.connections).values().filter(|c| c.bus_id.as_deref() == Some(bus_id)) {
            let _ = conn.stream.shutdown(Shutdown::Both);
        }
        logf!(self.logger, "Stopped exporting {} ({})", device.bus_id, device.product());
        Some(device)
    }

    fn handle_connection(self: Arc<Self>, conn: TcpStream) {
        let id = self.next_connection.fetch_add(1, Ordering::SeqCst);
        if let Ok(stream) = conn.try_clone() {
            lock(&self.connections).insert(id, Connection { stream, bus_id: None });
        }
        // Closing the server must also unblock reads on already accepted
        // connections, or a stopped session could stay alive until the peer
        // closes its socket.
        if self.stopping.load(Ordering::SeqCst) {
            let _ = conn.shutdown(Shutdown::Both);
        }
        self.serve_connection(id, &conn);
        lock(&self.connections).remove(&id);
    }

    fn serve_connection(self: &Arc<Self>, id: u64, conn: &TcpStream) {
        let remote = conn.peer_addr().map(|a| a.to_string()).unwrap_or_else(|_| "unknown".into());
        let (Ok(read_half), Ok(mut write_half)) = (conn.try_clone(), conn.try_clone()) else {
            return;
        };
        // On Windows each clone is a duplicated socket with its own copy of the
        // timeouts, so they are set and cleared on the halves actually used.
        let _ = read_half.set_read_timeout(Some(Duration::from_secs(30)));
        let _ = write_half.set_write_timeout(Some(Duration::from_secs(30)));
        let mut reader = ConnReader::new(read_half, 64 * 1024);
        let header = match read_op_header(&mut reader) {
            Ok(header) => header,
            Err(err) => {
                logf!(self.logger, "management handshake from {remote} failed: {}", describe(&err));
                return;
            }
        };

        match header.code {
            OP_REQ_DEVLIST => {
                logf!(self.logger, "device-list request from {remote}");
                if let Err(err) = write_half.write_all(&self.device_list()) {
                    logf!(self.logger, "device-list reply failed: {err}");
                }
            }
            OP_REQ_IMPORT => {
                let mut bus_raw = [0u8; 32];
                if let Err(err) = reader.read_exact(&mut bus_raw) {
                    logf!(self.logger, "import request failed: {}", describe(&partial_eof(err)));
                    return;
                }
                let bus_id = String::from_utf8_lossy(&bus_raw).trim_end_matches('\0').to_string();
                let Some(device) = self.import_device(id, &bus_id) else {
                    let mut reply = Vec::new();
                    append_op_header(&mut reply, OP_REP_IMPORT, 1);
                    let _ = write_half.write_all(&reply);
                    logf!(self.logger, "client requested unknown bus ID {bus_id:?}");
                    return;
                };
                let mut reply = Vec::new();
                append_op_header(&mut reply, OP_REP_IMPORT, 0);
                append_usb_device(&mut reply, &device);
                if write_half.write_all(&reply).is_err() {
                    return;
                }
                // An idle audio device sends no URBs, so the session must not time out.
                let _ = reader.get_ref().set_read_timeout(None);
                let _ = write_half.set_write_timeout(None);
                logf!(self.logger, "import accepted: {} ({}), client {remote}", device.bus_id, device.product());
                self.handle_urbs(reader, Box::new(write_half), device);
            }
            code => logf!(self.logger, "unsupported management opcode 0x{code:04x} from {remote}"),
        }
    }

    /// The OP_REP_DEVLIST reply.
    pub(crate) fn device_list(&self) -> Vec<u8> {
        let mut b = Vec::new();
        append_op_header(&mut b, OP_REP_DEVLIST, 0);
        let devices = lock(&self.devices);
        b.extend_from_slice(&(devices.len() as u32).to_be_bytes());
        for d in devices.iter() {
            append_usb_device(&mut b, d);
            // The devlist reply contains one class/subclass/protocol tuple for
            // every interface in the active configuration, not one per alt setting.
            b.extend_from_slice(&[0x01, 0x01, 0x00, 0]); // AudioControl
            b.extend_from_slice(&[0x01, 0x02, 0x00, 0]); // AudioStreaming playback
            b.extend_from_slice(&[0x01, 0x02, 0x00, 0]); // AudioStreaming capture
        }
        b
    }

    /// The exported device with this bus ID, recorded as connection `id`'s so
    /// that removing the device closes the connection.
    fn import_device(&self, id: u64, bus_id: &str) -> Option<Arc<Device>> {
        let devices = lock(&self.devices);
        let device = devices.iter().find(|d| d.bus_id == bus_id).cloned()?;
        if let Some(conn) = lock(&self.connections).get_mut(&id) {
            conn.bus_id = Some(bus_id.to_string());
        }
        Some(device)
    }

    pub(crate) fn handle_urbs<R: Read>(
        self: &Arc<Self>,
        mut reader: ConnReader<R>,
        writer: Box<dyn Write + Send>,
        device: Arc<Device>,
    ) {
        let state = Arc::new(ConnState::new(writer));
        let control_event = Event::new();
        let mut workers: HashMap<u32, EndpointWorker> = HashMap::new();

        loop {
            let basic = match read_basic_header(&mut reader) {
                Ok(basic) => basic,
                Err(err) => {
                    if !is_clean_eof(&err) && !self.stopping.load(Ordering::SeqCst) {
                        logf!(self.logger, "{} disconnected: {}", device.bus_id, describe(&err));
                    }
                    break;
                }
            };

            match basic.command {
                CMD_SUBMIT => {
                    let mut submit = match read_submit(&mut reader, basic, &state.pool) {
                        Ok(submit) => submit,
                        Err(err) => {
                            logf!(
                                self.logger,
                                "{} could not parse SUBMIT seq={}: {}",
                                device.bus_id,
                                basic.sequence,
                                describe(&err)
                            );
                            break;
                        }
                    };

                    // Control traffic is deliberately serialized. Descriptor and
                    // state requests on endpoint zero must retain USB ordering
                    // during enumeration.
                    if basic.endpoint == 0 || !submit.req.is_isochronous() {
                        self.trace_submit(&device, &submit);
                        let result = self.process_submit(&state, &device, &mut submit, None, &control_event);
                        submit.release(&state.pool);
                        if let Err(err) = result {
                            logf!(self.logger, "{} SUBMIT seq={} failed: {err}", device.bus_id, basic.sequence);
                            break;
                        }
                        continue;
                    }

                    let worker =
                        workers.entry(basic.endpoint).or_insert_with(|| EndpointWorker::start(self, &state, &device));
                    let now = current_bus_time();
                    let slot = state.reserve_iso_completion(basic.endpoint, submit.req.number_of_packets, now);
                    // usbip-win2 copies start_frame back into the ASAP URB; usbaudio
                    // tracks the stream position from it, so it must be the frame
                    // actually used.
                    submit.host_start_frame = submit.req.start_frame;
                    if self.diagnostics {
                        submit.in_flight = state.iso_state(basic.endpoint, now.frame()).0;
                    }
                    submit.req.start_frame = slot.start_frame;
                    submit.complete_at = slot.deadline;
                    submit.complete_frame = slot.end_frame;
                    if slot.resynced && self.diagnostics {
                        let count = self.iso_resync_count.fetch_add(1, Ordering::SeqCst) + 1;
                        if count <= 20 || count.is_multiple_of(100) {
                            logf!(
                                self.logger,
                                "{} ISO RESYNC #{count} seq={} ep={}: host submitted too late, restarting at frame {}",
                                device.bus_id,
                                basic.sequence,
                                basic.endpoint,
                                slot.start_frame
                            );
                        }
                    }
                    self.trace_submit(&device, &submit);
                    submit.tracked = true;
                    let cancel = Arc::new(CancelToken::new(Some(Arc::clone(&worker.shared.event))));
                    state.add_pending(basic.sequence, basic.endpoint, Arc::clone(&cancel));
                    worker.push(Job { submit, cancel });
                }

                CMD_UNLINK => {
                    let target = match reader.read_fixed(UNLINK_BODY_SIZE) {
                        Ok(body) => u32::from_be_bytes([body[0], body[1], body[2], body[3]]),
                        Err(_) => break,
                    };
                    let (found, result) = state.unlink(&basic, target);
                    // Windows unlinks every queued URB on a pipe abort. Logging each
                    // one synchronously on this reader loop delays following SUBMITs.
                    if self.diagnostics {
                        let count = self.unlink_count.fetch_add(1, Ordering::SeqCst) + 1;
                        if count <= 20 || count.is_multiple_of(100) {
                            logf!(
                                self.logger,
                                "{} UNLINK #{count} seq={} target={target} found={found}",
                                device.bus_id,
                                basic.sequence
                            );
                        }
                    }
                    if result.is_err() {
                        break;
                    }
                }

                command => {
                    logf!(self.logger, "{} unsupported URB command 0x{command:08x}", device.bus_id);
                    break;
                }
            }
        }

        state.cancel_all();
        for (_, worker) in workers {
            worker.stop();
        }
        logf!(self.logger, "USB/IP session closed for {}", device.bus_id);
    }

    /// Handles one URB and writes its reply. Isochronous URBs first wait for
    /// their last frame on the bus clock; `cancel` interrupts the wait.
    fn process_submit(
        &self,
        state: &ConnState,
        device: &Device,
        submit: &mut ParsedSubmit,
        cancel: Option<&CancelToken>,
        event: &Event,
    ) -> io::Result<()> {
        let req = submit.req;

        if req.basic.endpoint == 0 {
            let setup = match uac1::parse_setup(&req.setup) {
                Ok(setup) => setup,
                Err(_) => return state.write_submit(submit, STATUS_INVALID, 0, &[], &[], 0),
            };
            let (mut data, status) = device.handle_control(setup, &submit.out);
            if status == STATUS_OK {
                let now = current_bus_time();
                if is_endpoint_halt_clear(&setup) {
                    self.trace_endpoint_halt(device, state, &setup, now);
                }
                reset_timelines_after_control(state, &setup);
            }
            data.truncate(req.transfer_buffer_length as usize);
            let mut actual = data.len() as u32;
            if req.basic.direction == DIRECTION_OUT && status == STATUS_OK {
                actual = submit.out.len() as u32;
                data.clear();
            }
            return state.write_submit(submit, status, actual, &data, &[], 0);
        }

        let cancelled = || cancel.is_some_and(CancelToken::is_cancelled);
        let fail = |submit: &mut ParsedSubmit, status: i32| {
            mark_iso_error(&mut submit.packets, status);
            let error_count = submit.packets.len() as u32;
            state.write_submit(submit, status, 0, &[], &submit.packets, error_count)
        };
        if req.is_isochronous() && cancelled() {
            return fail(submit, STATUS_CONN_RESET);
        }

        match (req.basic.endpoint, req.basic.direction) {
            (1, DIRECTION_OUT) => {
                // Playback data enters the ring when its frames have been
                // transmitted, mirroring how capture data leaves it. The ring
                // level is then the distance between the two schedules on the
                // same bus clock, rather than also counting however many URBs the
                // host happens to have queued ahead.
                if req.is_isochronous()
                    && wait_iso_completion(event, submit.complete_at, submit.complete_frame, &cancelled).is_err()
                {
                    return fail(submit, STATUS_CONN_RESET);
                }
                // Windows can queue the first isochronous request immediately
                // around SET_INTERFACE. Do not tear down the device for this
                // harmless race: accept and discard the packet until the playback
                // interface is active.
                let mut actual = submit.out.len();
                if device.playback_active() {
                    actual = device.write_playback(&submit.out);
                }
                mark_iso_packets(&mut submit.packets, actual);
                state.write_submit(submit, STATUS_OK, actual as u32, &[], &submit.packets, 0)
            }
            (2, DIRECTION_IN) => {
                if req.is_isochronous()
                    && wait_iso_completion(event, submit.complete_at, submit.complete_frame, &cancelled).is_err()
                {
                    return fail(submit, STATUS_CONN_RESET);
                }
                let length = response_payload_length(&req, &submit.packets);
                submit.input.resize(length, 0);
                if device.capture_active() {
                    device.read_capture(&mut submit.input);
                    self.report_audio_health(state, device);
                } else {
                    submit.input.fill(0); // digital silence; the reused buffer holds old audio
                }
                mark_iso_packets(&mut submit.packets, length);
                state.write_submit(submit, STATUS_OK, length as u32, &submit.input, &submit.packets, 0)
            }
            _ => fail(submit, STATUS_PIPE),
        }
    }

    /// Logs the state of an endpoint when the host clears its halt: how many
    /// URBs are still in flight and where its schedule stands.
    pub(crate) fn trace_endpoint_halt(&self, device: &Device, state: &ConnState, setup: &SetupPacket, now: BusTime) {
        if !self.diagnostics {
            return;
        }
        let endpoint = (setup.index & 0x0f) as u32;
        let count = self.halt_count.fetch_add(1, Ordering::SeqCst) + 1;
        let now_nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos() as i64);
        let previous = self.last_halt_nanos.swap(now_nanos, Ordering::SeqCst);
        if previous == 0 || now_nanos - previous > 1_000_000_000 {
            self.halt_burst_start.store(count, Ordering::SeqCst);
        }
        if count - self.halt_burst_start.load(Ordering::SeqCst) >= HALT_BURST_LINES && !count.is_multiple_of(1000) {
            return;
        }
        let (pending, next_lead, scheduled) = state.iso_state(endpoint, now.frame());
        logf!(
            self.logger,
            "{} HALT CLEAR #{count} ep={endpoint} in_flight={pending} scheduled={scheduled} next_lead_frames={next_lead} since_previous_us={}",
            device.bus_id,
            (now_nanos - previous) / 1000
        );
    }

    /// Logs a cable's audio health when it has changed: underruns and
    /// overruns, the reserve the ring has grown to, and the schedule restarts
    /// and pipe resets that usually cause them. Unlike the traces, it is always
    /// on, so the log of a session that went wrong shows why; it adds at most
    /// one line per `HEALTH_REPORT_INTERVAL`. The first look only records a
    /// baseline.
    pub(crate) fn report_audio_health(&self, state: &ConnState, device: &Device) {
        let now = Instant::now();
        let mut report = lock(&state.health);
        if report.next_check.is_some_and(|at| now < at) {
            return;
        }
        report.next_check = Some(now + HEALTH_REPORT_INTERVAL);
        let ring = device.buffer.status();
        let health = {
            let s = lock(&state.sched);
            Health {
                underruns: ring.underruns,
                overruns: ring.overruns,
                reserve_frames: ring.reserve_frames,
                playback_resyncs: s.iso_resyncs.get(&1).copied().unwrap_or(0),
                capture_resyncs: s.iso_resyncs.get(&2).copied().unwrap_or(0),
                halt_clears: s.halt_clears,
            }
        };
        let Some(previous) = report.reported.replace(health) else {
            return;
        };
        if health == previous {
            return;
        }
        logf!(
            self.logger,
            "{} AUDIO HEALTH underruns={} (+{}) overruns={} (+{}) reserve_ms={} configured_reserve_ms={} queue_ms={} rate_correction_ppm={} playback_resyncs={} capture_resyncs={} halt_clears={}",
            device.bus_id,
            health.underruns,
            health.underruns - previous.underruns,
            health.overruns,
            health.overruns - previous.overruns,
            health.reserve_frames / 48,
            device.reserve_frames() / 48,
            ring.buffered_frames / 48,
            ring.correction_ppm,
            health.playback_resyncs,
            health.capture_resyncs,
            health.halt_clears
        );
    }

    pub(crate) fn trace_submit(&self, device: &Device, submit: &ParsedSubmit) {
        if !self.diagnostics {
            return;
        }
        let req = &submit.req;
        if req.basic.endpoint == 0 {
            let count = self.control_count.fetch_add(1, Ordering::SeqCst) + 1;
            if count <= 20 || count.is_multiple_of(500) {
                let setup = uac1::parse_setup(&req.setup).unwrap_or_default();
                logf!(
                    self.logger,
                    "{} CTRL #{count} seq={} dir={} {}",
                    device.bus_id,
                    req.basic.sequence,
                    direction_name(req.basic.direction),
                    describe_setup(&setup)
                );
            }
            return;
        }
        let count = self.urb_count.fetch_add(1, Ordering::SeqCst) + 1;
        if count <= 20 || count.is_multiple_of(250) {
            let (available, dropped, underruns) = device.buffer.stats();
            let now = current_bus_time();
            let unaligned_packets = submit.packets.iter().filter(|p| p.length % 6 != 0).count();
            let frame = now.frame() as u32;
            logf!(
                self.logger,
                "{} URB #{count} seq={} ep={} dir={} bytes={} iso_packets={} playback_active={} capture_active={} queue_ms={} rate_correction_ppm={} dropped_bytes={dropped} underrun_bytes={underruns} unaligned_iso_packets={unaligned_packets} out_tail_bytes={} start_frame={} lead_frames={} bus_lag_us={} flags=0x{:x} host_start_frame={} host_lead_frames={} in_flight={}",
                device.bus_id,
                req.basic.sequence,
                req.basic.endpoint,
                direction_name(req.basic.direction),
                req.transfer_buffer_length,
                packet_count_name(req.number_of_packets),
                device.playback_active(),
                device.capture_active(),
                available / 288,
                device.buffer.correction_ppm(),
                submit.out.len() % 6,
                req.start_frame,
                req.start_frame.wrapping_sub(frame) as i32,
                (now.precise - now.coarse) / 10,
                req.transfer_flags,
                submit.host_start_frame,
                submit.host_start_frame.wrapping_sub(frame) as i32,
                submit.in_flight
            );
        }
    }
}

/// Appends the 312-byte USB/IP device record.
fn append_usb_device(b: &mut Vec<u8>, d: &Device) {
    let mut path = [0u8; 256];
    let mut bus_id = [0u8; 32];
    fixed_string(&mut path, &format!("/sys/devices/platform/virtual-cables/{}", d.bus_id));
    fixed_string(&mut bus_id, &d.bus_id);
    b.extend_from_slice(&path);
    b.extend_from_slice(&bus_id);
    let dev = &d.descriptors.device;
    let le16 = |at: usize| u16::from_le_bytes([dev[at], dev[at + 1]]);
    b.extend_from_slice(&1u32.to_be_bytes()); // bus number
    b.extend_from_slice(&d.number.to_be_bytes()); // device number
    b.extend_from_slice(&SPEED_FULL.to_be_bytes()); // USB full speed
    b.extend_from_slice(&le16(8).to_be_bytes());
    b.extend_from_slice(&le16(10).to_be_bytes());
    b.extend_from_slice(&le16(12).to_be_bytes());
    b.extend_from_slice(&[dev[4], dev[5], dev[6]]);
    b.push(1); // active configuration
    b.push(dev[17]); // configurations
    b.push(3); // interfaces
}

/// Cancels one URB: an unlink, or the end of its connection.
pub(crate) struct CancelToken {
    cancelled: AtomicBool,
    /// Signalled on cancel, to wake the worker waiting on the URB.
    wake: Option<Arc<Event>>,
}

impl CancelToken {
    pub(crate) fn new(wake: Option<Arc<Event>>) -> CancelToken {
        CancelToken { cancelled: AtomicBool::new(false), wake }
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        if let Some(wake) = &self.wake {
            wake.signal();
        }
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

struct PendingUrb {
    endpoint: u32,
    cancel: Arc<CancelToken>,
}

/// Per-connection URB bookkeeping and the reply writer.
pub(crate) struct ConnState {
    writer: Mutex<WriterState>,
    sched: Mutex<Schedule>,
    pub(crate) pool: UrbPool,
    health: Mutex<HealthReport>,
}

/// How often the audio health report may add a line to the log.
const HEALTH_REPORT_INTERVAL: Duration = Duration::from_secs(10);

/// The audio health report's state: when it may next look, and what it last
/// reported.
#[derive(Default)]
struct HealthReport {
    next_check: Option<Instant>,
    reported: Option<Health>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Health {
    underruns: u64,
    overruns: u64,
    reserve_frames: usize,
    playback_resyncs: u64,
    capture_resyncs: u64,
    halt_clears: u64,
}

struct WriterState {
    out: Box<dyn Write + Send>,
    /// The reply encoding buffer, reused under the writer lock.
    frame: Vec<u8>,
}

#[derive(Default)]
struct Schedule {
    pending: HashMap<u32, PendingUrb>,
    iso_pending: HashMap<u32, i64>,
    iso_next: HashMap<u32, u64>,
    /// Schedule restarts by endpoint, and endpoint halt clears, for the audio
    /// health report.
    iso_resyncs: HashMap<u32, u64>,
    halt_clears: u64,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct IsoSlot {
    pub start_frame: u32,
    /// The frame after the last packet; the URB completes when the bus clock
    /// reaches it.
    pub end_frame: u64,
    pub deadline: Option<Instant>,
    /// The URB arrived after its slot in a running stream had passed, and was
    /// moved to the next bus frame.
    pub resynced: bool,
}

impl Schedule {
    /// Discards the completion schedule of an idle endpoint, so its next URB
    /// starts a new stream on the next bus frame. Used when the host stops the
    /// stream (SET_INTERFACE or SET_CONFIGURATION), aborts it (an UNLINK of
    /// every queued URB) or resets the pipe (CLEAR_FEATURE(ENDPOINT_HALT)).
    ///
    /// For a reset, this is what the Windows USB stack documents for ASAP
    /// transfers: the first transfer after the pipe "was opened or last reset"
    /// begins on the next frame. Carrying on the old schedule instead reports
    /// the first URB after a reset as having started before it was submitted,
    /// which can keep usbaudio resetting the pipe after every URB. For an
    /// abort, it also discards the future slots of the unlinked URBs, which
    /// would otherwise push all later completions further into the future.
    fn reset_iso_timeline(&mut self, endpoint: u32) {
        if self.iso_pending.get(&endpoint).copied().unwrap_or(0) == 0 {
            self.iso_next.remove(&endpoint);
        }
    }

    /// Removes a pending URB so that exactly one reply is sent for it. When
    /// unlinked, the URB is also cancelled, and an endpoint left idle by the
    /// unlink starts a new stream with its next URB.
    fn claim_pending(&mut self, sequence: u32, unlinked: bool) -> bool {
        let Some(entry) = self.pending.remove(&sequence) else {
            return false;
        };
        *self.iso_pending.entry(entry.endpoint).or_default() -= 1;
        if unlinked {
            entry.cancel.cancel();
            self.reset_iso_timeline(entry.endpoint);
        }
        true
    }
}

impl ConnState {
    pub(crate) fn new(out: Box<dyn Write + Send>) -> ConnState {
        ConnState {
            writer: Mutex::new(WriterState { out, frame: Vec::new() }),
            sched: Mutex::new(Schedule::default()),
            pool: UrbPool::default(),
            health: Mutex::new(HealthReport::default()),
        }
    }

    pub(crate) fn reset_idle_iso_timelines(&self) {
        let mut s = lock(&self.sched);
        let endpoints: Vec<u32> = s.iso_next.keys().copied().collect();
        for endpoint in endpoints {
            s.reset_iso_timeline(endpoint);
        }
    }

    /// Handles CLEAR_FEATURE(ENDPOINT_HALT): the host has reset the pipe.
    pub(crate) fn clear_endpoint_halt(&self, endpoint: u32) {
        let mut s = lock(&self.sched);
        s.halt_clears += 1;
        s.reset_iso_timeline(endpoint);
    }

    /// Reports how many URBs are pending on an endpoint and how far its next
    /// free frame is from the current one, for diagnostics.
    pub(crate) fn iso_state(&self, endpoint: u32, current: u64) -> (i64, i64, bool) {
        let s = lock(&self.sched);
        let pending = s.iso_pending.get(&endpoint).copied().unwrap_or(0);
        match s.iso_next.get(&endpoint) {
            Some(&next) => (pending, next as i64 - current as i64, true),
            None => (pending, -(current as i64), false),
        }
    }

    /// Places an isochronous transfer on the endpoint's bus frame schedule, as
    /// a host controller does for USBD_START_ISO_TRANSFER_ASAP: each URB starts
    /// on the frame after the previous URB on the endpoint, and completes when
    /// its last frame ends. Windows queues several URBs at once, so the
    /// schedule (not the arrival time) sets the cadence. Because the schedule is
    /// on the host's bus clock, completions never drift against the frame
    /// numbers Windows uses to track the stream.
    ///
    /// A URB never starts on a frame that has already begun: if its slot has
    /// passed, it starts on the next bus frame, as Linux host controllers do
    /// for URB_ISO_ASAP ("the first unexpired slot, breaking the queue's
    /// synchronization"). Reporting a start frame in the past is something no
    /// real host controller does. If no other URB is queued, the stream had
    /// simply ended, and the URB starts a new one.
    pub(crate) fn reserve_iso_completion(&self, endpoint: u32, packets: u32, now: BusTime) -> IsoSlot {
        if packets == 0 {
            return IsoSlot {
                start_frame: now.frame() as u32,
                end_frame: 0,
                deadline: Some(now.wall),
                resynced: false,
            };
        }
        let packets = packets.min(255);

        let mut s = lock(&self.sched);
        let current = now.frame();
        let mut resynced = false;
        let start = match s.iso_next.get(&endpoint) {
            Some(&next) if next > current => next,
            scheduled => {
                resynced = scheduled.is_some() && s.iso_pending.get(&endpoint).copied().unwrap_or(0) > 0;
                current + 1
            }
        };
        if resynced {
            *s.iso_resyncs.entry(endpoint).or_default() += 1;
        }
        let end = start + packets as u64;
        s.iso_next.insert(endpoint, end);
        IsoSlot { start_frame: start as u32, end_frame: end, deadline: Some(now.frame_start(end)), resynced }
    }

    /// Sends RET_SUBMIT for a URB. Asynchronous URBs are claimed first: if an
    /// UNLINK already claimed the URB, RET_UNLINK is its only reply and the
    /// RET_SUBMIT is suppressed, as the USB/IP protocol requires. Claiming under
    /// the writer lock also guarantees a RET_SUBMIT precedes a RET_UNLINK that
    /// lost the race.
    pub(crate) fn write_submit(
        &self,
        submit: &ParsedSubmit,
        status: i32,
        actual: u32,
        data: &[u8],
        packets: &[IsoPacket],
        error_count: u32,
    ) -> io::Result<()> {
        let mut w = lock(&self.writer);
        if submit.tracked && !lock(&self.sched).claim_pending(submit.req.basic.sequence, false) {
            return Ok(());
        }
        let w = &mut *w;
        w.frame.clear();
        append_ret_submit(&mut w.frame, &submit.req, status, actual, data, packets, error_count);
        w.write_frame()
    }

    /// Claims a pending URB, cancels it and replies with RET_UNLINK. Reports
    /// whether the URB was still pending, and the result of the reply write.
    pub(crate) fn unlink(&self, req: &BasicHeader, target: u32) -> (bool, io::Result<()>) {
        let mut w = lock(&self.writer);
        let found = lock(&self.sched).claim_pending(target, true);
        // The protocol specifies -ECONNRESET for a successful unlink.
        let status = if found { STATUS_CONN_RESET } else { STATUS_OK };
        let w = &mut *w;
        w.frame.clear();
        append_ret_unlink(&mut w.frame, req, status);
        (found, w.write_frame())
    }

    pub(crate) fn add_pending(&self, sequence: u32, endpoint: u32, cancel: Arc<CancelToken>) {
        let mut s = lock(&self.sched);
        if let Some(previous) = s.pending.remove(&sequence) {
            previous.cancel.cancel();
            *s.iso_pending.entry(previous.endpoint).or_default() -= 1;
        }
        s.pending.insert(sequence, PendingUrb { endpoint, cancel });
        *s.iso_pending.entry(endpoint).or_default() += 1;
    }

    pub(crate) fn cancel_all(&self) {
        for entry in lock(&self.sched).pending.values() {
            entry.cancel.cancel();
        }
    }
}

impl WriterState {
    fn write_frame(&mut self) -> io::Result<()> {
        let result = self.out.write_all(&self.frame);
        // Keep the buffer for the next reply unless an unusually large control
        // transfer grew it.
        if self.frame.capacity() > MAX_POOLED_BUFFER {
            self.frame = Vec::new();
        }
        result
    }
}

/// Bounds the buffers kept for reuse. Audio URBs need a few kilobytes; a rare
/// large control transfer is not worth keeping.
const MAX_POOLED_BUFFER: usize = 64 * 1024;

/// The per-URB buffers, reused across URBs so that a steady stream does not
/// allocate per URB.
#[derive(Default)]
struct UrbBuffers {
    out: Vec<u8>,
    input: Vec<u8>,
    packets: Vec<IsoPacket>,
}

/// Reusable per-URB buffers. Each connection has its own pool: its reader
/// takes buffers for incoming URBs and its endpoint workers return them.
#[derive(Default)]
pub(crate) struct UrbPool(Mutex<Vec<UrbBuffers>>);

const MAX_POOLED_URBS: usize = 256;

impl UrbPool {
    fn take(&self) -> UrbBuffers {
        lock(&self.0).pop().unwrap_or_default()
    }

    fn give(&self, mut buffers: UrbBuffers) {
        if buffers.out.capacity() > MAX_POOLED_BUFFER {
            buffers.out = Vec::new();
        }
        if buffers.input.capacity() > MAX_POOLED_BUFFER {
            buffers.input = Vec::new();
        }
        let mut pool = lock(&self.0);
        if pool.len() < MAX_POOLED_URBS {
            pool.push(buffers);
        }
    }
}

pub(crate) struct ParsedSubmit {
    pub req: SubmitRequest,
    pub out: Vec<u8>,
    pub packets: Vec<IsoPacket>,
    /// The IN payload; its contents are stale until written.
    pub input: Vec<u8>,
    pub complete_at: Option<Instant>,
    pub complete_frame: u64,
    /// Tracked URBs are registered as pending and may be unlinked.
    pub tracked: bool,
    /// The start frame the host requested, before scheduling replaces it. It is
    /// only meaningful for URBs without USBD_START_ISO_TRANSFER_ASAP.
    pub host_start_frame: u32,
    /// How many URBs were already pending on the endpoint.
    pub in_flight: i64,
}

impl ParsedSubmit {
    #[cfg(test)]
    pub(crate) fn from_request(req: SubmitRequest) -> ParsedSubmit {
        ParsedSubmit::with_buffers(req, UrbBuffers::default())
    }

    fn with_buffers(req: SubmitRequest, buffers: UrbBuffers) -> ParsedSubmit {
        ParsedSubmit {
            req,
            out: buffers.out,
            packets: buffers.packets,
            input: buffers.input,
            complete_at: None,
            complete_frame: 0,
            tracked: false,
            host_start_frame: 0,
            in_flight: 0,
        }
    }

    /// Returns the URB's buffers to `pool` for reuse.
    pub(crate) fn release(self, pool: &UrbPool) {
        pool.give(UrbBuffers { out: self.out, input: self.input, packets: self.packets });
    }
}

pub(crate) fn read_submit<R: Read>(
    reader: &mut ConnReader<R>,
    basic: BasicHeader,
    pool: &UrbPool,
) -> io::Result<ParsedSubmit> {
    let req = read_submit_body(reader, basic)?;
    if req.transfer_buffer_length > MAX_TRANSFER_LENGTH {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid transfer length {}", req.transfer_buffer_length),
        ));
    }

    let mut submit = ParsedSubmit::with_buffers(req, pool.take());
    submit.out.clear();
    if basic.direction == DIRECTION_OUT && req.transfer_buffer_length > 0 {
        submit.out.resize(req.transfer_buffer_length as usize, 0);
        if let Err(err) = reader.read_exact(&mut submit.out) {
            submit.release(pool);
            return Err(partial_eof(err));
        }
    }
    if let Err(err) = read_iso_packets(reader, req.number_of_packets, &mut submit.packets) {
        submit.release(pool);
        return Err(err);
    }
    Ok(submit)
}

struct Job {
    submit: ParsedSubmit,
    cancel: Arc<CancelToken>,
}

struct WorkerShared {
    queue: Mutex<VecDeque<Job>>,
    /// Signalled on new work, cancellation and stop.
    event: Arc<Event>,
    stopping: AtomicBool,
}

/// Completes one endpoint's isochronous URBs in SUBMIT order. SUBMIT requests
/// arrive in stream order and each URB's deadline follows the previous one's,
/// so completing them in order keeps the audio payloads read from and written
/// to the ring in stream order.
struct EndpointWorker {
    shared: Arc<WorkerShared>,
    thread: Option<JoinHandle<()>>,
}

impl EndpointWorker {
    fn start(server: &Arc<Server>, state: &Arc<ConnState>, device: &Arc<Device>) -> EndpointWorker {
        let shared = Arc::new(WorkerShared {
            queue: Mutex::new(VecDeque::new()),
            event: Arc::new(Event::new()),
            stopping: AtomicBool::new(false),
        });
        let (server, state, device, worker) =
            (Arc::clone(server), Arc::clone(state), Arc::clone(device), Arc::clone(&shared));
        let thread = std::thread::Builder::new()
            .name("usbip endpoint".into())
            .spawn(move || worker.run(&server, &state, &device))
            .expect("start endpoint worker thread");
        EndpointWorker { shared, thread: Some(thread) }
    }

    fn push(&self, job: Job) {
        lock(&self.shared.queue).push_back(job);
        self.shared.event.signal();
    }

    /// Finishes the queued URBs (cancelled by now) and stops the thread.
    fn stop(mut self) {
        self.shared.stopping.store(true, Ordering::SeqCst);
        self.shared.event.signal();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl WorkerShared {
    fn run(&self, server: &Server, state: &ConnState, device: &Device) {
        loop {
            let job = lock(&self.queue).pop_front();
            let Some(mut job) = job else {
                if self.stopping.load(Ordering::SeqCst) {
                    return;
                }
                self.event.wait(None);
                continue;
            };
            let result = server.process_submit(state, device, &mut job.submit, Some(&job.cancel), &self.event);
            if let Err(err) = result
                && !job.cancel.is_cancelled()
            {
                logf!(
                    server.logger,
                    "{} asynchronous SUBMIT seq={} failed: {err}",
                    device.bus_id,
                    job.submit.req.basic.sequence
                );
            }
            job.submit.release(&state.pool);
        }
    }
}

pub(crate) fn is_endpoint_halt_clear(setup: &SetupPacket) -> bool {
    setup.request_type == 0x02 && setup.request == uac1::REQUEST_CLEAR_FEATURE && setup.value == 0
}

/// How many CLEAR_FEATURE(ENDPOINT_HALT) requests are logged in full after a
/// quiet period, to capture how a pipe-reset loop begins.
const HALT_BURST_LINES: u64 = 30;

/// Updates the completion schedule of endpoints the host has reset. Windows
/// aborts a pipe (unlinking its URBs) and then clears ENDPOINT_HALT before
/// streaming again; the new stream starts on the next bus frame, neither
/// waiting for the slots of the discarded URBs nor continuing the old schedule.
/// Selecting an interface or configuration stops the stream, so its next URB
/// starts a new schedule.
pub(crate) fn reset_timelines_after_control(state: &ConnState, setup: &SetupPacket) {
    if is_endpoint_halt_clear(setup) {
        state.clear_endpoint_halt((setup.index & 0x0f) as u32);
    } else if (setup.request_type == 0x01 && setup.request == uac1::REQUEST_SET_INTERFACE)
        || (setup.request_type == 0x00 && setup.request == uac1::REQUEST_SET_CONFIGURATION)
    {
        state.reset_idle_iso_timelines();
    }
}

/// Waits until a transfer's last frame has ended: first by the precise
/// deadline, then until the coarse bus clock usbaudio reads also shows
/// `end_frame`.
fn wait_iso_completion(
    event: &Event,
    complete_at: Option<Instant>,
    end_frame: u64,
    cancelled: &dyn Fn() -> bool,
) -> Result<(), Cancelled> {
    let Some(complete_at) = complete_at else {
        return Ok(());
    };
    while Instant::now() < complete_at {
        if cancelled() {
            return Err(Cancelled);
        }
        event.wait(Some(complete_at));
    }
    wait_bus_frame(event, end_frame, cancelled)
}

fn response_payload_length(req: &SubmitRequest, packets: &[IsoPacket]) -> usize {
    if packets.is_empty() {
        return req.transfer_buffer_length as usize;
    }
    let mut total: u64 = packets.iter().map(|p| p.length as u64).sum();
    if req.transfer_buffer_length > 0 && total > req.transfer_buffer_length as u64 {
        total = req.transfer_buffer_length as u64;
    }
    total.min(MAX_TRANSFER_LENGTH as u64) as usize
}

pub(crate) fn mark_iso_packets(packets: &mut [IsoPacket], actual_total: usize) {
    let mut remaining = actual_total;
    for packet in packets {
        packet.status = STATUS_OK;
        let wanted = (packet.length as usize).min(remaining);
        packet.actual_length = wanted as u32;
        remaining -= wanted;
    }
}

fn mark_iso_error(packets: &mut [IsoPacket], status: i32) {
    for packet in packets {
        packet.actual_length = 0;
        packet.status = status;
    }
}

fn direction_name(direction: u32) -> &'static str {
    if direction == DIRECTION_IN { "IN" } else { "OUT" }
}

fn packet_count_name(count: u32) -> String {
    if count == NO_ISO_PACKETS { "none".into() } else { count.to_string() }
}

fn describe_setup(s: &SetupPacket) -> String {
    format!(
        "bmRequestType=0x{:02X} bRequest=0x{:02X} wValue=0x{:04X} wIndex=0x{:04X} wLength={}",
        s.request_type, s.request, s.value, s.index, s.length
    )
}

#[cfg(test)]
mod tests;
