//! Which processors our own threads run on: hybrid P-core/E-core selection
//! plus explicit homogeneous core-tier partitioning (e.g. 6C/6T CPUs) so
//! background and helper pools do not preempt the title's primary threads on
//! cores 0-1. `cpu_sets` and `keep_on_fast_cores` build on
//! fxceless/victory-fork's (`framelog.rs`, commit 0b0074e0, GPL-3.0-or-later).

use std::sync::OnceLock;

/// Scheduling tier for host threads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tier {
    /// Threads a frame directly waits on (`victory-submit`, `victory-relay`,
    /// `victory-aside`, `victory-heap-sync`, `victory-late-wait`).
    Critical,
    /// Burst parallel helpers (`victory-wide`, `victory-land`).
    Helper,
    /// Bulk parallel pools (`victory-list`, `victory-under`, `victory-records`,
    /// `victory-views`).
    Bulk,
    /// Continuous low-priority background work (`uploads-ahead`,
    /// `victory-pool-grow`, `victory-pipelines`, `victory-picked`,
    /// `victory-ask-early`, `victory-card-awake`, `dstorage`).
    Background,
}

/// The system's processor sets of group 0: each one's id and its efficiency
/// class (higher is faster), ordered by logical processor index.
fn cpu_sets() -> Vec<(u32, u8)> {
    use windows_sys::Win32::System::SystemInformation::{GetSystemCpuSetInformation, SYSTEM_CPU_SET_INFORMATION};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let mut bytes = 0u32;
    // SAFETY: a size query, then the call into a buffer of that size.
    unsafe { GetSystemCpuSetInformation(std::ptr::null_mut(), 0, &mut bytes, GetCurrentProcess(), 0) };
    let mut buffer = vec![0u64; (bytes as usize).div_ceil(8).max(1)];
    // SAFETY: as above; the buffer holds `bytes` bytes, aligned for the records.
    let ok = unsafe { GetSystemCpuSetInformation(buffer.as_mut_ptr().cast(), bytes, &mut bytes, GetCurrentProcess(), 0) };
    let mut sets = Vec::new();
    let mut at = 0usize;
    while ok != 0 && at + size_of::<SYSTEM_CPU_SET_INFORMATION>() <= bytes as usize {
        // SAFETY: a record inside the buffer; each says its own size.
        let info = unsafe { &*(buffer.as_ptr().cast::<u8>().add(at).cast::<SYSTEM_CPU_SET_INFORMATION>()) };
        if info.Size == 0 {
            break;
        }
        // SAFETY: the record's type is CpuSet (the only one there is).
        let set = unsafe { &info.Anonymous.CpuSet };
        if set.Group == 0 {
            sets.push((set.Id, set.EfficiencyClass));
        }
        at += info.Size as usize;
    }
    sets
}

fn cached_sets() -> &'static [(u32, u8)] {
    static SETS: OnceLock<Vec<(u32, u8)>> = OnceLock::new();
    SETS.get_or_init(cpu_sets)
}

/// The sets of the fastest class, if there are slower ones too.
fn fast_of(sets: &[(u32, u8)]) -> Vec<u32> {
    let best = sets.iter().map(|c| c.1).max().unwrap_or(0);
    let fast: Vec<u32> = sets.iter().filter(|c| c.1 == best).map(|c| c.0).collect();
    if fast.len() == sets.len() { Vec::new() } else { fast }
}

/// The sets outside the fastest class, if the CPU is hybrid.
fn slow_of(sets: &[(u32, u8)]) -> Vec<u32> {
    let best = sets.iter().map(|c| c.1).max().unwrap_or(0);
    sets.iter().filter(|c| c.1 < best).map(|c| c.0).collect()
}

