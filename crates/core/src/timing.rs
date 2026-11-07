use std::time::Instant;

/// Most phases one step can report.
pub const MAX_PHASES: usize = 8;

/// Wall-clock time spent in each named phase of one step. Fixed-size, so timing never allocates.
#[derive(Clone, Copy, Debug, Default)]
pub struct Phases {
    names: [&'static str; MAX_PHASES],
    seconds: [f64; MAX_PHASES],
    len: usize,
}

impl Phases {
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn name(&self, i: usize) -> &'static str {
        self.names[i]
    }

    pub fn seconds(&self, i: usize) -> f64 {
        self.seconds[i]
    }

    pub fn total(&self) -> f64 {
        self.seconds[..self.len].iter().sum()
    }
}

/// Times consecutive phases of a step: call [`PhaseTimer::mark`] at the end of each one.
pub struct PhaseTimer {
    phases: Phases,
    last: Instant,
}

impl PhaseTimer {
    pub fn start() -> PhaseTimer {
        PhaseTimer { phases: Phases::default(), last: Instant::now() }
    }

    /// Ends the phase that started at the previous mark (or at `start`). Phases past [`MAX_PHASES`] are
    /// folded into the last one.
    pub fn mark(&mut self, name: &'static str) {
        let now = Instant::now();
        let elapsed = (now - self.last).as_secs_f64();
        self.last = now;
        let p = &mut self.phases;
        if p.len < MAX_PHASES {
            p.names[p.len] = name;
            p.seconds[p.len] = elapsed;
            p.len += 1;
        } else {
            p.seconds[MAX_PHASES - 1] += elapsed;
        }
    }

    pub fn finish(self) -> Phases {
        self.phases
    }
}
