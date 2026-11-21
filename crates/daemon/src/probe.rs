//! System-state probing for auto-pause: battery and CPU load.

use std::time::{Duration, Instant};

use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

/// One sample of the machine's state.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct SystemState {
    /// The machine runs on battery (no AC power).
    pub on_battery: bool,
    /// CPU load caused by *other* processes, as a fraction of all cores (0.0 to 1.0). The
    /// indexer's own load is excluded, otherwise it would pause itself.
    pub cpu_load: f32,
}

/// Source of [`SystemState`] samples, polled by the scheduler's control thread every
/// [`Politeness::probe_interval`](crate::Politeness::probe_interval).
///
/// Tests drive auto-pause with a fake; the app uses [`DefaultProbe`].
pub trait SystemProbe: Send {
    /// Measures the current state. Called from one thread only; may take a few milliseconds.
    fn sample(&mut self) -> SystemState;
}

/// A probe that always reports an idle machine on AC power: auto-pause never triggers.
#[derive(Debug, Clone, Copy, Default)]
pub struct NullProbe;

impl SystemProbe for NullProbe {
    fn sample(&mut self) -> SystemState {
        SystemState::default()
    }
}

/// How often the battery state is re-read: enumerating batteries costs more than a CPU
/// sample, and AC plug changes are rare.
const BATTERY_REFRESH: Duration = Duration::from_secs(15);

/// The real probe: CPU load from `sysinfo`, AC/battery state from `starship-battery`
/// (feature `battery`, on by default).
///
/// CPU load is the global usage since the previous sample minus this process's own usage, so
/// the first sample reads 0. On battery means at least one battery reports *discharging*; a
/// desktop without a battery, or a platform where the state cannot be read, counts as AC.
pub struct DefaultProbe {
    sys: System,
    pid: Option<Pid>,
    cpus: usize,
    on_battery: bool,
    battery_read: Option<Instant>,
}

impl std::fmt::Debug for DefaultProbe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DefaultProbe")
            .field("pid", &self.pid)
            .field("cpus", &self.cpus)
            .field("on_battery", &self.on_battery)
            .finish_non_exhaustive()
    }
}

impl Default for DefaultProbe {
    fn default() -> Self {
        Self::new()
    }
}

impl DefaultProbe {
    /// A probe for this machine. Cheap: no measurement is taken until the first sample.
    pub fn new() -> Self {
        let mut sys = System::new();
        sys.refresh_cpu_usage();
        let cpus = sys.cpus().len().max(1);
        DefaultProbe {
            sys,
            pid: sysinfo::get_current_pid().ok(),
            cpus,
            on_battery: false,
            battery_read: None,
        }
    }

    fn cpu_load(&mut self) -> f32 {
        self.sys.refresh_cpu_usage();
        let global = self.sys.global_cpu_usage() / 100.0;
        let own = match self.pid {
            Some(pid) => {
                self.sys.refresh_processes_specifics(
                    ProcessesToUpdate::Some(&[pid]),
                    true,
                    ProcessRefreshKind::nothing().with_cpu(),
                );
                // Per-process usage is in percent of one core.
                self.sys
                    .process(pid)
                    .map_or(0.0, |p| p.cpu_usage() / 100.0 / self.cpus as f32)
            }
            None => 0.0,
        };
        (global - own).clamp(0.0, 1.0)
    }

    fn on_battery(&mut self) -> bool {
        let stale = self
            .battery_read
            .is_none_or(|t| t.elapsed() >= BATTERY_REFRESH);
        if stale {
            self.on_battery = read_on_battery();
            self.battery_read = Some(Instant::now());
        }
        self.on_battery
    }
}

impl SystemProbe for DefaultProbe {
    fn sample(&mut self) -> SystemState {
        SystemState {
            on_battery: self.on_battery(),
            cpu_load: self.cpu_load(),
        }
    }
}

#[cfg(feature = "battery")]
fn read_on_battery() -> bool {
    use starship_battery::{Manager, State};
    let Ok(manager) = Manager::new() else {
        return false;
    };
    let Ok(batteries) = manager.batteries() else {
        return false;
    };
    batteries.flatten().any(|b| b.state() == State::Discharging)
}

#[cfg(not(feature = "battery"))]
fn read_on_battery() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_probe_samples_sane_values() {
        let mut p = DefaultProbe::new();
        let a = p.sample();
        std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);
        let b = p.sample();
        for s in [a, b] {
            assert!((0.0..=1.0).contains(&s.cpu_load), "{s:?}");
        }
        assert_eq!(a.on_battery, b.on_battery, "battery state is cached");
        eprintln!("default probe on this machine: {b:?}");
    }
}