/// Computes preferred CPU-set IDs for `tier`.
///
/// - On hybrid CPUs (multiple efficiency classes): honors `VICTORY_FAST_CORES`
///   (`stage`) for `Critical`/`Helper`/`Bulk` and steers `Background` (plus
///   `Bulk` when `stage < 3`) onto efficiency cores.
/// - On homogeneous CPUs (`4..=16` logical cores) with `layout == true`
///   (`VICTORY_CORE_LAYOUT`, default on): reserves the first `reserve` cores
///   (`(n / 3).clamp(1, 2)` -> cores `0..2` on a 6C/6T CPU) free of our worker
///   pools so the title's main/render threads stay unobstructed:
///   * `Critical`: `sets[reserve .. (n - 1).max(reserve + 1)]` (cores `2..5` on 6C)
///   * `Helper` / `Bulk`: `sets[reserve .. n]` (cores `2..6` on 6C)
///   * `Background`: `sets[n - bg .. n]` where `bg = (n / 3).clamp(1, 4)` (cores `4..6` on 6C)
fn partition_sets(sets: &[(u32, u8)], tier: Tier, stage: u32, layout: bool) -> Vec<u32> {
    if sets.is_empty() {
        return Vec::new();
    }
    let fast = fast_of(sets);
    if !fast.is_empty() {
        return match tier {
            Tier::Critical if stage >= 1 => fast,
            Tier::Helper if stage >= 2 => fast,
            Tier::Bulk if stage >= 3 => fast,
            Tier::Bulk | Tier::Background if layout => slow_of(sets),
            _ => Vec::new(),
        };
    }
    // Homogeneous CPU: apply explicit core-tier layout when enabled and 4..=16 cores.
    if !layout || sets.len() < 4 || sets.len() > 16 {
        return Vec::new();
    }
    let n = sets.len();
    let reserve = (n / 3).clamp(1, 2);
    let bg = (n / 3).clamp(1, 4);
    let slice = match tier {
        Tier::Critical => {
            if stage == 0 {
                return Vec::new();
            }
            let end = (n - 1).max(reserve + 1);
            &sets[reserve..end]
        }
        Tier::Helper | Tier::Bulk => &sets[reserve..n],
        Tier::Background => &sets[n - bg..n],
    };
    slice.iter().map(|c| c.0).collect()
}

/// `VICTORY_FAST_CORES`: 0: no thread asks; 1: the threads a frame waits for
/// one by one; 2 (the default): the threads they share their work with too;
/// 3: those that work beside the rest as well.
fn stage() -> u32 {
    static STAGE: OnceLock<u32> = OnceLock::new();
    *STAGE.get_or_init(|| std::env::var("VICTORY_FAST_CORES").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(2))
}

/// `VICTORY_CORE_LAYOUT`: 1 (default) enables explicit core-tier partitioning
/// on homogeneous 4..=16-core CPUs and E-core steering for background threads
/// on hybrid CPUs; 0 disables it.
fn core_layout_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_CORE_LAYOUT").is_none_or(|v| v != "0"))
}

/// Asks the system to run the calling thread, which a frame waits for, on the
/// critical/fast CPU set tier (soft preference via `SetThreadSelectedCpuSets`).
pub fn keep_on_fast_cores(what: &str) {
    ask(what, Tier::Critical);
}

/// [`keep_on_fast_cores`] for one of the threads a submission's preparing or
/// a landing shares its work with.
pub fn keep_helper_on_fast_cores(what: &str) {
    ask(what, Tier::Helper);
}

/// [`keep_on_fast_cores`] for one of many threads whose work is ahead of what
/// a frame waits for.
pub fn keep_many_on_fast_cores(what: &str) {
    ask(what, Tier::Bulk);
}

/// Steers continuous background work onto auxiliary/efficiency cores so it
/// does not preempt primary title or submission threads.
pub fn keep_background_on_aux_cores(what: &str) {
    ask(what, Tier::Background);
}

/// Lowers the calling background thread to `THREAD_PRIORITY_BELOW_NORMAL` and
/// steers it onto auxiliary cores (`keep_background_on_aux_cores`).
pub fn keep_background_thread(what: &str) {
    use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL};
    // SAFETY: plain priority adjustment on our own helper thread.
    unsafe {
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
    }
    keep_background_on_aux_cores(what);
}

