//! Headless test client: joins a server as one or more players, moves and fires, and measures how often
//! each entity's state arrives, split by distance from the player. That's the number that decides
//! whether soldiers look smooth or stutter.

use horde_net::{NetClient, PlayerInput};
use std::net::{SocketAddr, ToSocketAddrs};
use std::time::{Duration, Instant};

const USAGE: &str = "\
usage: horde-netprobe [options]

  --server ADDR     server address (default 127.0.0.1:7777)
  --clients N       players to connect (default 1)
  --seconds S       how long to measure (default 15)";

/// Distance bands for the report, in metres from the player.
const BANDS: [(f32, &str); 3] = [(60.0, "near (<60 m)"), (200.0, "mid (60-200 m)"), (f32::INFINITY, "far (>200 m)")];
const MAX_INTERVAL: usize = 256;

struct Report {
    connected: bool,
    entity: Option<u32>,
    seconds: f64,
    bytes: u64,
    packets: u64,
    ticks: u32,
    known: usize,
    /// Per band, a histogram of update intervals in server ticks.
    intervals: [Vec<u64>; 3],
}

fn percentile(hist: &[u64], p: f64) -> Option<usize> {
    let total: u64 = hist.iter().sum();
    if total == 0 {
        return None;
    }
    let target = (total as f64 * p).ceil().max(1.0) as u64;
    let mut seen = 0;
    for (interval, &count) in hist.iter().enumerate() {
        seen += count;
        if seen >= target {
            return Some(interval);
        }
    }
    Some(hist.len() - 1)
}

fn run_client(server: SocketAddr, index: usize, seconds: f64) -> Report {
    let mut client = match NetClient::connect(server, &format!("probe-{index}")) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("probe {index}: {e}");
            return Report { connected: false, entity: None, seconds: 0.0, bytes: 0, packets: 0, ticks: 0, known: 0, intervals: Default::default() };
        }
    };
    let mut intervals: [Vec<u64>; 3] = std::array::from_fn(|_| vec![0; MAX_INTERVAL]);
    let frame = Duration::from_secs_f64(1.0 / 30.0);
    let start = Instant::now();
    let mut next_frame = start;
    let mut measure_from: Option<(Instant, u64, u64, u32)> = None;

    while start.elapsed().as_secs_f64() < seconds + 2.0 {
        let me = client.entity().and_then(|e| client.entities().get(e as usize).copied());
        let world = client.world_size();
        let measuring = measure_from.is_some();
        let (mx, mz) = me.map(|m| (m.state.to_state(world).x, m.state.to_state(world).z)).unwrap_or((0.0, 0.0));
        client.poll(|_, state, previous, tick| {
            if !measuring || previous == 0 {
                return;
            }
            let s = state.to_state(world);
            let d = ((s.x - mx).powi(2) + (s.z - mz).powi(2)).sqrt();
            let band = BANDS.iter().position(|&(limit, _)| d < limit).unwrap_or(2);
            let interval = ((tick - previous) as usize).min(MAX_INTERVAL - 1);
            intervals[band][interval] += 1;
        });
        // Give the first two seconds to connecting and the initial full download.
        if measure_from.is_none() && client.is_connected() && start.elapsed() >= Duration::from_secs(2) {
            let (bytes, packets) = client.traffic();
            measure_from = Some((Instant::now(), bytes, packets, client.server_tick()));
        }
        if client.was_rejected() {
            eprintln!("probe {index}: server is full");
            break;
        }
        if Instant::now() >= next_frame {
            // Walk a slow circle, aiming where we walk, firing all the time.
            let t = start.elapsed().as_secs_f32() * 0.4 + index as f32;
            let input = PlayerInput { move_x: t.cos(), move_z: t.sin(), yaw: t.cos().atan2(t.sin()), fire: true };
            client.send_input(input);
            next_frame += frame;
        }
        std::thread::sleep(Duration::from_millis(1));
    }

    let (from, bytes0, packets0, tick0) = measure_from.unwrap_or((Instant::now(), 0, 0, 0));
    let (bytes, packets) = client.traffic();
    Report {
        connected: client.is_connected(),
        entity: client.entity(),
        seconds: from.elapsed().as_secs_f64(),
        bytes: bytes - bytes0,
        packets: packets - packets0,
        ticks: client.server_tick().saturating_sub(tick0),
        known: client.entities().iter().filter(|e| e.tick != 0).count(),
        intervals,
    }
}

fn main() {
    let mut server = "127.0.0.1:7777".to_string();
    let (mut clients, mut seconds) = (1usize, 15.0f64);
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let mut value = || args.next().unwrap_or_else(|| fail(&format!("{flag} needs a value")));
        match flag.as_str() {
            "--server" => server = value(),
            "--clients" => clients = value().parse().unwrap_or_else(|_| fail("--clients needs a number")),
            "--seconds" => seconds = value().parse().unwrap_or_else(|_| fail("--seconds needs a number")),
            "-h" | "--help" => {
                println!("{USAGE}");
                return;
            }
            other => fail(&format!("unknown option '{other}'")),
        }
    }
    let addr = server.to_socket_addrs().ok().and_then(|mut a| a.next()).unwrap_or_else(|| fail(&format!("can't resolve {server}")));

    println!("probing {addr} with {clients} client(s) for {seconds} s (after a 2 s warm-up)...");
    let handles: Vec<_> =
        (0..clients).map(|i| std::thread::spawn(move || run_client(addr, i, seconds))).collect();
    for (i, handle) in handles.into_iter().enumerate() {
        let r = handle.join().expect("probe thread panicked");
        if !r.connected {
            println!("\nclient {i}: never connected");
            continue;
        }
        let secs = r.seconds.max(0.001);
        println!(
            "\nclient {i} (entity {}): {:.0} KB/s in, {:.0} packets/s, {:.1} server ticks/s, {} entities known",
            r.entity.map_or("none".to_string(), |e| e.to_string()),
            r.bytes as f64 / secs / 1024.0,
            r.packets as f64 / secs,
            f64::from(r.ticks) / secs,
            r.known,
        );
        for (band, (_, label)) in BANDS.iter().enumerate() {
            let hist = &r.intervals[band];
            let updates: u64 = hist.iter().sum();
            match (percentile(hist, 0.5), percentile(hist, 0.95), percentile(hist, 0.99)) {
                (Some(p50), Some(p95), Some(p99)) => println!(
                    "  {label:<16} {updates:>9} updates   interval p50 {p50} ticks, p95 {p95}, p99 {p99}  (1 tick = 33 ms)"
                ),
                _ => println!("  {label:<16} no updates"),
            }
        }
    }
}

fn fail(message: &str) -> ! {
    eprintln!("{message}\n\n{USAGE}");
    std::process::exit(2)
}
