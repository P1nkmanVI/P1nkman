//! Host-memory admission for speculative texture work and reusable upload storage.
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;
/// Universal dynamic VRAM policy for any dedicated GPU (8 / 10 / 12 / 16 / 24 GB+):
/// All containment thresholds are derived dynamically at runtime from DXGI
/// `DXGI_ADAPTER_DESC1::DedicatedVideoMemory` (or live `QueryVideoMemoryInfo`
/// budget when dedicated memory is unavailable) — zero hardcoded MB ceilings:
/// - Stage 1 (Safe): `0%..75%` (`total_vram * 75 / 100`)
/// - Stage 2 (Alert): `75%..85%` (`total_vram * 85 / 100`)
/// - Stage 3 (Critical / Hard Purge): `>= 85%`, live DXGI budget saturation,
///   or Windows commit `< 512 MiB`.
/// - Dynamic ceiling: `88%` of physical VRAM, never exceeding DXGI's live `own.Budget`.
pub(crate) const VRAM_8GB_THRESHOLD: u64 = 8 * GIB + 512 * MIB;
/// Stage 2 (Alert) trigger at 75% of dedicated VRAM (~5,933 MiB on 8 GB RTX 5050).
pub(crate) const VRAM_ALERT_PCT: u64 = 75;
/// Stage 3 (Critical / Hard Purge) trigger at 85% of dedicated VRAM (~6,724 MiB on 8 GB RTX 5050).
pub(crate) const VRAM_TARGET_PCT: u64 = 85;
/// Windows commit floor below which Stage 3 (Critical / Hard Purge) activates immediately.
pub(crate) const CRITICAL_COMMIT_FLOOR_BYTES: u64 = 512 * MIB;
const VRAM_CEILING_PCT: u64 = 88;
const VRAM_ALERT_CLEAR_PCT: u64 = 70;
const VRAM_CLEAR_PCT: u64 = 78;
const MIN_REAL_VRAM_BYTES: u64 = 512 * MIB;

/// 3-stage adaptive VRAM containment policy:
/// - `Safe` (0%..75% VRAM): normal pool & texture retention, no aggressive purge.
/// - `Alert` (75%..85% VRAM): active collection (`limit = 1`), early idle trim,
///   selective scenery mip drop (`> 256 px` with mips), dynamic fingerprint stop.
/// - `Critical` (>= 85% VRAM or Windows commit < 512 MiB): hard purge (`limit = 0`),
///   immediate synchronous pool reclaim preserving ~700+ MB for Windows 11 DWM.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
#[repr(u8)]
pub(crate) enum VramStage {
    Safe = 0,
    Alert = 1,
    Critical = 2,
}

impl VramStage {
    fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Alert,
            2 => Self::Critical,
            _ => Self::Safe,
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Safe => "safe",
            Self::Alert => "alert",
            Self::Critical => "critical",
        }
    }
}

static PRESSURED: AtomicBool = AtomicBool::new(false);
static VRAM_PRESSURED: AtomicBool = AtomicBool::new(false);
static VRAM_STAGE: AtomicU8 = AtomicU8::new(VramStage::Safe as u8);
static DEDICATED_VRAM: AtomicU64 = AtomicU64::new(0);
static HEADROOM: AtomicU64 = AtomicU64::new(u64::MAX);

fn memory_status() -> Option<MEMORYSTATUSEX> {
    // SAFETY: the API initializes the fields of a correctly sized structure.
    let mut status: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    status.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    (unsafe { GlobalMemoryStatusEx(&mut status) } != 0).then_some(status)
}

// The texture sweep may run for seconds inside a single submitted frame.
static NEXT_COMMIT_SAMPLE: AtomicU64 = AtomicU64::new(0);
static AVAILABLE_COMMIT: AtomicU64 = AtomicU64::new(u64::MAX);
static AVAILABLE_PHYS: AtomicU64 = AtomicU64::new(u64::MAX);
static RETRY_UPLOAD_AT: AtomicU64 = AtomicU64::new(0);

pub(crate) fn allocation_error(error: &windows::core::Error) {
    if error.code() == windows::Win32::Foundation::E_OUTOFMEMORY {
        RETRY_UPLOAD_AT.fetch_max(crate::host::coarse_ms().saturating_add(500), Ordering::Relaxed);
    }
}

