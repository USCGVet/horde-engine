//! CPU topology and thread placement.
//!
//! The server shares the PC with a game client, so it doesn't just take every core: [`CpuPlan`] decides
//! which logical CPU runs the tick thread, which run simulation workers, and which are left alone for
//! the client. On hybrid Intel CPUs the client keeps the performance cores, since games run their main
//! and render threads there, and the server works on the efficient cores.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoreKind {
    Performance,
    Efficient,
}

/// One physical core and the logical CPUs (hyperthreads) on it.
#[derive(Clone, Debug)]
pub struct PhysicalCore {
    pub kind: CoreKind,
    pub logical: Vec<usize>,
}

#[derive(Clone, Debug)]
pub struct Topology {
    pub cores: Vec<PhysicalCore>,
}

impl Topology {
    /// Reads the core layout from the OS. Where that isn't available, every logical CPU is treated as
    /// its own performance core.
    pub fn detect() -> Topology {
        sys::physical_cores().map(classify).unwrap_or_else(|| {
            let n = std::thread::available_parallelism().map_or(1, |n| n.get());
            let cores = (0..n)
                .map(|cpu| PhysicalCore { kind: CoreKind::Performance, logical: vec![cpu] })
                .collect();
            Topology { cores }
        })
    }

    pub fn cores_of(&self, kind: CoreKind) -> impl Iterator<Item = &PhysicalCore> {
        self.cores.iter().filter(move |core| core.kind == kind)
    }

    pub fn logical_count(&self) -> usize {
        self.cores.iter().map(|core| core.logical.len()).sum()
    }

    pub fn describe(&self) -> String {
        let cpus = |kind| {
            let list: Vec<usize> = self.cores_of(kind).flat_map(|core| core.logical.iter().copied()).collect();
            (self.cores_of(kind).count(), cpu_list(&list))
        };
        let (perf, perf_cpus) = cpus(CoreKind::Performance);
        let (eff, eff_cpus) = cpus(CoreKind::Efficient);
        if eff == 0 {
            format!("{perf} cores, {} logical CPUs ({perf_cpus})", self.logical_count())
        } else {
            format!("{perf} performance cores (CPUs {perf_cpus}), {eff} efficient cores (CPUs {eff_cpus})")
        }
    }
}

