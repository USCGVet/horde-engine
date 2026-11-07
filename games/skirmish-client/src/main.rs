//! Skirmish client: joins a skirmish-server and lets you play one of the defenders.

#![windows_subsystem = "console"]

use horde_client::{ClientConfig, Weapon};
use std::net::ToSocketAddrs;

const USAGE: &str = "\
usage: skirmish-client [options]

  --server ADDR     server to join (default 127.0.0.1:7777)
  --name NAME       player name (default player)
  --all-cores       don't keep off the cores a server on this PC uses

controls: click to capture the mouse, mouse aims, WASD moves, left button fires,
          V switches first/third person, mouse wheel zooms, Esc frees the mouse";

fn main() {
    let (mut server, mut name, mut all_cores) = ("127.0.0.1:7777".to_string(), "player".to_string(), false);
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--server" => server = args.next().unwrap_or_else(|| fail("--server needs an address")),
            "--name" => name = args.next().unwrap_or_else(|| fail("--name needs a value")),
            "--all-cores" => all_cores = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                return;
            }
            other => fail(&format!("unknown option '{other}'")),
        }
    }
    let addr = server
        .to_socket_addrs()
        .ok()
        .and_then(|mut a| a.next())
        .unwrap_or_else(|| fail(&format!("can't resolve {server}")));

    let config = ClientConfig {
        server: addr,
        name,
        title: "Skirmish".to_string(),
        stay_off_server_cores: !all_cores,
        weapon: Weapon { fire_interval: skirmish::PLAYER_FIRE_INTERVAL, range: skirmish::FIRE_RANGE },
    };
    if let Err(e) = horde_client::run(config) {
        eprintln!("skirmish-client: {e}");
        std::process::exit(1);
    }
}

fn fail(message: &str) -> ! {
    eprintln!("{message}\n\n{USAGE}");
    std::process::exit(2)
}
