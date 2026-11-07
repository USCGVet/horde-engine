//! Skirmish dedicated server. Networking arrives in milestone 2; for now it runs the battle headless and
//! reports timings.

use horde_runtime::{Options, USAGE as RUNTIME_USAGE, parse_number};
use skirmish::{Config, World};

const USAGE: &str = "\
usage: skirmish-server [options]

  --bots N          total bots (default 1000)
  --defenders N     bots on your side, team 0 (default 14); the rest are the horde
  --hz N            tick rate (default 30)
  --map METRES      map side length (default 1024)
  --points N        capture points (default 5)
  --seed N          simulation seed";

fn parse_game_args(args: Vec<String>) -> Result<Config, String> {
    let mut config = Config::default();
    let (mut bots, mut defenders) = (1000usize, 14usize);
    let mut it = args.into_iter();
    while let Some(flag) = it.next() {
        let mut value = || it.next().ok_or_else(|| format!("{flag} needs a value"));
        match flag.as_str() {
            "--bots" => bots = parse_number(&flag, value()?)?,
            "--defenders" => defenders = parse_number(&flag, value()?)?,
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
    Ok(config)
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let parsed = Options::take_from(&mut args).and_then(|options| Ok((options, parse_game_args(args)?)));
    let (options, config) = match parsed {
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

    let host = horde_runtime::start(&options);
    let mut world = World::new(config.clone());
    println!(
        "skirmish: {} bots ({} yours, {} horde) on a {} m map",
        world.soldiers(),
        config.bots[0],
        config.bots[1],
        config.map_size,
    );
    horde_runtime::run(&options, &host, &mut world);
}
