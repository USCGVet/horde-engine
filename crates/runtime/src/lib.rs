//! Runs a game's [`Simulation`] as a dedicated server: decides which cores to use, pins the tick thread,
//! starts the job pool, steps the world at its fixed rate, and prints per-second and end-of-run reports.
//!
//! A game's server binary parses its own options, hands the rest to [`Options::take_from`], then calls
//! [`start`] and [`run`]. Work that has to happen around each step, such as networking, plugs in through
//! [`TickHooks`].

use horde_core::{MAX_PHASES, Phases, Simulation};
use horde_cpu::{CpuPlan, Placement, Topology};
use horde_jobs::JobPool;
use std::time::{Duration, Instant};

/// Help text for the options [`Options::take_from`] understands, for a game to append to its own.
pub const USAGE: &str = "\
  --seconds S       how long to run (default 20; 0 runs until stopped)
  --bench           run ticks back to back instead of at the tick rate, to measure headroom
  --cpu MODE        shared (default): leave performance cores to a game client
                    dedicated: one thread on every physical core
  --workers N       use at most N worker threads";

#[derive(Clone, Debug)]
pub struct Options {
    pub placement: Placement,
    pub max_workers: Option<usize>,
    pub seconds: f64,
    pub bench: bool,
}

impl Default for Options {
    fn default() -> Options {
        Options { placement: Placement::SharedWithClient, max_workers: None, seconds: 20.0, bench: false }
    }
}

impl Options {
    /// Removes the runtime's options from `args` and returns them, leaving the game's options in place.
    pub fn take_from(args: &mut Vec<String>) -> Result<Options, String> {
        let mut options = Options::default();
        let mut rest = Vec::with_capacity(args.len());
        let mut it = std::mem::take(args).into_iter();
        while let Some(flag) = it.next() {
            let mut value = || it.next().ok_or_else(|| format!("{flag} needs a value"));
            match flag.as_str() {
                "--seconds" => options.seconds = parse_number(&flag, value()?)?,
                "--bench" => options.bench = true,
                "--cpu" => {
                    options.placement = match value()?.as_str() {
                        "shared" => Placement::SharedWithClient,
                        "dedicated" => Placement::Dedicated,
                        other => return Err(format!("--cpu: unknown mode '{other}'")),
                    }
                }
                "--workers" => options.max_workers = Some(parse_number(&flag, value()?)?),
                _ => rest.push(flag),
            }
        }
        *args = rest;
        Ok(options)
    }
}

/// Parses a numeric option value, naming the option in the error.
pub fn parse_number<T: std::str::FromStr>(flag: &str, value: String) -> Result<T, String> {
    value.parse().map_err(|_| format!("{flag}: '{value}' isn't a valid number"))
}

/// Work the runtime runs around every step of a `S`, such as receiving inputs and sending snapshots.
pub trait TickHooks<S> {
    /// Runs before each step, on the tick thread.
    fn before_step(&mut self, _sim: &mut S) {}

    /// Runs after each step, on the tick thread.
    fn after_step(&mut self, _sim: &S) {}

    /// Extra text for the per-second report, covering the last `seconds`.
    fn report(&mut self, _seconds: f64) -> Option<String> {
        None
    }
}

/// No hooks: a headless simulation.
impl<S> TickHooks<S> for () {}

/// The server's threads: the job pool, plus the calling thread pinned as the tick thread.
pub struct Host {
    pub plan: CpuPlan,
    pub pool: JobPool,
}

/// Detects the CPU, builds the plan, pins the calling thread as the tick thread and starts the workers.
pub fn start(options: &Options) -> Host {
    let topology = Topology::detect();
    let mut plan = CpuPlan::new(&topology, options.placement);
    if let Some(max) = options.max_workers {
        plan.limit_workers(&topology, max);
    }
    println!("CPU: {}", topology.describe());
    println!("plan: {}", plan.describe());
    if !horde_cpu::pin_current_thread(plan.tick_cpu) {
        println!("note: couldn't pin threads on this OS; the scheduler places them");
    }
    horde_cpu::raise_current_thread_priority();
    let pool = JobPool::pinned(&plan.worker_cpus);
    Host { plan, pool }
}

/// Phase timings summed over a reporting window.
#[derive(Default)]
struct Window {
    ticks: u32,
    names: [&'static str; MAX_PHASES],
    sums: [f64; MAX_PHASES],
    phases: usize,
    max_total: f64,
}

impl Window {
    fn add(&mut self, p: &Phases) {
        if self.ticks == 0 {
            self.phases = p.len();
            for i in 0..p.len() {
                self.names[i] = p.name(i);
            }
        }
        self.ticks += 1;
        for i in 0..p.len().min(self.phases) {
            self.sums[i] += p.seconds(i);
        }
        self.max_total = self.max_total.max(p.total());
    }