/// Applies a soft CPU-set preference for `thread` onto logical processor
/// `logical_core` (wrapped into available group-0 sets). Returns true on
/// success. Used by HLE affinity remapping to avoid hard single-core stalls.
pub fn prefer_core_for_thread(thread: *mut std::ffi::c_void, logical_core: usize) -> bool {
    use windows_sys::Win32::System::Threading::SetThreadSelectedCpuSets;
    let sets = cached_sets();
    if sets.is_empty() {
        return false;
    }
    let id = sets[logical_core % sets.len()].0;
    // SAFETY: caller passes a valid Win32 thread handle; `id` is a system CPU-set id.
    unsafe { SetThreadSelectedCpuSets(thread, &id, 1) != 0 }
}

fn ask(what: &str, tier: Tier) {
    use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadSelectedCpuSets};
    let sets = cached_sets();
    let chosen = partition_sets(sets, tier, stage(), core_layout_on());
    if chosen.is_empty() {
        return;
    }
    // SAFETY: our own thread and a list of the system's own set ids.
    let ok = unsafe { SetThreadSelectedCpuSets(GetCurrentThread(), chosen.as_ptr(), chosen.len() as u32) };
    if ok == 0 {
        tracing::warn!(what, ?tier, error = %std::io::Error::last_os_error(), "the thread could not be assigned its CPU sets");
    } else {
        tracing::info!(
            what,
            ?tier,
            chosen = chosen.len(),
            of = sets.len(),
            "thread CPU-set tier applied (VICTORY_CORE_LAYOUT=0 / VICTORY_FAST_CORES=0: off)"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fast_cores_are_those_of_the_best_class_if_there_is_a_worse_one() {
        assert_eq!(fast_of(&[(256, 1), (257, 1), (258, 0)]), [256, 257]);
        assert!(fast_of(&[(256, 0), (257, 0)]).is_empty());
        assert!(fast_of(&[]).is_empty());
        assert!(!cpu_sets().is_empty());
        std::thread::spawn(|| keep_on_fast_cores("test")).join().unwrap();
    }

    #[test]
    fn homogeneous_six_core_layout_protects_primary_cores() {
        let six: Vec<(u32, u8)> = (0..6).map(|i| (256 + i, 0)).collect();
        // Cores 0-1 (256, 257) stay free of host worker pools on 6C/6T.
        assert_eq!(partition_sets(&six, Tier::Critical, 2, true), vec![258, 259, 260]);
        assert_eq!(partition_sets(&six, Tier::Helper, 2, true), vec![258, 259, 260, 261]);
        assert_eq!(partition_sets(&six, Tier::Bulk, 2, true), vec![258, 259, 260, 261]);
        assert_eq!(partition_sets(&six, Tier::Background, 2, true), vec![260, 261]);
        // Disabled layout leaves homogeneous CPUs unconstrained.
        assert!(partition_sets(&six, Tier::Critical, 2, false).is_empty());
        assert!(partition_sets(&six, Tier::Background, 2, false).is_empty());
        // 2-core CPUs are too small to partition.
        let two = vec![(256, 0), (257, 0)];
        assert!(partition_sets(&two, Tier::Background, 2, true).is_empty());
    }

    #[test]
    fn hybrid_layout_separates_p_and_e_cores() {
        let hybrid = vec![(256, 1), (257, 1), (258, 1), (259, 1), (260, 0), (261, 0)];
        assert_eq!(partition_sets(&hybrid, Tier::Critical, 2, true), vec![256, 257, 258, 259]);
        assert_eq!(partition_sets(&hybrid, Tier::Helper, 2, true), vec![256, 257, 258, 259]);
        assert_eq!(partition_sets(&hybrid, Tier::Bulk, 2, true), vec![260, 261]);
        assert_eq!(partition_sets(&hybrid, Tier::Bulk, 3, true), vec![256, 257, 258, 259]);
        assert_eq!(partition_sets(&hybrid, Tier::Background, 2, true), vec![260, 261]);
    }
}