thread_local! {
    static STAGING_DEFERRED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub(crate) fn note_staging_deferred() {
    STAGING_DEFERRED.set(true);
}

pub(crate) fn take_staging_deferred() -> bool {
    STAGING_DEFERRED.replace(false)
}

pub(crate) fn upload_refused() -> bool {
    crate::host::coarse_ms() < RETRY_UPLOAD_AT.load(Ordering::Relaxed)
}

fn sample_memory(now: u64) -> (u64, u64) {
    let next = NEXT_COMMIT_SAMPLE.load(Ordering::Relaxed);
    if now >= next
        && NEXT_COMMIT_SAMPLE
            .compare_exchange(next, now.saturating_add(32), Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        && let Some(status) = memory_status()
    {
        AVAILABLE_COMMIT.store(status.ullAvailPageFile, Ordering::Relaxed);
        AVAILABLE_PHYS.store(status.ullAvailPhys, Ordering::Relaxed);
    }
    (
        AVAILABLE_COMMIT.load(Ordering::Relaxed),
        AVAILABLE_PHYS.load(Ordering::Relaxed),
    )
}

fn sample_commit(now: u64) -> u64 {
    sample_memory(now).0
}

fn has_upload_reserve(commit: u64, bake: bool) -> bool {
    // Keep a protective floor against Rust/game OOM aborts without starving
    // bindless world textures or one-shot ped decoration bakes when steady-state
    // commit headroom sits in the 1-4 GiB range on 32 GiB hosts.
    commit >= if bake { 256 * MIB } else { 512 * MIB }
}

fn has_growth_reserve_for(commit: u64, physical: u64, bake: bool) -> bool {
    // Growing a fresh upload block in host RAM for non-bake texture staging
    // requires both commit headroom (>= 1 GiB) and free physical RAM above
    // `CRITICAL_HOST_RAM_BYTES` (1,500 MiB) to prevent Windows disk thrashing.
    // One-shot decoration bakes (`bake = true`) remain protected down to 256 MiB commit.
    if bake {
        commit >= 256 * MIB
    } else {
        commit >= 1024 * MIB && physical >= crate::host::CRITICAL_HOST_RAM_BYTES
    }
}

#[cfg(test)]
fn has_growth_reserve(commit: u64, bake: bool) -> bool {
    has_growth_reserve_for(commit, u64::MAX, bake)
}

/// Leave commit for game allocations and submissions already in flight while
/// still letting bindless textures and one-shot material bakes upload.
pub(crate) fn admit_texture_pass(bake: bool) -> bool {
    let now = crate::host::coarse_ms();
    if now < RETRY_UPLOAD_AT.load(Ordering::Relaxed) {
        return false;
    }
    has_upload_reserve(sample_commit(now), bake)
}

/// Whether the staging allocator may commit a brand-new OS upload buffer
/// (reusing existing free ranges in already-committed pool blocks stays open
/// whenever `admit_texture_pass` is true).
pub(crate) fn allow_staging_growth(bake: bool) -> bool {
    let now = crate::host::coarse_ms();
    if now < RETRY_UPLOAD_AT.load(Ordering::Relaxed) {
        return false;
    }
    let (commit, physical) = sample_memory(now);
    has_growth_reserve_for(commit, physical, bake)
}

pub(crate) fn limited_host_ram() -> bool {
    static LIMITED: OnceLock<bool> = OnceLock::new();
    *LIMITED.get_or_init(|| memory_status().is_some_and(|s| s.ullTotalPhys <= 32 * GIB))
}

fn next_pressure_for(was: bool, physical: u64, commit: u64, total_phys: u64) -> bool {
    if total_phys > 0 && total_phys <= 16 * GIB + 512 * MIB {
        // 16 GiB PCs sit around 3.5-4.8 GiB free commit in steady state; use
        // proportional thresholds while keeping `critical_commit` (< 1.5 GiB) strict.
        if was {
            physical < 3 * GIB || commit < 3584 * MIB
        } else {
            physical <= 2 * GIB || commit <= 2560 * MIB
        }
    } else if was {
        physical < 5 * GIB || commit < 6 * GIB
    } else {
        physical <= 3 * GIB || commit <= 4 * GIB
    }
}

#[cfg(test)]
fn next_pressure(was: bool, physical: u64, commit: u64) -> bool {
    next_pressure_for(was, physical, commit, 32 * GIB)
}

/// Called by the once-per-second submission sweep. A failed sample keeps the last good state.
pub(crate) fn refresh() {
    let Some(status) = memory_status() else { return };
    let (physical, commit) = (status.ullAvailPhys, status.ullAvailPageFile);
    AVAILABLE_COMMIT.store(commit, Ordering::Relaxed);
    AVAILABLE_PHYS.store(physical, Ordering::Relaxed);
    HEADROOM.store(physical.min(commit), Ordering::Relaxed);
    let was = pressured();
    let pressure = next_pressure_for(was, physical, commit, status.ullTotalPhys);
    if pressure != was {
        PRESSURED.store(pressure, Ordering::Relaxed);
        if pressure {
            tracing::warn!(
                pid = std::process::id(),
                frame = crate::frame::current(),
                physical_free_mib = physical >> 20,
                available_commit_mib = commit >> 20,
                buffer_mirror_span_mib = crate::shadow::live_span_bytes() >> 20,
                texture_staging_mib = crate::texture::live_staging_bytes() >> 20,
                spare_scratch_mib = crate::host::ALIVE[4].load(Ordering::Relaxed) >> 20,
                game_filled_gpu_texture_mib = crate::resource::KINDS[1].load(Ordering::Relaxed) >> 20,
                "host memory pressure: limiting texture prefetch and idle upload storage"
            );
        } else {
            tracing::info!(physical_free_mib = physical >> 20, available_commit_mib = commit >> 20, "host memory pressure cleared");
        }
    }
}

pub(crate) fn headroom() -> Option<u64> {
    let bytes = HEADROOM.load(Ordering::Relaxed);
    (bytes != u64::MAX).then_some(bytes)
}

pub(crate) fn pressured() -> bool {
    PRESSURED.load(Ordering::Relaxed)
}

pub(crate) fn vram_pressured() -> bool {
    VRAM_PRESSURED.load(Ordering::Relaxed)
}

pub(crate) fn vram_stage() -> VramStage {
    VramStage::from_u8(VRAM_STAGE.load(Ordering::Relaxed))
}

pub(crate) fn set_dedicated_vram_bytes(bytes: u64) {
    if bytes > 0 {
        DEDICATED_VRAM.store(bytes, Ordering::Relaxed);
    }
}

pub(crate) fn dedicated_vram_bytes() -> Option<u64> {
    let bytes = DEDICATED_VRAM.load(Ordering::Relaxed);
    (bytes > 0).then_some(bytes)
}

fn vram_ceiling_env_mb() -> Option<u64> {
    static CEILING_MB: OnceLock<Option<u64>> = OnceLock::new();
    *CEILING_MB.get_or_init(|| {
        std::env::var("VICTORY_VRAM_CEILING_MB")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
    })
}

/// Pure 75% dynamic Alert threshold from physical dedicated VRAM (`total_vram * 75 / 100`):
/// 8 GiB -> 6,144 MiB, 10 GiB -> 7,680 MiB, 12 GiB -> 9,216 MiB, 16 GiB -> 12,288 MiB, 24 GiB -> 18,432 MiB.
fn pct_of(bytes: u64, pct: u64) -> u64 {
    ((u128::from(bytes) * u128::from(pct)) / 100) as u64
}

pub(crate) fn vram_alert_from_total(total_vram: u64) -> u64 {
    pct_of(total_vram, VRAM_ALERT_PCT)
}

/// Pure 85% dynamic Critical target from physical dedicated VRAM (`total_vram * 85 / 100`):
/// 8 GiB -> 6,963 MiB, 10 GiB -> 8,704 MiB, 12 GiB -> 10,444 MiB, 16 GiB -> 13,926 MiB, 24 GiB -> 20,889 MiB.
pub(crate) fn vram_target_from_total(total_vram: u64) -> u64 {
    pct_of(total_vram, VRAM_TARGET_PCT)
}

/// Pure percentage-based usable VRAM ceiling for any GPU (zero hardcoded MB caps):
/// - Applies `VICTORY_VRAM_CEILING_MB` only when explicitly set by the user.
/// - When physical dedicated VRAM from DXGI is known and `budget` belongs to that
///   GPU (`512 MiB <= budget <= total`), caps dynamically at `88%` of physical VRAM
///   (`total * 88 / 100`), never exceeding DXGI's live `budget`.
/// - Otherwise uses live DXGI `budget` directly (preserving synthetic test budgets too).
pub(crate) fn vram_ceiling_for(budget: u64, dedicated: Option<u64>, env_mb: Option<u64>) -> u64 {
    if budget == 0 {
        return 0;
    }
    if let Some(mb) = env_mb {
        return budget.min(mb.clamp(2048, 65_536) * MIB);
    }
    if let Some(total) = dedicated
        && budget >= MIN_REAL_VRAM_BYTES
        && budget <= total
    {
        return budget.min(pct_of(total, VRAM_CEILING_PCT));
    }
    budget
}

pub(crate) fn vram_ceiling_bytes(budget: u64) -> u64 {
    vram_ceiling_for(budget, dedicated_vram_bytes(), vram_ceiling_env_mb())
}

/// Dynamic 85% VRAM soft target for any GPU:
/// uses `total_vram * 85 / 100` (clamped to the calibrated ceiling) whenever
/// physical dedicated VRAM is known, otherwise `85%` of the calibrated ceiling.
pub(crate) fn vram_soft_target_for(budget: u64, dedicated: Option<u64>, env_mb: Option<u64>) -> u64 {
    let ceiling = vram_ceiling_for(budget, dedicated, env_mb);
    if ceiling == 0 {
        return 0;
    }
    if env_mb.is_none()
        && let Some(total) = dedicated
        && budget >= MIN_REAL_VRAM_BYTES
        && budget <= total
    {
        return vram_target_from_total(total).min(ceiling);
    }
    pct_of(ceiling, VRAM_TARGET_PCT)
}

pub(crate) fn vram_soft_target_bytes(budget: u64) -> u64 {
    vram_soft_target_for(budget, dedicated_vram_bytes(), vram_ceiling_env_mb())
}

/// Stage 2 (75% Alert) VRAM threshold for any GPU (`total_vram * 75 / 100`,
/// clamped to the calibrated ceiling, e.g. ~5,933 MiB on 7,911 MiB RTX 5050).
pub(crate) fn vram_alert_target_for(budget: u64, dedicated: Option<u64>, env_mb: Option<u64>) -> u64 {
    let ceiling = vram_ceiling_for(budget, dedicated, env_mb);
    if ceiling == 0 {
        return 0;
    }
    if env_mb.is_none()
        && let Some(total) = dedicated
        && budget >= MIN_REAL_VRAM_BYTES
        && budget <= total
    {
        return vram_alert_from_total(total).min(ceiling);
    }
    pct_of(ceiling, VRAM_ALERT_PCT)
}

pub(crate) fn vram_alert_target_bytes(budget: u64) -> u64 {
    vram_alert_target_for(budget, dedicated_vram_bytes(), vram_ceiling_env_mb())
}

pub(crate) fn next_vram_pressure(
    was: bool,
    budget: u64,
    used: u64,
    dedicated: Option<u64>,
    env_mb: Option<u64>,
) -> bool {
    let target = vram_soft_target_for(budget, dedicated, env_mb);
    if target == 0 {
        return false;
    }
    if was {
        // Hysteresis: clear containment once usage drops below 78/85 (~91.8%) of the 85% target.
        u128::from(used) * u128::from(VRAM_TARGET_PCT) >= u128::from(target) * u128::from(VRAM_CLEAR_PCT)
    } else {
        used >= target
    }
}

/// Computes the next 3-stage VRAM containment state with hysteresis:
/// - `Critical`: `used >= 85%` (clears below `78%`) or `commit < 512 MiB` / recent OOM.
/// - `Alert`: `used >= 75%` (clears below `70%`).
/// - `Safe`: below Alert threshold and healthy commit headroom.
pub(crate) fn next_vram_stage(
    prev: VramStage,
    budget: u64,
    used: u64,
    dedicated: Option<u64>,
    env_mb: Option<u64>,
    commit: u64,
    oom_backoff: bool,
) -> VramStage {
    if oom_backoff
        || commit < CRITICAL_COMMIT_FLOOR_BYTES
        || (budget >= MIN_REAL_VRAM_BYTES && used >= budget)
    {
        return VramStage::Critical;
    }
    let was_critical = prev == VramStage::Critical;
    if next_vram_pressure(was_critical, budget, used, dedicated, env_mb) {
        return VramStage::Critical;
    }
    let alert = vram_alert_target_for(budget, dedicated, env_mb);
    if alert == 0 {
        return VramStage::Safe;
    }
    let stay_alert = prev >= VramStage::Alert
        && u128::from(used) * u128::from(VRAM_ALERT_PCT) >= u128::from(alert) * u128::from(VRAM_ALERT_CLEAR_PCT);
    if used >= alert || stay_alert {
        VramStage::Alert
    } else {
        VramStage::Safe
    }
}

/// Updates the 3-stage VRAM containment state dynamically for any GPU:
/// - Stage 1 (`Safe`, 0%..75%): normal caching & spares.
/// - Stage 2 (`Alert`, 75%..85%): active collection (`limit = 1`), idle trim, selective mip drop.
/// - Stage 3 (`Critical`, >= 85% or Windows commit < 512 MiB): hard purge (`limit = 0`), preserving DWM headroom.
pub(crate) fn update_vram(memory: Option<[u64; 4]>) -> VramStage {
    let prev = vram_stage();
    let dedicated = dedicated_vram_bytes();
    let env_mb = vram_ceiling_env_mb();
    let now = crate::host::coarse_ms();
    let commit = sample_commit(now);
    let oom_backoff = now < RETRY_UPLOAD_AT.load(Ordering::Relaxed);
    let next = match memory {
        Some([budget, used, ..]) => next_vram_stage(prev, budget, used, dedicated, env_mb, commit, oom_backoff),
        None if oom_backoff || commit < CRITICAL_COMMIT_FLOOR_BYTES => VramStage::Critical,
        None => VramStage::Safe,
    };
    let critical = next == VramStage::Critical;
    VRAM_STAGE.store(next as u8, Ordering::Relaxed);
    VRAM_PRESSURED.store(critical, Ordering::Relaxed);
    if next != prev {
        let (budget, used) = memory.map_or((0, 0), |[b, u, ..]| (b, u));
        let ceiling = vram_ceiling_for(budget, dedicated, env_mb);
        let alert = vram_alert_target_for(budget, dedicated, env_mb);
        let target = vram_soft_target_for(budget, dedicated, env_mb);
        match next {
            VramStage::Critical => {
                tracing::warn!(
                    frame = crate::frame::current(),
                    stage = next.as_str(),
                    used_mib = used >> 20,
                    dedicated_mib = dedicated.unwrap_or(0) >> 20,
                    budget_mib = budget >> 20,
                    ceiling_mib = ceiling >> 20,
                    alert_mib = alert >> 20,
                    target_mib = target >> 20,
                    available_commit_mib = commit >> 20,
                    "vram stage 3 (critical / hard purge): >=85% VRAM or <512 MiB commit, preserving Windows 11 DWM headroom"
                );
            }
            VramStage::Alert => {
                tracing::warn!(
                    frame = crate::frame::current(),
                    stage = next.as_str(),
                    used_mib = used >> 20,
                    dedicated_mib = dedicated.unwrap_or(0) >> 20,
                    budget_mib = budget >> 20,
                    alert_mib = alert >> 20,
                    target_mib = target >> 20,
                    available_commit_mib = commit >> 20,
                    "vram stage 2 (alert): >=75% VRAM, enabling active collection and selective mip drop"
                );
            }
            VramStage::Safe => {
                tracing::info!(
                    frame = crate::frame::current(),
                    stage = next.as_str(),
                    used_mib = used >> 20,
                    alert_mib = alert >> 20,
                    target_mib = target >> 20,
                    available_commit_mib = commit >> 20,
                    "vram stage 1 (safe): containment cleared"
                );
            }
        }
    }
    next
}

/// True when OS commit headroom is critically low (< 1.5 GiB) or an allocation
/// just failed with `E_OUTOFMEMORY`, requiring emergency throttling.
pub(crate) fn critical_commit() -> bool {
    let now = crate::host::coarse_ms();
    now < RETRY_UPLOAD_AT.load(Ordering::Relaxed) || sample_commit(now) < 1536 * MIB
}

/// A soft per-frame admission limit. A single texture can cross it. The
/// queue retains unserved slots so they can be retried on later frames.
pub(crate) fn prefetch_limit(bytes: u64, pressure: bool, critical: bool, limited: bool, staging: u64) -> u64 {
    if critical {
        bytes.min(16 << 20)
    } else if pressure || (limited && staging >= GIB) {
        bytes.min(64 << 20)
    } else {
        bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bake_and_lookahead_leave_room_for_live_game_allocations() {
        for (commit, bake, lookahead, growth) in [
            (128 * MIB, false, false, false),
            (256 * MIB, true, false, false),
            (512 * MIB, true, true, false),
            (768 * MIB, true, true, false),
            (1024 * MIB, true, true, true),
        ] {
            assert_eq!(has_upload_reserve(commit, true), bake);
            assert_eq!(has_upload_reserve(commit, false), lookahead);
            assert_eq!(has_growth_reserve(commit, false), growth);
        }
        // Physical RAM below 1,500 MiB blocks non-bake staging growth even with ample commit,
        // while keeping one-shot decoration bakes (`bake = true`) protected.
        assert!(!has_growth_reserve_for(8 * GIB, 1_499 * MIB, false));
        assert!(!has_growth_reserve_for(8 * GIB, 82 * MIB, false));
        assert!(has_growth_reserve_for(8 * GIB, 1_500 * MIB, false));
        assert!(has_growth_reserve_for(512 * MIB, 82 * MIB, true));
        assert!(!has_growth_reserve_for(128 * MIB, 82 * MIB, true));
    }

    #[test]
    fn either_constraint_enters_pressure_and_both_must_recover() {
        assert!(next_pressure(false, 12 * GIB, 4 * GIB));
        assert!(next_pressure(false, 3 * GIB, 16 * GIB));
        assert!(next_pressure(true, 4 * GIB, 7 * GIB));
        assert!(next_pressure(true, 6 * GIB, 5 * GIB));
        assert!(!next_pressure(true, 5 * GIB, 6 * GIB));
        assert!(!next_pressure(false, 4 * GIB, 5 * GIB));
    }

    #[test]
    fn vram_dynamic_85_percent_scales_across_all_gpu_tiers() {
        // Pure 85% dynamic VRAM target formula (`total_vram * 85 / 100`) across GPU tiers:
        // 8 GiB -> ~6,963 MiB, 12 GiB -> ~10,444 MiB, 16 GiB -> ~13,926 MiB, 24 GiB -> ~20,889 MiB.
        assert_eq!(vram_target_from_total(8 * GIB) >> 20, 6_963);
        assert_eq!(vram_target_from_total(12 * GIB) >> 20, 10_444);
        assert_eq!(vram_target_from_total(16 * GIB) >> 20, 13_926);
        assert_eq!(vram_target_from_total(24 * GIB) >> 20, 20_889);

        // With driver-reported DedicatedVideoMemory (pure percentage, zero hardcoded MB caps):
        // 8 GB GPU: 88% ceiling = 7,208 MiB (or lower live DXGI budget), 75% alert = 6,144 MiB, 85% target = 6,963 MiB.
        assert_eq!(vram_ceiling_for(8 * GIB, Some(8 * GIB), None) >> 20, 7_208);
        assert_eq!(vram_ceiling_for(7_143 * MIB, Some(8 * GIB), None), 7_143 * MIB);
        assert_eq!(vram_alert_target_for(7_143 * MIB, Some(8 * GIB), None) >> 20, 6_144);
        assert_eq!(vram_soft_target_for(7_143 * MIB, Some(8 * GIB), None) >> 20, 6_963);

        // 12 / 16 / 24 GB GPUs: dynamic 85% target & 88% ceiling preserve ~15% for Windows DWM.
        assert_eq!(vram_soft_target_for(11_500 * MIB, Some(12 * GIB), None) >> 20, 10_444);
        assert_eq!(vram_soft_target_for(15_500 * MIB, Some(16 * GIB), None) >> 20, 13_926);
        assert_eq!(vram_soft_target_for(23_500 * MIB, Some(24 * GIB), None) >> 20, 20_889);
        assert_eq!(vram_ceiling_for(12 * GIB, Some(12 * GIB), None) >> 20, 10_813);
        assert_eq!(vram_ceiling_for(16 * GIB, Some(16 * GIB), None) >> 20, 14_417);
        assert_eq!(vram_ceiling_for(24 * GIB, Some(24 * GIB), None) >> 20, 21_626);

        // Synthetic unit-test budgets (tiny or larger than host dedicated VRAM) remain untouched.
        assert_eq!(vram_ceiling_for(100, Some(8 * GIB), None), 100);
        assert_eq!(vram_ceiling_for(100 * GIB, Some(8 * GIB), None), 100 * GIB);

        // Explicit env override still works and clamps to budget.
        assert_eq!(vram_ceiling_for(8 * GIB, Some(8 * GIB), Some(6_800)), 6_800 * MIB);
        assert_eq!(vram_ceiling_for(7_000 * MIB, Some(8 * GIB), Some(7_200)), 7_000 * MIB);

        // Proactive containment hysteresis (85% enter / 78% clear) on 8 GB and 16 GB GPUs.
        assert!(!next_vram_pressure(false, 7_143 * MIB, 6_900 * MIB, Some(8 * GIB), None));
        assert!(next_vram_pressure(false, 7_143 * MIB, 6_964 * MIB, Some(8 * GIB), None));
        assert!(next_vram_pressure(true, 7_143 * MIB, 6_500 * MIB, Some(8 * GIB), None));
        assert!(!next_vram_pressure(true, 7_143 * MIB, 6_350 * MIB, Some(8 * GIB), None));

        assert!(!next_vram_pressure(false, 15_500 * MIB, 13_800 * MIB, Some(16 * GIB), None));
        assert!(next_vram_pressure(false, 15_500 * MIB, 13_930 * MIB, Some(16 * GIB), None));
        assert!(!next_vram_pressure(true, 15_500 * MIB, 12_700 * MIB, Some(16 * GIB), None));
    }

    #[test]
    fn prefetch_respects_existing_budgets_and_resumes_after_recovery() {
        assert_eq!(prefetch_limit(512 << 20, false, false, true, GIB), 64 << 20);
        assert_eq!(prefetch_limit(512 << 20, false, false, true, GIB - 1), 512 << 20);
        assert_eq!(prefetch_limit(512 << 20, false, false, false, 2 * GIB), 512 << 20);
        assert_eq!(prefetch_limit(0, true, true, false, 0), 0);
        assert_eq!(prefetch_limit(8 << 20, true, true, false, 0), 8 << 20);
        assert_eq!(prefetch_limit(512 << 20, true, false, false, 0), 64 << 20);
        assert_eq!(prefetch_limit(512 << 20, true, true, false, 0), 16 << 20);
        assert_eq!(prefetch_limit(192 << 20, false, false, false, 0), 192 << 20);
    }

    #[test]
    fn sixteen_gb_host_pressure_thresholds_avoid_false_throttling_at_four_gb_free() {
        // 4.6 GiB phys / 4.0 GiB commit free on 16 GiB RAM should stay unpressured.
        assert!(!next_pressure_for(false, 4650 * MIB, 4047 * MIB, 16 * GIB));
        // Real low-memory conditions still trigger and recover with hysteresis.
        assert!(next_pressure_for(false, 2 * GIB, 4 * GIB, 16 * GIB));
        assert!(next_pressure_for(false, 4 * GIB, 2560 * MIB, 16 * GIB));
        assert!(next_pressure_for(true, 2800 * MIB, 4 * GIB, 16 * GIB));
        assert!(!next_pressure_for(true, 3 * GIB, 3584 * MIB, 16 * GIB));
    }

    #[test]
    fn three_stage_vram_containment_transitions_and_commit_floor() {
        // RTX 5050 8 GB telemetry profile: dedicated = 7,911 MiB, DXGI budget = 7,143 MiB.
        // Stage 2 (75% Alert) = 5,933 MiB; Stage 3 (85% Critical) = 6,724 MiB.
        let dedicated = Some(7_911 * MIB);
        let budget = 7_143 * MIB;
        let healthy_commit = 4 * GIB;
        assert_eq!(vram_alert_target_for(budget, dedicated, None) >> 20, 5_933);
        assert_eq!(vram_soft_target_for(budget, dedicated, None) >> 20, 6_724);

        // Stage 1: Safe (0%..75%).
        assert_eq!(
            next_vram_stage(VramStage::Safe, budget, 5_500 * MIB, dedicated, None, healthy_commit, false),
            VramStage::Safe
        );
        // Enter Stage 2: Alert (>= 75%, ~5,933 MiB).
        assert_eq!(
            next_vram_stage(VramStage::Safe, budget, 5_934 * MIB, dedicated, None, healthy_commit, false),
            VramStage::Alert
        );
        // Hysteresis stays in Alert above 70% (~5,537 MiB) and clears below 70%.
        assert_eq!(
            next_vram_stage(VramStage::Alert, budget, 5_650 * MIB, dedicated, None, healthy_commit, false),
            VramStage::Alert
        );
        assert_eq!(
            next_vram_stage(VramStage::Alert, budget, 5_500 * MIB, dedicated, None, healthy_commit, false),
            VramStage::Safe
        );
        // Enter Stage 3: Critical (>= 85%, ~6,724 MiB).
        assert_eq!(
            next_vram_stage(VramStage::Alert, budget, 6_725 * MIB, dedicated, None, healthy_commit, false),
            VramStage::Critical
        );
        // Drop from Critical below 78% (~6,170 MiB) lands in Alert first, then Safe below 70%.
        assert_eq!(
            next_vram_stage(VramStage::Critical, budget, 6_100 * MIB, dedicated, None, healthy_commit, false),
            VramStage::Alert
        );
        // Windows commit < 512 MiB triggers Stage 3 immediately even when VRAM usage is low.
        assert_eq!(
            next_vram_stage(VramStage::Safe, budget, 2_000 * MIB, dedicated, None, 400 * MIB, false),
            VramStage::Critical
        );
        // Recent OOM backoff also forces Stage 3.
        assert_eq!(
            next_vram_stage(VramStage::Safe, budget, 2_000 * MIB, dedicated, None, healthy_commit, true),
            VramStage::Critical
        );
        // Live DXGI QueryVideoMemoryInfo budget saturation forces Stage 3 even below 85% physical VRAM.
        assert_eq!(
            next_vram_stage(VramStage::Safe, 5_800 * MIB, 5_800 * MIB, dedicated, None, healthy_commit, false),
            VramStage::Critical
        );
    }

    #[test]
    fn universal_dynamic_dxgi_three_stage_scales_across_8_10_12_16_24_gb_gpus() {
        let healthy_commit = 8 * GIB;
        // (total_vram, live_dxgi_budget, expected_75_mib, expected_85_mib, expected_88_mib)
        let tiers = [
            (8 * GIB, 7_400 * MIB, 6_144, 6_963, 7_208),
            (10 * GIB, 9_500 * MIB, 7_680, 8_704, 9_011),
            (12 * GIB, 11_500 * MIB, 9_216, 10_444, 10_813),
            (16 * GIB, 15_500 * MIB, 12_288, 13_926, 14_417),
            (24 * GIB, 23_500 * MIB, 18_432, 20_889, 21_626),
        ];
        for (total, live_budget, alert_mib, target_mib, ceil_mib) in tiers {
            assert_eq!(vram_alert_from_total(total) >> 20, alert_mib);
            assert_eq!(vram_target_from_total(total) >> 20, target_mib);
            assert_eq!(vram_ceiling_for(live_budget, Some(total), None) >> 20, ceil_mib);
            assert_eq!(vram_alert_target_for(live_budget, Some(total), None) >> 20, alert_mib);
            assert_eq!(vram_soft_target_for(live_budget, Some(total), None) >> 20, target_mib);

            let alert_bytes = vram_alert_from_total(total);
            let target_bytes = vram_target_from_total(total);
            // Stage 1: Safe below 75%.
            assert_eq!(
                next_vram_stage(VramStage::Safe, live_budget, alert_bytes - MIB, Some(total), None, healthy_commit, false),
                VramStage::Safe
            );
            // Stage 2: Alert at 75%..85%.
            assert_eq!(
                next_vram_stage(VramStage::Safe, live_budget, alert_bytes, Some(total), None, healthy_commit, false),
                VramStage::Alert
            );
            // Stage 3: Critical at >= 85%.
            assert_eq!(
                next_vram_stage(VramStage::Alert, live_budget, target_bytes, Some(total), None, healthy_commit, false),
                VramStage::Critical
            );
            // Fallback when DedicatedVideoMemory is unknown uses live DXGI budget percentages directly.
            assert_eq!(vram_alert_target_for(total, None, None) >> 20, alert_mib);
            assert_eq!(vram_soft_target_for(total, None, None) >> 20, target_mib);
        }
    }
}
