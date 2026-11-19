//! Pure arithmetic shared by the read-only telemetry sampler and host checks.
//! Ported from TITAN CORE (telemetry_math.rs, GPL-3.0-or-later); see CREDITS.md.

pub fn cpu_percent(previous: [u64; 5], current: [u64; 5]) -> Option<f64> {
    let mut total = 0u64;
    let mut idle = 0;
    for i in 0..5 {
        let delta = current[i].checked_sub(previous[i])?;
        total = total.checked_add(delta)?;
        if i == 4 {
            idle = delta;
        }
    }
    if total == 0 {
        return None;
    }
    Some(100.0 * (total - idle) as f64 / total as f64)
}

/// Process CPU is deliberately not capped at 100: 100% is one logical CPU.
pub fn process_percent(previous: u64, current: u64, elapsed_seconds: f64) -> Option<f64> {
    if !elapsed_seconds.is_finite() || elapsed_seconds <= 0.0 {
        return None;
    }
    Some(current.checked_sub(previous)? as f64 / 1_000_000.0 / elapsed_seconds * 100.0)
}

/// Non-free physical RAM includes caches; absent or inconsistent counters stay unknown.
pub fn used_ram(total: Option<u64>, free_pages: Option<u64>, page_size: Option<u64>) -> Option<u64> {
    let total = total.filter(|n| *n > 0)?;
    let free = free_pages?.checked_mul(page_size.filter(|n| *n > 0)?)?;
    total.checked_sub(free)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cpu_uses_deltas_and_idle() {
        assert_eq!(cpu_percent([100, 0, 20, 0, 900], [120, 0, 30, 0, 970]), Some(30.0));
        assert_eq!(cpu_percent([0; 5], [0, 0, 0, 0, 100]), Some(0.0));
        assert_eq!(cpu_percent([0; 5], [100, 0, 0, 0, 0]), Some(100.0));
    }
    #[test]
    fn reset_or_empty_cpu_is_unknown() {
        assert_eq!(cpu_percent([1; 5], [0; 5]), None);
        assert_eq!(cpu_percent([1; 5], [1; 5]), None);
        assert_eq!(cpu_percent([0; 5], [u64::MAX; 5]), None);
    }
    #[test]
    fn process_cpu_can_use_multiple_cores() {
        assert_eq!(process_percent(0, 3_000_000, 2.0), Some(150.0));
        assert_eq!(process_percent(10, 0, 1.0), None);
        assert_eq!(process_percent(0, 1, 0.0), None);
    }
    #[test]
    fn ram_never_invents_missing_or_invalid_counters() {
        assert_eq!(used_ram(Some(16_384), Some(2), Some(4096)), Some(8192));
        assert_eq!(used_ram(None, Some(2), Some(4096)), None);
        assert_eq!(used_ram(Some(4096), Some(2), Some(4096)), None);
        assert_eq!(used_ram(Some(4096), Some(u64::MAX), Some(4096)), None);
        assert_eq!(used_ram(Some(4096), Some(2), Some(0)), None);
    }
}
