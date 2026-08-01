//! How much memory gitkay may use, read from the system rather than guessed.
//!
//! Everything here is **advisory**. `None` means "this platform won't say", and every
//! caller falls back to a static default — a missing answer must never stop the app
//! working. Linux only, because `/proc/meminfo` and the cgroup files are readable with
//! plain `std::fs` while every other unix needs a `sysctl` FFI call, and this crate
//! contains no `unsafe`.
//!
//! Parsing is split from reading so the awkward cases (a truncated file, `max`, a
//! cgroup limit above physical RAM) are unit-testable without a `/proc`.

/// Memory as the system reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemInfo {
    /// Physical RAM, or the cgroup's limit when that is lower.
    pub total: u64,
    /// What could be handed out without swapping — `MemAvailable`, which counts
    /// reclaimable page cache, so it means "obtainable by evicting cache" rather than
    /// "sitting idle". That is why callers take a fraction of it and not the lot.
    pub available: u64,
}

/// Share of **total** memory gitkay keeps its hands off, so the machine never ends up
/// short because of us. Subtracted from `available` before anything is derived.
const RESERVE_PERCENT: u64 = 10;

/// Read the system's memory, or `None` where it cannot be known.
///
/// Takes the **minimum** of the host's figures and the cgroup's, because under a
/// container or a systemd slice `/proc/meminfo` reports the host and would happily
/// authorise a budget the cgroup will OOM-kill us for. `available_parallelism` already
/// respects cgroups, so reading the host here would make the thread count and the
/// memory budget disagree about what machine we are on.
#[cfg(target_os = "linux")]
pub fn read() -> Option<MemInfo> {
    let mut info = parse_meminfo(&std::fs::read_to_string("/proc/meminfo").ok()?)?;
    if let Some((limit, used)) = read_cgroup_memory() {
        info.total = info.total.min(limit);
        info.available = info.available.min(limit.saturating_sub(used));
    }
    Some(info)
}

/// Non-Linux unices: no `/proc/meminfo`, and every alternative needs `unsafe` FFI.
/// Callers fall back to their static defaults.
#[cfg(not(target_os = "linux"))]
pub const fn read() -> Option<MemInfo> {
    None
}

/// Bytes gitkay may plan to use: what is available, less `RESERVE_PERCENT` of total.
///
/// Saturating, so a machine already inside the reserve yields 0 and callers clamp to
/// their own floor rather than to nothing — being short of memory is a reason to cache
/// less, never a reason to stop working.
pub fn usable_bytes() -> Option<u64> {
    read().map(|m| m.available.saturating_sub(m.total / 100 * RESERVE_PERCENT))
}

/// `MemTotal` and `MemAvailable` from `/proc/meminfo`'s text, in bytes.
///
/// Both are reported in kB (the kernel's spelling, always kB regardless of size).
/// Returns `None` unless both are present: half an answer would silently authorise a
/// budget computed against a zero reserve.
fn parse_meminfo(text: &str) -> Option<MemInfo> {
    let field = |name: &str| -> Option<u64> {
        text.lines()
            .find_map(|l| l.strip_prefix(name)?.trim().strip_suffix("kB"))
            .and_then(|kb| kb.trim().parse::<u64>().ok())
            .map(|kb| kb * 1024)
    };
    Some(MemInfo {
        total: field("MemTotal:")?,
        available: field("MemAvailable:")?,
    })
}

/// The cgroup's `(limit, current usage)` in bytes, v2 first then v1, or `None` when
/// unlimited or unreadable.
#[cfg(target_os = "linux")]
fn read_cgroup_memory() -> Option<(u64, u64)> {
    // THIS process's cgroup, not the root's. Reading `/sys/fs/cgroup/memory.max`
    // directly — which an earlier version did — reads the root cgroup, which is `max`
    // on any ordinary system, so the clamp silently never applied and the container
    // awareness was decorative.
    let own = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let rel = cgroup_relative_path(&own)?;
    let dir = format!("/sys/fs/cgroup{rel}");
    let pair = |limit: String, usage: String| -> Option<(u64, u64)> {
        let limit = parse_cgroup_limit(&std::fs::read_to_string(limit).ok()?)?;
        let usage = std::fs::read_to_string(usage)
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(0);
        Some((limit, usage))
    };
    pair(format!("{dir}/memory.max"), format!("{dir}/memory.current")).or_else(|| {
        pair(
            format!("/sys/fs/cgroup/memory{rel}/memory.limit_in_bytes"),
            format!("/sys/fs/cgroup/memory{rel}/memory.usage_in_bytes"),
        )
    })
}