/// Turns (efficiency class, logical CPUs) pairs into cores. The highest class is the performance tier;
/// on a uniform CPU every core has the same class and all count as performance cores.
fn classify(raw: Vec<(u8, Vec<usize>)>) -> Topology {
    let top = raw.iter().map(|(class, _)| *class).max().unwrap_or(0);
    let cores = raw
        .into_iter()
        .map(|(class, logical)| PhysicalCore {
            kind: if class == top { CoreKind::Performance } else { CoreKind::Efficient },
            logical,
        })
        .collect();
    Topology { cores }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Placement {
    /// Leave the performance cores to a game client running on the same PC.
    SharedWithClient,
    /// One thread on every physical core; nothing else on the PC needs the CPU.
    Dedicated,
}

#[derive(Clone, Debug)]
pub struct CpuPlan {
    pub placement: Placement,
    /// Logical CPU for the thread running the fixed-rate tick loop. It also works on every parallel pass.
    pub tick_cpu: usize,
    /// Logical CPUs for simulation workers, one pinned thread each.
    pub worker_cpus: Vec<usize>,
    /// Logical CPUs the server leaves alone.
    pub client_cpus: Vec<usize>,
}

impl CpuPlan {
    pub fn new(topology: &Topology, placement: Placement) -> CpuPlan {
        let perf: Vec<&PhysicalCore> = topology.cores_of(CoreKind::Performance).collect();
        let eff: Vec<&PhysicalCore> = topology.cores_of(CoreKind::Efficient).collect();
        // Workers go on the first hyperthread of a core only: this work is compute-bound, so a second
        // thread on the same core adds little throughput and makes tick times noisier.
        let first = |core: &&PhysicalCore| core.logical[0];
        let (tick_cpu, worker_cpus) = match placement {
            Placement::Dedicated => {
                let mut all = perf.iter().chain(&eff).map(first);
                (all.next().unwrap_or(0), all.collect())
            }
            Placement::SharedWithClient if !eff.is_empty() => {
                // The tick loop's serial work wants a fast core; the client keeps all the others.
                (perf.last().map_or(0, first), eff.iter().map(first).collect())
            }
            Placement::SharedWithClient => {
                // Uniform CPU: the server takes the upper half of the cores.
                let mut upper = perf[perf.len() / 2..].iter().rev().map(first);
                (upper.next().unwrap_or(0), upper.collect())
            }
        };
        let mut plan = CpuPlan { placement, tick_cpu, worker_cpus, client_cpus: Vec::new() };
        plan.client_cpus = plan.unused(topology);
        plan
    }

    /// Keeps at most `max` workers; the CPUs freed go back to the client.
    pub fn limit_workers(&mut self, topology: &Topology, max: usize) {
        self.worker_cpus.truncate(max);
        self.client_cpus = self.unused(topology);
    }

    fn unused(&self, topology: &Topology) -> Vec<usize> {
        topology
            .cores
            .iter()
            .flat_map(|core| core.logical.iter().copied())
            .filter(|cpu| *cpu != self.tick_cpu && !self.worker_cpus.contains(cpu))
            .collect()
    }

    pub fn describe(&self) -> String {
        let client = if self.client_cpus.is_empty() {
            "none left for a client".to_string()
        } else {
            format!("CPUs {} left for the client", cpu_list(&self.client_cpus))
        };
        format!(
            "tick thread on CPU {}, {} workers on CPUs {}, {client}",
            self.tick_cpu,
            self.worker_cpus.len(),
            cpu_list(&self.worker_cpus),
        )
    }
}

/// Formats CPU numbers compactly, e.g. `0-13, 15`.
pub fn cpu_list(cpus: &[usize]) -> String {
    let mut sorted = cpus.to_vec();
    sorted.sort_unstable();
    let mut parts = Vec::new();
    let mut i = 0;
    while i < sorted.len() {
        let mut j = i;
        while j + 1 < sorted.len() && sorted[j + 1] == sorted[j] + 1 {
            j += 1;
        }
        parts.push(if i == j { sorted[i].to_string() } else { format!("{}-{}", sorted[i], sorted[j]) });
        i = j + 1;
    }
    if parts.is_empty() { "-".to_string() } else { parts.join(", ") }
}

/// Pins the calling thread to one logical CPU. Returns false where that isn't supported.
pub fn pin_current_thread(cpu: usize) -> bool {
    sys::pin_current_thread(cpu)
}

/// Restricts the whole process to `cpus`, e.g. a game client staying off the cores a server on the same PC
/// uses. Returns false where that isn't supported or `cpus` is empty.
pub fn restrict_process(cpus: &[usize]) -> bool {
    let mask = cpus.iter().filter(|&&cpu| cpu < 64).fold(0usize, |m, &cpu| m | 1 << cpu);
    mask != 0 && sys::restrict_process(mask)
}

/// Raises the calling thread's priority one step, for the latency-sensitive tick thread.
pub fn raise_current_thread_priority() -> bool {
    sys::raise_current_thread_priority()
}

/// CPU time (user + kernel) this process has used so far, in seconds.
pub fn process_cpu_seconds() -> Option<f64> {
    sys::process_cpu_seconds()
}

#[cfg(windows)]
mod sys {
    use core::ffi::c_void;

    type Handle = *mut c_void;
    const RELATION_PROCESSOR_CORE: i32 = 0;
    const THREAD_PRIORITY_ABOVE_NORMAL: i32 = 1;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetLogicalProcessorInformationEx(relationship: i32, buffer: *mut u8, length: *mut u32) -> i32;
        fn GetCurrentThread() -> Handle;
        fn GetCurrentProcess() -> Handle;
        fn SetThreadAffinityMask(thread: Handle, mask: usize) -> usize;
        fn SetProcessAffinityMask(process: Handle, mask: usize) -> i32;
        fn SetThreadPriority(thread: Handle, priority: i32) -> i32;
        fn GetProcessTimes(process: Handle, creation: *mut u64, exit: *mut u64, kernel: *mut u64, user: *mut u64) -> i32;
    }

    /// (efficiency class, logical CPUs) for each physical core.
    pub fn physical_cores() -> Option<Vec<(u8, Vec<usize>)>> {
        let mut len = 0u32;
        // SAFETY: a null buffer only asks for the required length, written to `len`.
        unsafe { GetLogicalProcessorInformationEx(RELATION_PROCESSOR_CORE, std::ptr::null_mut(), &mut len) };
        if len == 0 {
            return None;
        }
        let mut buf = vec![0u8; len as usize];
        // SAFETY: `buf` has `len` writable bytes.
        if unsafe { GetLogicalProcessorInformationEx(RELATION_PROCESSOR_CORE, buf.as_mut_ptr(), &mut len) } == 0 {
            return None;
        }
        buf.truncate(len as usize);

        // Each record is SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX { Relationship: u32, Size: u32,
        // PROCESSOR_RELATIONSHIP { Flags: u8, EfficiencyClass: u8, Reserved: [u8; 20], GroupCount: u16,
        // GroupMask: [GROUP_AFFINITY { Mask: u64, Group: u16, Reserved: [u16; 3] }; GroupCount] } }.
        let mut cores = Vec::new();
        let mut rest = buf.as_slice();
        while rest.len() >= 8 {
            let size = u32::from_le_bytes(rest[4..8].try_into().ok()?) as usize;
            if size < 8 + 24 || size > rest.len() {
                return None;
            }
            let body = &rest[8..size];
            let class = body[1];
            let groups = u16::from_le_bytes([body[22], body[23]]) as usize;
            let mut logical = Vec::new();
            for g in 0..groups {
                let at = 24 + g * 16;
                let mask = u64::from_le_bytes(body.get(at..at + 8)?.try_into().ok()?);
                let group = u16::from_le_bytes(body.get(at + 8..at + 10)?.try_into().ok()?) as usize;
                logical.extend((0..64usize).filter(|bit| mask >> bit & 1 == 1).map(|bit| group * 64 + bit));
            }
            cores.push((class, logical));
            rest = &rest[size..];
        }
        (!cores.is_empty()).then_some(cores)
    }

    pub fn pin_current_thread(cpu: usize) -> bool {
        // SetThreadAffinityMask only reaches processor group 0, the first 64 logical CPUs.
        if cpu >= 64 {
            return false;
        }
        // SAFETY: GetCurrentThread returns a pseudo-handle that is always valid for the calling thread.
        unsafe { SetThreadAffinityMask(GetCurrentThread(), 1usize << cpu) != 0 }
    }

    pub fn restrict_process(mask: usize) -> bool {
        // SAFETY: GetCurrentProcess returns a pseudo-handle that is always valid for this process.
        unsafe { SetProcessAffinityMask(GetCurrentProcess(), mask) != 0 }
    }

    pub fn raise_current_thread_priority() -> bool {
        // SAFETY: as above.
        unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL) != 0 }
    }

    pub fn process_cpu_seconds() -> Option<f64> {
        let (mut creation, mut exit, mut kernel, mut user) = (0u64, 0u64, 0u64, 0u64);
        // SAFETY: GetCurrentProcess returns an always-valid pseudo-handle, and each pointer is an
        // 8-byte slot, the size of a FILETIME.
        let ok = unsafe { GetProcessTimes(GetCurrentProcess(), &mut creation, &mut exit, &mut kernel, &mut user) };
        (ok != 0).then(|| (kernel + user) as f64 * 1e-7)
    }
}

