//! Skirmish dedicated server. Runs the battle and reports timings; with `--listen` it also accepts players
//! over UDP and replicates the battle to them.

use horde_net::{NetServer, ServerConfig};
use horde_runtime::{Options, USAGE as RUNTIME_USAGE, parse_number};
use skirmish::{Attack, Config, World};
use std::net::SocketAddr;

const USAGE: &str = "\
usage: skirmish-server [options]

  --listen ADDR     accept players on this UDP address, e.g. 0.0.0.0:7777
  --net-budget KB   snapshot kilobytes per player per tick (default 48)
  --bots N          total bots (default 1000)
  --defenders N     bots on your side, team 0 (default 14); the rest are the horde
  --horde-attack A  how the horde fights: touch (default, charges and strikes up close) or shoot
  --hz N            tick rate (default 30)
  --map METRES      map side length (default 1024)
  --points N        capture points (default 5)
  --seed N          simulation seed";

struct GameArgs {
    config: Config,
    listen: Option<SocketAddr>,
    net_budget_kb: usize,
}

fn parse_game_args(args: Vec<String>) -> Result<GameArgs, String> {
    let mut config = Config::default();
    let (mut bots, mut defenders) = (1000usize, 14usize);
    let (mut listen, mut net_budget_kb) = (None, 48usize);
    let mut it = args.into_iter();
    while let Some(flag) = it.next() {
        let mut value = || it.next().ok_or_else(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--listen" => {
                let v = value()?;
                listen = Some(v.parse().map_err(|_| format!("--listen: '{v}' isn't an address like 0.0.0.0:7777"))?);
            }
            "--net-budget" => net_budget_kb = parse_number(&flag, value()?)?,
            "--bots" => bots = parse_number(&flag, value()?)?,
            "--defenders" => defenders = parse_number(&flag, value()?)?,
            "--horde-attack" => {
                config.attacks[1] = match value()?.as_str() {
                    "touch" => Attack::Touch,
                    "shoot" => Attack::Shoot,
                    other => return Err(format!("--horde-attack: '{other}' isn't touch or shoot")),
                }
            }
            "--hz" => config.tick_rate = parse_number(&flag, value()?)?,
            "--map" => config.map_size = parse_number(&flag, value()?)?,
            "--points" => config.capture_points = parse_number(&flag, value()?)?,
            "--seed" => config.seed = parse_number(&flag, value()?)?,
            "-h" | "--help" => return Err(String::new()),
            other => return Err(format!("unknown option '{other}'")),
        }
    }
    if defenders > bots {
        return Err("--defenders can't exceed --bots".into());
    }
    if config.tick_rate == 0 {
        return Err("--hz must be at least 1".into());
    }
    config.bots = [defenders, bots - defenders];
    Ok(GameArgs { config, listen, net_budget_kb })
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let parsed = Options::take_from(&mut args).and_then(|options| Ok((options, parse_game_args(args)?)));
    let (options, game) = match parsed {
        Ok(parsed) => parsed,
        Err(e) => {
            let help = format!("{USAGE}\n{RUNTIME_USAGE}");
            if e.is_empty() {
                println!("{help}");
                return;
            }
            eprintln!("{e}\n\n{help}");
            std::process::exit(2);
        }
    };

    let config = game.config;
    let host = horde_runtime::start(&options);
    let mut world = World::new(config.clone());
    println!(
        "skirmish: {} bots ({} yours, {} horde attacking by {}) on a {} m map",
        world.soldiers(),
        config.bots[0],
        config.bots[1],
        if config.attacks[1] == Attack::Touch { "touch" } else { "shooting" },
        config.map_size,
    );
    match game.listen {
        Some(addr) => {
            let mut net_config = ServerConfig::new(addr);
            net_config.budget = game.net_budget_kb * 1024;
            let mut net = match NetServer::bind(net_config, config.tick_rate) {
                Ok(net) => net,
                Err(e) => {
                    eprintln!("can't listen on {addr}: {e}");
                    std::process::exit(1);
                }
            };
            println!("listening for players on {}", net.local_addr().map_or(addr, |a| a));
            horde_runtime::run(&options, &host, &mut world, &mut net);
        }
        None => horde_runtime::run(&options, &host, &mut world, &mut ()),
    }
}