/// The process's cgroup path from `/proc/self/cgroup`.
///
/// v2 has a single `0::/path` line, which is what modern systems use. v1 has one line
/// per controller and only the `memory` one is relevant here. Returns the path with its
/// leading slash, or `None` when neither shape is present.
fn cgroup_relative_path(text: &str) -> Option<String> {
    let v2 = text.lines().find_map(|l| l.strip_prefix("0::"));
    let v1 = || {
        text.lines().find_map(|l| {
            let mut parts = l.splitn(3, ':');
            let (_id, controllers, path) = (parts.next()?, parts.next()?, parts.next()?);
            controllers
                .split(',')
                .any(|c| c == "memory")
                .then_some(path)
        })
    };
    let path = v2.or_else(v1)?.trim();
    // The root cgroup is "/", and appending it would produce a doubled slash for no
    // gain — the sysfs root is already the right directory.
    Some(if path == "/" {
        String::new()
    } else {
        path.to_owned()
    })
}

/// One cgroup limit file's contents. `None` for "unlimited", which cgroup v2 spells
/// `max` and v1 spells as a number so large it exceeds any real machine — both mean the
/// cgroup imposes nothing and the host figures stand.
fn parse_cgroup_limit(text: &str) -> Option<u64> {
    let text = text.trim();
    if text == "max" {
        return None;
    }
    // v1's "no limit" is PAGE_COUNTER_MAX * PAGE_SIZE — an absurd number rather than a
    // sentinel, so anything past a petabyte is treated as unlimited instead.
    text.parse::<u64>().ok().filter(|&n| n < (1 << 50))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
MemTotal:       32791884 kB
MemFree:         1234567 kB
MemAvailable:   16395942 kB
Buffers:          123456 kB
";

    #[test]
    fn parses_total_and_available_in_bytes() {
        let got = parse_meminfo(SAMPLE).expect("both fields present");
        assert_eq!(got.total, 32_791_884 * 1024);
        assert_eq!(got.available, 16_395_942 * 1024);
    }

    /// Half an answer is worse than none: without `MemTotal` the reserve would be
    /// computed as zero, authorising a budget that ignores the whole point of it.
    #[test]
    fn a_missing_field_yields_nothing() {
        assert!(parse_meminfo("MemTotal: 100 kB\n").is_none());
        assert!(parse_meminfo("MemAvailable: 100 kB\n").is_none());
        assert!(parse_meminfo("").is_none());
    }

    /// `MemAvailable` is absent on kernels before 3.14. Falling back to `MemFree`
    /// would badly understate what is obtainable, so there is deliberately no
    /// fallback — the caller uses its static default instead.
    #[test]
    fn no_memavailable_means_no_answer() {
        assert!(parse_meminfo("MemTotal:  100 kB\nMemFree:   50 kB\n").is_none());
    }

    #[test]
    fn cgroup_unlimited_reads_as_no_limit() {
        assert_eq!(parse_cgroup_limit("max\n"), None, "v2 spelling");
        assert_eq!(
            parse_cgroup_limit("9223372036854771712\n"),
            None,
            "v1 spells unlimited as an absurd number, not a sentinel"
        );
        assert_eq!(parse_cgroup_limit("garbage"), None);
    }

    #[test]
    fn cgroup_path_prefers_v2_then_the_memory_controller() {
        assert_eq!(
            cgroup_relative_path("0::/user.slice/user-1000.slice/session.scope\n").as_deref(),
            Some("/user.slice/user-1000.slice/session.scope")
        );
        // v1: several lines, only the memory controller's path is ours.
        assert_eq!(
            cgroup_relative_path("8:cpu,cpuacct:/foo\n7:memory:/bar\n").as_deref(),
            Some("/bar")
        );
        // The root is the sysfs root already; appending "/" would double the slash.
        assert_eq!(cgroup_relative_path("0::/\n").as_deref(), Some(""));
        assert!(cgroup_relative_path("").is_none());
    }

    #[test]
    fn cgroup_real_limit_parses() {
        assert_eq!(parse_cgroup_limit(" 2147483648 \n"), Some(2_147_483_648));
    }
}
