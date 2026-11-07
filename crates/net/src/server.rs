//! The server endpoint: accepts players, applies their inputs, and replicates the world to each of them
//! by priority within a per-tick byte budget.

use crate::ack::{Received, for_each_acked, newer};
use crate::wire::{self, Header, Kind, MAX_PACKET, Quantized, RECORD_BODY, Reader};
use crate::{PlayerInput, Replicated, sockets};
use horde_runtime::TickHooks;
use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

/// Sent packets remembered per client for acknowledgement; about 1.5 s of snapshots at 30 Hz and 20
/// packets a tick. Acks for older packets are ignored and those entities simply get resent.
const SENT_HISTORY: usize = 1024;
/// Within this distance an entity's relevance is 1: it gets a slot every tick if the budget allows.
const NEAR: f32 = 60.0;
/// Floor on relevance, so even the farthest entity's priority keeps climbing and it isn't starved.
const MIN_RELEVANCE: f32 = 0.01;

#[derive(Clone, Debug)]
pub struct ServerConfig {
    pub bind: SocketAddr,
    /// Snapshot bytes per client per tick.
    pub budget: usize,
    pub max_clients: usize,
    /// A client silent for this long is dropped.
    pub timeout: Duration,
}

impl ServerConfig {
    pub fn new(bind: SocketAddr) -> ServerConfig {
        // 48 KB a tick is 1.4 MB/s per client at 30 Hz: nothing on a LAN, and enough to send 5,000+
        // changed entities every tick.
        ServerConfig { bind, budget: 48 * 1024, max_clients: 16, timeout: Duration::from_secs(5) }
    }
}

#[derive(Default)]
struct SentPacket {
    seq: u32,
    tick: u32,
    in_use: bool,
    entries: Vec<(u32, Quantized)>,
}

struct Client {
    addr: SocketAddr,
    id: u16,
    entity: Option<u32>,
    last_heard: Instant,
    send_seq: u32,
    received: Received,
    input: PlayerInput,
    input_seq: u32,
    any_input: bool,
    /// Per entity: the state this client has acknowledged, and the tick it came from (0 = nothing yet).
    baseline: Vec<Quantized>,
    baseline_tick: Vec<u32>,
    priority: Vec<f32>,
    sent: Vec<SentPacket>,
}

impl Client {
    fn new(addr: SocketAddr, id: u16, entity: Option<u32>) -> Client {
        Client {
            addr,
            id,
            entity,
            last_heard: Instant::now(),
            send_seq: 0,
            received: Received::default(),
            input: PlayerInput::default(),
            input_seq: 0,
            any_input: false,
            baseline: Vec::new(),
            baseline_tick: Vec::new(),
            priority: Vec::new(),
            sent: (0..SENT_HISTORY).map(|_| SentPacket::default()).collect(),
        }
    }

    fn next_header(&mut self, kind: Kind) -> Header {
        self.send_seq = self.send_seq.wrapping_add(1);
        let (ack, ack_bits) = self.received.ack();
        Header { kind, seq: self.send_seq, ack, ack_bits }
    }

    /// The client has every entity state in packet `seq`; make those its baselines.
    fn acknowledge(&mut self, seq: u32) {
        let slot = &mut self.sent[seq as usize % SENT_HISTORY];
        if !slot.in_use || slot.seq != seq {
            return;
        }
        slot.in_use = false;
        for &(entity, state) in &slot.entries {
            let e = entity as usize;
            if e < self.baseline.len() && slot.tick >= self.baseline_tick[e] {
                self.baseline[e] = state;
                self.baseline_tick[e] = slot.tick;
            }
        }
    }
}

/// Counters for the per-second report.
#[derive(Default)]
struct Window {
    bytes: u64,
    packets: u64,
    records: u64,
    ticks: u64,
    busy: Duration,
}

pub struct NetServer {
    socket: UdpSocket,
    cfg: ServerConfig,
    tick_rate: u32,
    clients: Vec<Client>,
    next_id: u16,
    tick: u32,
    /// The world size captured at the last tick; clients quantise against it.
    world_size: f32,
    /// This tick's quantised state and position for every entity, shared by all clients.
    current: Vec<Quantized>,
    position: Vec<(f32, f32)>,
    recv_buf: Vec<u8>,
    send_buf: Vec<u8>,
    candidates: Vec<(f32, u32)>,
    window: Window,
}