#[cfg(not(windows))]
mod sys {
    pub fn physical_cores() -> Option<Vec<(u8, Vec<usize>)>> {
        None
    }
    pub fn pin_current_thread(_cpu: usize) -> bool {
        false
    }
    pub fn restrict_process(_mask: usize) -> bool {
        false
    }
    pub fn raise_current_thread_priority() -> bool {
        false
    }
    pub fn process_cpu_seconds() -> Option<f64> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The i9-14900KF this was built on: 8 hyperthreaded P-cores, then 16 E-cores.
    fn hybrid() -> Topology {
        let mut raw: Vec<(u8, Vec<usize>)> = (0..8).map(|c| (1, vec![2 * c, 2 * c + 1])).collect();
        raw.extend((16..32).map(|cpu| (0, vec![cpu])));
        classify(raw)
    }

    #[test]
    fn shared_plan_leaves_performance_cores_to_the_client() {
        let plan = CpuPlan::new(&hybrid(), Placement::SharedWithClient);
        assert_eq!(plan.tick_cpu, 14);
        assert_eq!(plan.worker_cpus, (16..32).collect::<Vec<_>>());
        assert_eq!(cpu_list(&plan.client_cpus), "0-13, 15");
    }

    #[test]
    fn dedicated_plan_uses_one_thread_per_core() {
        let plan = CpuPlan::new(&hybrid(), Placement::Dedicated);
        assert_eq!(plan.tick_cpu, 0);
        assert_eq!(plan.worker_cpus.len(), 23);
        assert_eq!(cpu_list(&plan.client_cpus), "1, 3, 5, 7, 9, 11, 13, 15");
    }

    #[test]
    fn uniform_cpu_shares_half() {
        let topology = classify((0..8).map(|c| (0, vec![c])).collect());
        let plan = CpuPlan::new(&topology, Placement::SharedWithClient);
        assert_eq!(plan.tick_cpu, 7);
        assert_eq!(plan.worker_cpus, vec![6, 5, 4]);
        assert_eq!(cpu_list(&plan.client_cpus), "0-3");
    }
}
