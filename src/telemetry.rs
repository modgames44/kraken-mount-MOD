//! Bounded, read-only metrics for the web UI (CPU, RAM, uptime). Restricted PS5 sysctls are
//! optional, never guessed: a missing counter stays unknown. Ported from TITAN CORE (telemetry.rs,
//! GPL-3.0-or-later); see CREDITS.md.
use crate::telemetry_math;
use serde_json::{json, Value};
use std::time::Instant;

struct Reading {
    time: Instant,
    cpu: Option<[u64; 5]>,
    process_cpu_us: Option<u64>,
}

pub struct Monitor {
    started: Instant,
    previous: Option<Reading>,
    cached: Option<(Instant, Value)>,
}

impl Default for Monitor {
    fn default() -> Self {
        Self::new()
    }
}

fn sysctl_bytes(name: &[u8], buffer: &mut [u8]) -> Option<usize> {
    let mut length = buffer.len();
    let rc = unsafe {
        libc::sysctlbyname(name.as_ptr().cast(), buffer.as_mut_ptr().cast(), &mut length, std::ptr::null_mut(), 0)
    };
    if rc == 0 && length <= buffer.len() {
        Some(length)
    } else {
        None
    }
}

fn sysctl_uint(name: &[u8]) -> Option<u64> {
    let mut buffer = [0u8; 8];
    match sysctl_bytes(name, &mut buffer)? {
        4 => Some(u32::from_ne_bytes(buffer[..4].try_into().ok()?) as u64),
        8 => Some(u64::from_ne_bytes(buffer)),
        _ => None,
    }
}

fn cpu_ticks() -> Option<[u64; 5]> {
    let mut buffer = [0u8; 40];
    if sysctl_bytes(b"kern.cp_time\0", &mut buffer)? != buffer.len() {
        return None;
    }
    let mut ticks = [0u64; 5];
    for (i, chunk) in buffer.chunks_exact(8).enumerate() {
        ticks[i] = u64::from_ne_bytes(chunk.try_into().ok()?);
    }
    Some(ticks)
}

fn process_usage() -> Option<(u64, u64)> {
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) } != 0 {
        return None;
    }
    let micros = |t: libc::timeval| -> Option<u64> {
        u64::try_from(t.tv_sec).ok()?.checked_mul(1_000_000)?.checked_add(u64::try_from(t.tv_usec).ok()?)
    };
    let cpu = micros(usage.ru_utime)?.checked_add(micros(usage.ru_stime)?)?;
    // FreeBSD ru_maxrss is peak resident size in KiB, not live/system RAM usage.
    let peak = u64::try_from(usage.ru_maxrss).ok()?.checked_mul(1024)?;
    Some((cpu, peak))
}

impl Monitor {
    pub fn new() -> Self {
        Self { started: Instant::now(), previous: None, cached: None }
    }

    pub fn sample(&mut self) -> Value {
        // Multiple browser clients share one baseline; never sleep in an HTTP request.
        if let Some((at, value)) = &self.cached {
            if at.elapsed().as_secs_f64() < 1.0 {
                return value.clone();
            }
        }
        let now = Instant::now();
        let cpu = cpu_ticks();
        let process = process_usage();
        let reading = Reading { time: now, cpu, process_cpu_us: process.map(|x| x.0) };
        let system_percent = self.previous.as_ref().and_then(|previous| {
            telemetry_math::cpu_percent(previous.cpu?, reading.cpu?)
        });
        let payload_percent = self.previous.as_ref().and_then(|previous| {
            telemetry_math::process_percent(
                previous.process_cpu_us?,
                reading.process_cpu_us?,
                now.duration_since(previous.time).as_secs_f64(),
            )
        });
        let total = sysctl_uint(b"hw.physmem\0").filter(|n| *n > 0);
        let used = telemetry_math::used_ram(total, sysctl_uint(b"vm.stats.vm.v_free_count\0"), sysctl_uint(b"hw.pagesize\0"));
        let value = json!({
            "cpu_system_percent": system_percent,
            "cpu_payload_percent": payload_percent,
            "cpu_system_supported": cpu.is_some(),
            "cpu_payload_supported": process.is_some(),
            "logical_cpus": sysctl_uint(b"hw.ncpu\0").filter(|n| *n > 0 && *n <= 1024),
            "ram_total_bytes": total,
            "ram_used_bytes": used,
            "payload_peak_ram_bytes": process.map(|x| x.1),
            "payload_uptime_seconds": self.started.elapsed().as_secs(),
        });
        self.previous = Some(reading);
        self.cached = Some((now, value.clone()));
        value
    }
}