impl NetServer {
    pub fn bind(cfg: ServerConfig, tick_rate: u32) -> io::Result<NetServer> {
        let socket = sockets::open(cfg.bind)?;
        Ok(NetServer {
            socket,
            cfg,
            tick_rate,
            clients: Vec::new(),
            next_id: 1,
            tick: 0,
            world_size: 1.0,
            current: Vec::new(),
            position: Vec::new(),
            recv_buf: vec![0; 64 * 1024],
            send_buf: Vec::with_capacity(MAX_PACKET),
            candidates: Vec::new(),
            window: Window::default(),
        })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    fn send(&mut self, addr: SocketAddr) {
        if let Ok(n) = self.socket.send_to(&self.send_buf, addr) {
            self.window.bytes += n as u64;
            self.window.packets += 1;
        }
    }

    fn receive<S: Replicated>(&mut self, sim: &mut S) {
        loop {
            let (len, from) = match self.socket.recv_from(&mut self.recv_buf) {
                Ok(got) => got,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
                Err(e) if sockets::is_benign(&e) => continue,
                Err(_) => return,
            };
            let mut r = Reader::new(&self.recv_buf[..len]);
            let Some(header) = wire::read_header(&mut r) else { continue };
            let known = self.clients.iter().position(|c| c.addr == from);
            match (header.kind, known) {
                (Kind::Connect, None) => self.accept(sim, from),
                (Kind::Connect, Some(i)) => self.send_accept(i),
                (Kind::Input, Some(i)) => {
                    let client = &mut self.clients[i];
                    client.last_heard = Instant::now();
                    if !client.received.record(header.seq) {
                        continue;
                    }
                    for_each_acked(header.ack, header.ack_bits, |seq| client.acknowledge(seq));
                    let (Some(seq), Some(_count)) = (r.u32(), r.u8()) else { continue };
                    if let Some(input) = wire::read_input(&mut r)
                        && (!client.any_input || newer(seq, client.input_seq))
                    {
                        client.input = input;
                        client.input_seq = seq;
                        client.any_input = true;
                    }
                }
                (Kind::Disconnect, Some(i)) => self.drop_client(sim, i),
                _ => {}
            }
        }
    }

    fn accept<S: Replicated>(&mut self, sim: &mut S, from: SocketAddr) {
        if self.clients.len() >= self.cfg.max_clients {
            let mut reject = Client::new(from, 0, None);
            let header = reject.next_header(Kind::Reject);
            wire::write_header(&mut self.send_buf, &header);
            self.send_buf.push(1);
            self.send(from);
            return;
        }
        let entity = sim.spawn_player();
        self.world_size = sim.world_size();
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        self.clients.push(Client::new(from, id, entity));
        println!("net: client {id} joined from {from}, controlling entity {entity:?}");
        self.send_accept(self.clients.len() - 1);
    }

    fn send_accept(&mut self, i: usize) {
        let world = self.world_size;
        let client = &mut self.clients[i];
        let header = client.next_header(Kind::Accept);
        let (addr, id, entity) = (client.addr, client.id, client.entity.unwrap_or(u32::MAX));
        wire::write_header(&mut self.send_buf, &header);
        wire::put_u16(&mut self.send_buf, id);
        wire::put_u32(&mut self.send_buf, entity);
        wire::put_u16(&mut self.send_buf, self.tick_rate.min(u32::from(u16::MAX)) as u16);
        wire::put_f32(&mut self.send_buf, world);
        self.send(addr);
    }

    fn drop_client<S: Replicated>(&mut self, sim: &mut S, i: usize) {
        let client = self.clients.swap_remove(i);
        if let Some(entity) = client.entity {
            sim.despawn_player(entity);
        }
        println!("net: client {} left", client.id);
    }

    /// Captures every entity's state once per tick, for all clients to share.
    fn capture<S: Replicated>(&mut self, sim: &S) {
        let n = sim.entity_count();
        let world = sim.world_size();
        self.world_size = world;
        self.current.clear();
        self.position.clear();
        for e in 0..n {
            let state = sim.entity_state(e);
            self.current.push(Quantized::from_state(&state, world));
            self.position.push((state.x, state.z));
        }
    }

    fn send_snapshot(&mut self, i: usize) {
        let n = self.current.len();
        let tick = self.tick;
        let client = &mut self.clients[i];
        client.baseline.resize(n, Quantized::default());
        client.baseline_tick.resize(n, 0);
        client.priority.resize(n, 0.0);
        let (px, pz) = client.entity.and_then(|e| self.position.get(e as usize).copied()).unwrap_or((0.0, 0.0));
        let own = client.entity.map_or(u32::MAX, |e| e);

        // Every entity that differs from what this client has acknowledged is a candidate. Its priority
        // grows by its relevance each tick until it gets sent.
        self.candidates.clear();
        for e in 0..n {
            if client.baseline_tick[e] != 0 && client.baseline[e] == self.current[e] {
                client.priority[e] = 0.0;
                continue;
            }
            let relevance = if e as u32 == own {
                1.0e6
            } else {
                let (x, z) = self.position[e];
                let d = ((x - px).powi(2) + (z - pz).powi(2)).sqrt();
                if d <= NEAR { 1.0 } else { ((NEAR / d) * (NEAR / d)).max(MIN_RELEVANCE) }
            };
            client.priority[e] += relevance;
            self.candidates.push((client.priority[e], e as u32));
        }

        // The highest priorities that fit the budget, then in id order so id deltas stay one byte.
        let fits = self.cfg.budget / (RECORD_BODY + 2);
        if self.candidates.len() > fits {
            self.candidates.select_nth_unstable_by(fits, |a, b| b.0.total_cmp(&a.0));
            self.candidates.truncate(fits);
        }
        self.candidates.sort_unstable_by_key(|&(_, e)| e);

        // Pack into packets. Always send at least one, so the client keeps getting acks and ticks.
        let (last_input, mut next) = (client.input_seq, 0usize);
        loop {
            let header = client.next_header(Kind::Snapshot);
            wire::write_header(&mut self.send_buf, &header);
            wire::put_u32(&mut self.send_buf, tick);
            wire::put_u32(&mut self.send_buf, last_input);
            let count_at = self.send_buf.len();
            wire::put_u16(&mut self.send_buf, 0);

            let slot = &mut client.sent[header.seq as usize % SENT_HISTORY];
            slot.seq = header.seq;
            slot.tick = tick;
            slot.in_use = true;
            slot.entries.clear();

            let (mut previous, mut count) = (0u32, 0u16);
            while next < self.candidates.len() {
                let e = self.candidates[next].1;
                let delta = e - previous;
                if self.send_buf.len() + wire::varint_len(delta) + RECORD_BODY > MAX_PACKET || count == u16::MAX {
                    break;
                }
                wire::put_varint(&mut self.send_buf, delta);
                self.current[e as usize].write(&mut self.send_buf);
                slot.entries.push((e, self.current[e as usize]));
                client.priority[e as usize] = 0.0;
                previous = e;
                count += 1;
                next += 1;
            }
            self.send_buf[count_at..count_at + 2].copy_from_slice(&count.to_le_bytes());
            self.window.records += u64::from(count);
            let addr = client.addr;
            if let Ok(n) = self.socket.send_to(&self.send_buf, addr) {
                self.window.bytes += n as u64;
                self.window.packets += 1;
            }
            if next >= self.candidates.len() {
                break;
            }
        }
    }
}

impl<S: Replicated> TickHooks<S> for NetServer {
    fn before_step(&mut self, sim: &mut S) {
        let start = Instant::now();
        self.receive(sim);
        let timeout = self.cfg.timeout;
        let mut i = 0;
        while i < self.clients.len() {
            if self.clients[i].last_heard.elapsed() > timeout {
                println!("net: client {} timed out", self.clients[i].id);
                self.drop_client(sim, i);
            } else {
                i += 1;
            }
        }
        for client in &self.clients {
            if let Some(entity) = client.entity {
                sim.apply_input(entity, &client.input);
            }
        }
        self.window.busy += start.elapsed();
    }

    fn after_step(&mut self, sim: &S) {
        let start = Instant::now();
        self.tick = self.tick.wrapping_add(1).max(1);
        self.capture(sim);
        for i in 0..self.clients.len() {
            self.send_snapshot(i);
        }
        self.window.ticks += 1;
        self.window.busy += start.elapsed();
    }

    fn report(&mut self, seconds: f64) -> Option<String> {
        let w = std::mem::take(&mut self.window);
        let per_tick = |v: u64| v as f64 / w.ticks.max(1) as f64;
        Some(format!(
            "net: {} clients, {:.0} KB/s out, {:.0} updates/tick, {:.3} ms/tick",
            self.clients.len(),
            w.bytes as f64 / seconds / 1024.0,
            per_tick(w.records),
            w.busy.as_secs_f64() * 1e3 / w.ticks.max(1) as f64,
        ))
    }
}
