//! The client endpoint: connects, sends inputs, and keeps the newest replicated state of every entity.

use crate::ack::Received;
use crate::wire::{self, Header, Kind, Quantized, Reader};
use crate::{PlayerInput, sockets};
use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

/// Inputs repeated in each input packet, so a lost packet rarely loses a frame of controls.
const INPUT_REDUNDANCY: usize = 4;
const CONNECT_RETRY: Duration = Duration::from_millis(250);

/// The newest state the client has for one entity.
#[derive(Clone, Copy, Debug, Default)]
pub struct RemoteEntity {
    pub state: Quantized,
    /// Server tick of `state`; 0 if nothing has arrived yet.
    pub tick: u32,
}

pub struct NetClient {
    socket: UdpSocket,
    server: SocketAddr,
    name: String,
    connected: bool,
    last_connect: Option<Instant>,
    client_id: u16,
    entity: Option<u32>,
    tick_rate: u32,
    world_size: f32,
    send_seq: u32,
    received: Received,
    input_seq: u32,
    /// Recent inputs, newest first.
    inputs: [PlayerInput; INPUT_REDUNDANCY],
    entities: Vec<RemoteEntity>,
    server_tick: u32,
    recv_buf: Vec<u8>,
    send_buf: Vec<u8>,
    bytes_in: u64,
    packets_in: u64,
    rejected: bool,
}

impl NetClient {
    /// Opens a socket and starts connecting; [`NetClient::is_connected`] turns true once the server
    /// accepts.
    pub fn connect(server: SocketAddr, name: &str) -> io::Result<NetClient> {
        let bind: SocketAddr = if server.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" }.parse().expect("valid address");
        let socket = sockets::open(bind)?;
        let mut client = NetClient {
            socket,
            server,
            name: name.chars().take(32).collect(),
            connected: false,
            last_connect: None,
            client_id: 0,
            entity: None,
            tick_rate: 0,
            world_size: 1.0,
            send_seq: 0,
            received: Received::default(),
            input_seq: 0,
            inputs: [PlayerInput::default(); INPUT_REDUNDANCY],
            entities: Vec::new(),
            server_tick: 0,
            recv_buf: vec![0; 64 * 1024],
            send_buf: Vec::with_capacity(wire::MAX_PACKET),
            bytes_in: 0,
            packets_in: 0,
            rejected: false,
        };
        client.send_connect();
        Ok(client)
    }

    pub fn is_connected(&self) -> bool {
        self.connected
    }

    pub fn was_rejected(&self) -> bool {
        self.rejected
    }

    /// The entity this client controls, once accepted.
    pub fn entity(&self) -> Option<u32> {
        self.entity
    }

    pub fn tick_rate(&self) -> u32 {
        self.tick_rate
    }

    pub fn world_size(&self) -> f32 {
        self.world_size
    }

    /// Newest server tick seen.
    pub fn server_tick(&self) -> u32 {
        self.server_tick
    }

    pub fn entities(&self) -> &[RemoteEntity] {
        &self.entities
    }

    /// Bytes and packets received so far.
    pub fn traffic(&self) -> (u64, u64) {
        (self.bytes_in, self.packets_in)
    }

    fn next_header(&mut self, kind: Kind) -> Header {
        self.send_seq = self.send_seq.wrapping_add(1);
        let (ack, ack_bits) = self.received.ack();
        Header { kind, seq: self.send_seq, ack, ack_bits }
    }

    fn send_connect(&mut self) {
        let header = self.next_header(Kind::Connect);
        wire::write_header(&mut self.send_buf, &header);
        let name = self.name.as_bytes();
        self.send_buf.push(name.len() as u8);
        self.send_buf.extend_from_slice(name);
        let _ = self.socket.send_to(&self.send_buf, self.server);
        self.last_connect = Some(Instant::now());
    }

    /// Sends this frame's controls (with the last few for redundancy). Before the server accepts, this
    /// retries the connection request instead.
    pub fn send_input(&mut self, input: PlayerInput) {
        if !self.connected {
            if self.last_connect.is_none_or(|t| t.elapsed() >= CONNECT_RETRY) && !self.rejected {
                self.send_connect();
            }
            return;
        }
        self.inputs.rotate_right(1);
        self.inputs[0] = input;
        self.input_seq = self.input_seq.wrapping_add(1);
        let header = self.next_header(Kind::Input);
        wire::write_header(&mut self.send_buf, &header);
        wire::put_u32(&mut self.send_buf, self.input_seq);
        self.send_buf.push(INPUT_REDUNDANCY as u8);
        for input in self.inputs {
            wire::write_input(&mut self.send_buf, &input);
        }
        let _ = self.socket.send_to(&self.send_buf, self.server);
    }

    /// Processes everything that has arrived. `on_update(entity, new, previous_tick, tick)` is called
    /// for every entity whose state got newer.
    pub fn poll(&mut self, mut on_update: impl FnMut(u32, &Quantized, u32, u32)) {
        loop {
            let (len, from) = match self.socket.recv_from(&mut self.recv_buf) {
                Ok(got) => got,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
                Err(e) if sockets::is_benign(&e) => continue,
                Err(_) => return,
            };
            if from != self.server {
                continue;
            }
            self.bytes_in += len as u64;
            self.packets_in += 1;
            let mut r = Reader::new(&self.recv_buf[..len]);
            let Some(header) = wire::read_header(&mut r) else { continue };
            if !self.received.record(header.seq) {
                continue;
            }
            match header.kind {
                Kind::Accept => {
                    let (Some(id), Some(entity), Some(rate), Some(world)) = (r.u16(), r.u32(), r.u16(), r.f32())
                    else {
                        continue;
                    };
                    self.connected = true;
                    self.client_id = id;
                    self.entity = (entity != u32::MAX).then_some(entity);
                    self.tick_rate = u32::from(rate);
                    self.world_size = world;
                }
                Kind::Reject => self.rejected = true,
                Kind::Snapshot => {
                    let (Some(tick), Some(_last_input), Some(count)) = (r.u32(), r.u32(), r.u16()) else {
                        continue;
                    };
                    if tick > self.server_tick {
                        self.server_tick = tick;
                    }
                    let mut entity = 0u32;
                    for _ in 0..count {
                        let (Some(delta), Some(state)) = (r.varint(), Quantized::read(&mut r)) else { break };
                        entity = entity.wrapping_add(delta);
                        let e = entity as usize;
                        if e >= self.entities.len() {
                            if e > 50_000_000 {
                                break;
                            }
                            self.entities.resize(e + 1, RemoteEntity::default());
                        }
                        let slot = &mut self.entities[e];
                        if tick > slot.tick {
                            let previous = slot.tick;
                            *slot = RemoteEntity { state, tick };
                            on_update(entity, &state, previous, tick);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    pub fn disconnect(&mut self) {
        if self.connected {
            let header = self.next_header(Kind::Disconnect);
            wire::write_header(&mut self.send_buf, &header);
            let _ = self.socket.send_to(&self.send_buf, self.server);
            self.connected = false;
        }
    }
}

impl Drop for NetClient {
    fn drop(&mut self) {
        self.disconnect();
    }
}