    fn avg_ms(&self, seconds: f64) -> f64 {
        seconds / f64::from(self.ticks.max(1)) * 1e3
    }

    fn breakdown(&self) -> String {
        let parts: Vec<String> =
            (0..self.phases).map(|i| format!("{} {:.3}", self.names[i], self.avg_ms(self.sums[i]))).collect();
        parts.join(" ")
    }
}

/// Steps `sim` until `options.seconds` have passed (forever if 0), at its tick rate (or back to back with
/// `--bench`), running `hooks` around each step and printing a report every second and a summary at the
/// end.
pub fn run<S: Simulation>(options: &Options, host: &Host, sim: &mut S, hooks: &mut impl TickHooks<S>) {
    let hz = sim.tick_rate().max(1);
    let budget = 1.0 / f64::from(hz);
    let tick_period = Duration::from_secs_f64(budget);
    println!(
        "{} Hz{}, {} threads per pass\n",
        hz,
        if options.bench { ", benchmark: ticks back to back" } else { "" },
        host.pool.threads(),
    );

    let run_start = Instant::now();
    let cpu_start = horde_cpu::process_cpu_seconds();
    let mut window = Window::default();
    let mut window_start = run_start;
    let mut window_cpu = cpu_start;
    let forever = options.seconds <= 0.0;
    // Keep every tick time for the end-of-run percentiles, capped so an endless run stays bounded.
    let keep = if forever { 30 * 60 * hz as usize } else { (options.seconds * f64::from(hz)) as usize + 1 };
    let mut all: Vec<f64> = Vec::with_capacity(keep);
    let mut next_tick = run_start;

    while forever || run_start.elapsed().as_secs_f64() < options.seconds {
        if !options.bench {
            let now = Instant::now();
            if next_tick > now {
                std::thread::sleep(next_tick - now);
            }
            next_tick += tick_period;
        }
        hooks.before_step(sim);
        let phases = sim.step(&host.pool);
        hooks.after_step(sim);
        window.add(&phases);
        if all.len() < keep {
            all.push(phases.total());
        }

        let elapsed = window_start.elapsed().as_secs_f64();
        if elapsed >= 1.0 {
            let cpu_now = horde_cpu::process_cpu_seconds();
            let cores = match (window_cpu, cpu_now) {
                (Some(a), Some(b)) => format!("{:4.2} cores busy", (b - a) / elapsed),
                _ => "cpu n/a".to_string(),
            };
            let avg = window.avg_ms(window.sums.iter().sum());
            let extra = hooks.report(elapsed).map(|r| format!("  {r}")).unwrap_or_default();
            println!(
                "t={:5.1}s  {:4} ticks  avg {:6.3} ms ({:4.1}% of {:.1} ms)  max {:6.3} ms  [{}]  {}  {}{}",
                run_start.elapsed().as_secs_f64(),
                window.ticks,
                avg,
                avg / (budget * 1e3) * 100.0,
                budget * 1e3,
                window.max_total * 1e3,
                window.breakdown(),
                cores,
                sim.status(),
                extra,
            );
            window = Window::default();
            window_start = Instant::now();
            window_cpu = cpu_now;
        }
    }

    let wall = run_start.elapsed().as_secs_f64();
    if all.is_empty() {
        println!("\nno ticks ran");
        return;
    }
    all.sort_by(f64::total_cmp);
    let pct = |p: f64| all[((all.len() - 1) as f64 * p) as usize] * 1e3;
    let mean = all.iter().sum::<f64>() / all.len() as f64;
    println!("\n{} ticks in {:.1} s", all.len(), wall);
    println!(
        "step time: mean {:.3} ms, median {:.3}, p99 {:.3}, max {:.3}  (budget {:.1} ms at {} Hz)",
        mean * 1e3,
        pct(0.5),
        pct(0.99),
        pct(1.0),
        budget * 1e3,
        hz,
    );
    if options.bench {
        let rate = all.len() as f64 / wall;
        println!("throughput: {rate:.0} ticks/s, {:.1}x real time at {hz} Hz", rate / f64::from(hz));
    }
    if let (Some(a), Some(b)) = (cpu_start, horde_cpu::process_cpu_seconds()) {
        println!("CPU used: {:.2} cores on average", (b - a) / wall);
    }
}
