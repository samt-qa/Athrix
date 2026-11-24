//! Memory use in the log: the process's private bytes every few seconds, logged when a new
//! peak is reached and every ten minutes otherwise. With the stage messages around them
//! (walk finished, content candidates queued...), the log shows what each stage costs.

use std::thread;
use std::time::{Duration, Instant};

use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};

/// How often memory is sampled.
const SAMPLE_EVERY: Duration = Duration::from_secs(5);
/// A routine report at least this often.
const REPORT_EVERY: Duration = Duration::from_secs(10 * 60);
/// A new peak is logged when it is this much above the last logged peak.
const PEAK_STEP: u64 = 64 << 20;

/// Starts the sampling thread (once, for the app's lifetime).
pub fn start() {
    let spawned = thread::Builder::new()
        .name("athrix-memlog".into())
        .spawn(run);
    if let Err(e) = spawned {
        tracing::warn!(error = %e, "cannot start the memory log");
    }
}

fn mib(b: u64) -> u64 {
    b >> 20
}

fn run() {
    let Ok(pid) = sysinfo::get_current_pid() else {
        return;
    };
    let mut sys = System::new();
    let mut logged_peak = 0u64;
    let mut last_report = Instant::now();
    let started = Instant::now();
    loop {
        sys.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[pid]),
            true,
            ProcessRefreshKind::nothing().with_memory(),
        );
        if let Some(p) = sys.process(pid) {
            // On Windows `virtual_memory` is the commit charge (private bytes) and `memory`
            // the working set, which also counts memory-mapped index files.
            let private = p.virtual_memory();
            let working_set = p.memory();
            let uptime_s = started.elapsed().as_secs();
            if private > logged_peak + PEAK_STEP {
                logged_peak = private;
                last_report = Instant::now();
                tracing::info!(
                    private_mib = mib(private),
                    working_set_mib = mib(working_set),
                    uptime_s,
                    "memory: new peak"
                );
            } else if last_report.elapsed() >= REPORT_EVERY {
                last_report = Instant::now();
                tracing::info!(
                    private_mib = mib(private),
                    working_set_mib = mib(working_set),
                    peak_mib = mib(logged_peak),
                    uptime_s,
                    "memory"
                );
            }
        }
        thread::sleep(SAMPLE_EVERY);
    }
}
