//! Host memory for the textures the title fills, and for their staging
//! buffers, cut from blocks made once.
//!
//! Each such texture had an allocation of its own and its staging buffer
//! another. On an RX 7900 XTX an allocation takes about 310 microseconds
//! whatever its size, on one thread (`texture::allocation_costs`), and a
//! texture cut from a block 6: the pass that makes the textures shaders
//! pick by index (`draw.rs` `heap_textures`) got through 350-500 of the
//! 24,000 queued when the world loads in each of its 250 ms turns, some
//! forty seconds of a world without its textures.
//! `VICTORY_TEXTURE_POOL=0` makes every one an allocation of its own again.
//!
//! A piece given back is used again only two seconds later (nothing the card
//! was told before that is still running: Windows resets a card that takes
//! that long over one submission), and by then no list names it (the owners' rule, as for allocations of their own): a
//! texture's piece comes back when the last reference to the host texture
//! goes (an object of ours the texture holds says so), a staging buffer's
//! when the last list that recorded its copy is reset.

use std::collections::{BTreeMap, VecDeque};
use std::ffi::c_void;
use std::sync::{Condvar, Mutex, OnceLock};
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use windows::Win32::Graphics::Direct3D12::*;
use windows::core::{GUID, HRESULT, IUnknown, Interface};

/// Default block size in bytes for unit tests and 8 GiB texture pools.
/// On <= 8 GiB VRAM hosts, texture heaps and staging / UAV / shared buffer
/// pools use 32 MiB blocks so 4 MiB and 8 MiB buffer mirrors suballocate
/// inside the pooled path (`alloc <= block_sz / 2`) without falling back to
/// standalone OS `CreateCommittedResource` calls (`VICTORY_POOL_BLOCK_MB`
/// overrides, clamped to 16..=64 MiB).
const BLOCK: u64 = 32 << 20;
/// VRAM budget threshold (8 GiB) below/at which compact pool blocks & spare caps apply.
const LOW_VRAM_BUDGET: u64 = 8 << 30;
/// Blocks with nothing in them that are kept.
const SPARE: usize = 2;
/// Empty texture blocks kept while the card has room (> 8 GiB VRAM).
const SPARE_KEPT: usize = 48;
/// Empty texture blocks kept while an <= 8 GiB card has room (4 * 32 MiB = 128 MiB).
const SPARE_KEPT_8GB: usize = 4;
/// Empty staging blocks kept in host RAM (2 * 32 MiB = 64 MiB max).
const STAGING_KEPT: usize = 2;
/// Empty blocks kept for pooled GPU UAV buffers (2 * 32 MiB = 64 MiB max).
const UAV_KEPT: usize = 2;
/// Empty blocks kept for pooled CPU-mapped shared buffers (2 * 32 MiB = 64 MiB max).
const SHARED_KEPT: usize = 2;
/// How many empty blocks are kept now (for textures).
static SPARES: AtomicUsize = AtomicUsize::new(SPARE);
/// Separate spare retention cap for host-RAM pools (`STAGING`, `SHARED_BUFFERS`),
/// so `Pool::rested()` never reverts host-RAM pools back to VRAM `SPARES` when
/// free physical RAM is `< 1,500 MiB` while `VramStage` is still `Safe`.
static HOST_RAM_SPARES: AtomicUsize = AtomicUsize::new(STAGING_KEPT);

/// Selects pool block size in bytes from local VRAM budget, pool kind, and
/// optional `VICTORY_POOL_BLOCK_MB` override (clamped to 16..=64 MiB).
pub(crate) fn block_size_for(vram_budget: Option<u64>, is_texture: bool, env_mb: Option<u64>) -> u64 {
    if let Some(mb) = env_mb {
        return mb.clamp(16, 64) << 20;
    }
    let low_vram = vram_budget.is_some_and(|b| b > 0 && b <= LOW_VRAM_BUDGET);
    match (low_vram, is_texture) {
        (true, true) => 32 << 20,
        (true, false) => 32 << 20,
        (false, true) => 64 << 20,
        (false, false) => 32 << 20,
    }
}

fn pool_block_env_mb() -> Option<u64> {
    std::env::var("VICTORY_POOL_BLOCK_MB")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
}

fn texture_block_bytes() -> u64 {
    static BYTES: OnceLock<u64> = OnceLock::new();
    *BYTES.get_or_init(|| {
        let budget = crate::host::video_memory_ready().map(|m| m[0]);
        let bytes = block_size_for(budget, true, pool_block_env_mb());
        tracing::info!(
            texture_block_mib = bytes >> 20,
            vram_budget_mib = budget.unwrap_or(0) >> 20,
            "pool: texture heap block calibrated (VICTORY_POOL_BLOCK_MB overrides)"
        );
        bytes
    })
}

fn buffer_block_bytes() -> u64 {
    static BYTES: OnceLock<u64> = OnceLock::new();
    *BYTES.get_or_init(|| {
        let budget = crate::host::video_memory_ready().map(|m| m[0]);
        let bytes = block_size_for(budget, false, pool_block_env_mb());
        tracing::info!(
            buffer_block_mib = bytes >> 20,
            vram_budget_mib = budget.unwrap_or(0) >> 20,
            "pool: staging/buffer heap block calibrated (VICTORY_POOL_BLOCK_MB overrides)"
        );
        bytes
    })
}

/// Sets how many empty blocks stay, by the card's memory (budget and use):
/// [`SPARE_KEPT`] below 85 % ([`crate::host::keep_freed`]), else [`SPARE`].
/// Asked once a second.
/// Computes idle spare block retention limit from the 3-stage VRAM policy:
/// - Stage 1 (`Safe`): normal `base_spares` (e.g., `4` on <= 8 GiB, `48` on > 8 GiB).
/// - Stage 2 (`Alert`, 75%..85% VRAM): active collection keeping at most `1` spare block.
/// - Stage 3 (`Critical`, >= 85% VRAM or commit < 512 MiB): hard purge (`0` spares).
pub(crate) fn spare_limit_for_stage(stage: crate::residency::VramStage, base_spares: usize) -> usize {
    match stage {
        crate::residency::VramStage::Critical => 0,
        crate::residency::VramStage::Alert => base_spares.min(1),
        crate::residency::VramStage::Safe => base_spares,
    }
}

/// Computes host-RAM spare retention limit for `STAGING` and `SHARED_BUFFERS`:
/// forces `0` immediately whenever commit is critical, VRAM is in Stage 3,
/// or live free physical RAM drops below `1,500 MiB` (`low_phys_ram`).
pub(crate) fn host_ram_limit_for(
    stage: crate::residency::VramStage,
    pressure: bool,
    critical: bool,
    low_phys_ram: bool,
    limit: usize,
) -> usize {
    if critical || low_phys_ram {
        0
    } else if stage == crate::residency::VramStage::Alert || pressure {
        1
    } else {
        limit
    }
}

pub(crate) fn room(memory: Option<[u64; 4]>) {
    let pressure = crate::residency::pressured();
    let low_phys_ram = crate::host::low_physical_ram();
    let stage = crate::residency::update_vram(memory);
    let critical = crate::residency::critical_commit() || stage == crate::residency::VramStage::Critical;
    let alert = stage == crate::residency::VramStage::Alert;
    let base_spares = spares(memory, crate::host::keep_freed() && !pressure && !low_phys_ram);
    let limit = spare_limit_for_stage(stage, base_spares);
    SPARES.store(limit, Ordering::Relaxed);
    let tick = now();
    // A full pool can become idle while another subsystem needs the VRAM.
    // In Stage 3 (Critical), `limit == 0` performs an immediate hard purge of all empty blocks.
    if let Ok(mut pool) = TEXTURES.try_lock() {
        pool.reclaim(tick, limit, true);
    }
    let host_ram_limit = host_ram_limit_for(stage, pressure, critical, low_phys_ram, limit);
    HOST_RAM_SPARES.store(host_ram_limit, Ordering::Relaxed);
    if let Ok(mut pool) = STAGING.try_lock() {
        pool.reclaim(tick, host_ram_limit.min(STAGING_KEPT), true);
    }
    if let Ok(mut pool) = UAV_BUFFERS.try_lock() {
        let uav_limit = if critical {
            0
        } else if alert || pressure {
            1
        } else {
            limit.min(UAV_KEPT)
        };
        pool.reclaim(tick, uav_limit, true);
    }
    if let Ok(mut pool) = SHARED_BUFFERS.try_lock() {
        pool.reclaim(tick, host_ram_limit.min(SHARED_KEPT), true);
    }
}

/// Immediately drops all completely empty idle pool blocks across RAM and VRAM
/// pools when an allocation fails with `E_OUTOFMEMORY`. Pieces still waiting
/// in `resting` for in-flight GPU work are preserved safely.
pub(crate) fn emergency_reclaim() {
    let tick = now();
    SPARES.store(0, Ordering::Relaxed);
    HOST_RAM_SPARES.store(0, Ordering::Relaxed);
    if let Ok(mut pool) = STAGING.try_lock() {
        pool.reclaim(tick, 0, true);
    }
    if let Ok(mut pool) = SHARED_BUFFERS.try_lock() {
        pool.reclaim(tick, 0, true);
    }
    if let Ok(mut pool) = UAV_BUFFERS.try_lock() {
        pool.reclaim(tick, 0, true);
    }
    if let Ok(mut pool) = TEXTURES.try_lock() {
        pool.reclaim(tick, 0, true);
    }
}

fn spares(memory: Option<[u64; 4]>, keep: bool) -> usize {
    match memory {
        Some([budget, used, ..]) => {
            let effective = crate::residency::vram_ceiling_bytes(budget);
            let target = crate::residency::vram_soft_target_bytes(budget);
            if keep && target > 0 && used < target {
                if effective <= LOW_VRAM_BUDGET {
                    SPARE_KEPT_8GB
                } else {
                    SPARE_KEPT
                }
            } else {
                SPARE
            }
        }
        _ => SPARE,
    }
}

/// Milliseconds until a piece given back is used again. (It was three
/// frames, a tenth of a second while the title shows a loading screen at 30
/// a second, with up to 18 submissions in the air.)
const REST: u64 = 2500;

fn now() -> u64 {
    crate::host::now_ms()
}

pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_TEXTURE_POOL").is_none_or(|v| v != "0"))
}

/// Whether `uav_buffer` and `shared_buffer` suballocate from pre-mapped 64 MiB
/// heaps (`VICTORY_BUFFER_POOL=0`: each buffer is a committed allocation).
pub(crate) fn buffer_pool_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    on() && *ON.get_or_init(|| std::env::var_os("VICTORY_BUFFER_POOL").is_none_or(|v| v != "0"))
}

/// Empty blocks kept ready ahead for `UAV_BUFFERS` / `SHARED_BUFFERS` by the
/// background grower, clamped to 0 under pressure or high VRAM use.
/// Empty ahead blocks requested for `SHARED_BUFFERS` in host RAM: strictly `0`
/// whenever physical RAM drops below `1,500 MiB` or host memory is pressured.
fn shared_ahead_target_for(low_phys_ram: bool, pressured: bool, limited_host_ram: bool) -> usize {
    if low_phys_ram || pressured {
        0
    } else if limited_host_ram {
        1
    } else {
        2
    }
}

fn bg_ahead_for(base: usize, vram: Option<[u64; 4]>, pressure: bool, critical: bool, is_vram: bool) -> usize {
    if base == 0 || pressure || critical {
        return 0;
    }
    if is_vram && let Some([budget, used, ..]) = vram {
        let alert = crate::residency::vram_alert_target_bytes(budget);
        let critical_target = crate::residency::vram_soft_target_bytes(budget);
        if critical_target > 0 {
            let step = block_size_for(Some(budget), true, None);
            let next = used.saturating_add(step);
            if next >= critical_target {
                return 0;
            }
            if alert > 0 && next >= alert {
                return base.min(1);
            }
        }
    }
    base
}

/// How many blocks of textures are made ahead, so that one has been there
/// for a while when its first piece is taken (`VICTORY_RIPE_BLOCKS`, 12;
/// 0: none, and new blocks are used at once, as before).
///
/// A page fault of lists already on the card, in a texture made the same
/// tenth of a second as the first piece of a block made for it, lost the
/// device in runs n08 and n11 (2026-11-25): the title gives a slot that
/// those lists read by index a new texture. Why the card does not have the
/// new block's addresses yet is not known (n11 waited for `MakeResident`;
/// `indirect::tests::new_blocks_under_readers` never loses it). A block that
/// every submission begun before its making has left behind ([`Ripe`]) has
/// no such lists: pieces are cut from those first.
fn ahead() -> usize {
    static AHEAD: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *AHEAD.get_or_init(|| std::env::var("VICTORY_RIPE_BLOCKS").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(12))
}

/// Submissions begun and finished ([`crate::host::submissions`]), and how
/// many blocks to make ahead (0: every block is ripe at once).
#[derive(Clone, Copy)]
struct Ripe {
    started: u64,
    finished: u64,
    ahead: usize,
}

impl Ripe {
    const ALL: Ripe = Ripe { started: 0, finished: 0, ahead: 0 };

    fn now() -> Ripe {
        let (started, finished) = crate::host::submissions();
        // Not more than are kept ([`room`]): they would go and be made again.
        Ripe { started, finished, ahead: ahead().min(SPARES.load(Ordering::Relaxed)) }
    }

    fn is<B>(self, block: &Block<B>) -> bool {
        self.ahead == 0 || block.made <= self.finished
    }
}

/// Pieces cut from blocks that were not ripe, for want of a ripe one.
static UNRIPE: AtomicU64 = AtomicU64::new(0);

/// The free ranges of a block: start and length, never two that touch.
struct Ranges(BTreeMap<u64, u64>);

impl Ranges {
    fn new(size: u64) -> Ranges {
        Ranges(BTreeMap::from([(0, size)]))
    }

    /// The lowest place for `len` bytes at a multiple of `align`.
    fn take(&mut self, len: u64, align: u64) -> Option<u64> {
        let (start, size, at) = self.0.iter().find_map(|(&start, &size)| {
            let at = start.next_multiple_of(align);
            (at + len <= start + size).then_some((start, size, at))
        })?;
        self.0.remove(&start);
        if at > start {
            self.0.insert(start, at - start);
        }
        if at + len < start + size {
            self.0.insert(at + len, start + size - at - len);
        }
        Some(at)
    }

    fn give(&mut self, mut start: u64, mut len: u64) {
        if let Some((&before, &size)) = self.0.range(..start).next_back()
            && before + size == start
        {
            self.0.remove(&before);
            (start, len) = (before, len + size);
        }
        if let Some(size) = self.0.remove(&(start + len)) {
            len += size;
        }
        self.0.insert(start, len);
    }

    fn whole(&self, size: u64) -> bool {
        self.0.get(&0) == Some(&size)
    }
}

/// A part of a block.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Piece {
    block: usize,
    start: u64,
    len: u64,
}

struct Block<B> {
    memory: B,
    free: Ranges,
    /// The end of the highest piece ever taken: above it the memory is as
    /// it was made (zeros).
    used_to: u64,
    /// The newest submission begun when it was made ([`Ripe`]).
    made: u64,
    /// When that was (ms).
    #[cfg_attr(not(feature = "diagnostics"), allow(dead_code))]
    since: u64,
}

enum TryTake<B> {
    Found { piece: Piece, memory: B, again: bool, need_more: bool },
    Empty { can_grow: bool },
    TooLarge,
}

struct Pool<B> {
    /// `None`: a block let go, its place to be used again.
    blocks: Vec<Option<Block<B>>>,
    /// Pieces given back, and when each may be used again.
    resting: VecDeque<(u64, Piece)>,
    /// Size of each block in this pool (bytes).
    block: u64,
    /// The empty blocks it was last told to keep, and the most it keeps.
    spare: usize,
    most: usize,
    host_ram: bool,
    retry_growth_at: u64,
}

impl<B: Clone> Pool<B> {
    const fn new(most: usize) -> Pool<B> {
        Pool { blocks: Vec::new(), resting: VecDeque::new(), block: BLOCK, spare: 0, most, host_ram: false, retry_growth_at: 0 }
    }

    const fn new_host_ram(most: usize) -> Pool<B> {
        Pool { blocks: Vec::new(), resting: VecDeque::new(), block: BLOCK, spare: 0, most, host_ram: true, retry_growth_at: 0 }
    }

    /// Sets this pool's block size before its first block is allocated.
    fn ensure_block(&mut self, block: u64) -> u64 {
        if self.blocks.is_empty() && block > 0 {
            self.block = block;
        }
        self.block
    }

    /// A piece of `len` bytes and the memory it lies in, and whether that
    /// part was ever used before; `make` makes a block when none has room.
    #[cfg(test)]
    fn take(&mut self, len: u64, align: u64, now: u64, make: impl FnMut() -> Option<B>) -> Option<(Piece, B, bool)> {
        self.take_ripe(len, align, now, Ripe::ALL, make)
    }

    /// The same from a block that is ripe, if one has room; `make` makes one
    /// ahead too, when few are empty.
    #[cfg(test)]
    fn take_ripe(
        &mut self,
        len: u64,
        align: u64,
        now: u64,
        ripe: Ripe,
        mut make: impl FnMut() -> Option<B>,
    ) -> Option<(Piece, B, bool)> {
        // With blocks made ahead, what would be an allocation of its own (and
        // new when the card first reads it) lies in one of those too.
        let cap = if ripe.ahead > 0 { self.block } else { self.block / 2 };
        if len == 0 || len > cap {
            return None;
        }
        self.rested(now);
        let mut found = None;
        for (i, block) in self.blocks.iter_mut().enumerate() {
            if let Some(block) = block
                && ripe.is(block)
                && let Some(start) = block.free.take(len, align)
            {
                found = Some((i, start));
                break;
            }
        }
        if found.is_none() && ripe.ahead > 0 {
            // The one that has been there the longest.
            let mut unripe: Vec<(u64, usize)> =
                self.blocks.iter().enumerate().filter_map(|(i, b)| Some((b.as_ref()?.made, i))).collect();
            unripe.sort_unstable();
            found = unripe.into_iter().find_map(|(_, i)| Some((i, self.blocks[i].as_mut()?.free.take(len, align)?)));
        }
        // A failed 64 MiB block must not trigger another driver call for each
        // small texture. Existing free ranges remain usable during backoff.
        let (i, start) = match found {
            Some(found) => found,
            None => {
                let memory = self.grow(now, &mut make)?;
                let i = self.add(memory, ripe.started);
                (i, self.blocks[i].as_mut()?.free.take(len, align)?)
            }
        };
        if ripe.ahead > 0 {
            if !ripe.is(self.blocks[i].as_ref()?) {
                let n = UNRIPE.fetch_add(1, Ordering::Relaxed) + 1;
                if n.is_power_of_two() {
                    tracing::info!("textures cut from blocks made while lists ran that still run: {n}");
                }
            }
            let empty = self.blocks.iter().flatten().filter(|b| b.free.whole(self.block)).count();
            if empty < ripe.ahead
                && let Some(memory) = self.grow(now, &mut make)
            {
                self.add(memory, ripe.started);
            }
        }
        let block = self.blocks[i].as_mut()?;
        let again = start < block.used_to;
        block.used_to = block.used_to.max(start + len);
        Some((Piece { block: i, start, len }, block.memory.clone(), again))
    }

    #[cfg(test)]
    fn grow(&mut self, now: u64, make: &mut impl FnMut() -> Option<B>) -> Option<B> {
        if now < self.retry_growth_at {
            return None;
        }
        let block = make();
        if block.is_none() {
            self.retry_growth_at = now.saturating_add(500);
        }
        block
    }

    fn empty_blocks(&self) -> usize {
        self.blocks.iter().flatten().filter(|b| b.free.whole(self.block)).count()
    }

    fn can_grow(&self, now: u64) -> bool {
        now >= self.retry_growth_at
    }

    fn note_growth_failed(&mut self, now: u64) {
        self.retry_growth_at = now.saturating_add(500);
    }

    /// Lock-friendly allocation: takes a piece from an existing block without
    /// ever invoking a driver allocation while the pool mutex is held. Returns
    /// whether the background grower should top up empty blocks, or whether the
    /// caller must grow a block outside the lock as a fallback.
    fn try_take_ripe(&mut self, len: u64, align: u64, now: u64, ripe: Ripe) -> TryTake<B> {
        let cap = if ripe.ahead > 0 { self.block } else { self.block / 2 };
        if len == 0 || len > cap {
            return TryTake::TooLarge;
        }
        self.rested(now);
        let mut found = None;
        for (i, block) in self.blocks.iter_mut().enumerate() {
            if let Some(block) = block
                && ripe.is(block)
                && let Some(start) = block.free.take(len, align)
            {
                found = Some((i, start));
                break;
            }
        }
        if found.is_none() && ripe.ahead > 0 {
            let mut unripe: Vec<(u64, usize)> =
                self.blocks.iter().enumerate().filter_map(|(i, b)| Some((b.as_ref()?.made, i))).collect();
            unripe.sort_unstable();
            found = unripe.into_iter().find_map(|(_, i)| Some((i, self.blocks[i].as_mut()?.free.take(len, align)?)));
        }
        let Some((i, start)) = found else {
            return TryTake::Empty { can_grow: self.can_grow(now) };
        };
        if ripe.ahead > 0 && !ripe.is(self.blocks[i].as_ref().unwrap()) {
            let n = UNRIPE.fetch_add(1, Ordering::Relaxed) + 1;
            if n.is_power_of_two() {
                tracing::info!("textures cut from blocks made while lists ran that still run: {n}");
            }
        }
        let need_more = ripe.ahead > 0 && self.empty_blocks() < ripe.ahead && self.can_grow(now);
        let block = self.blocks[i].as_mut().unwrap();
        let again = start < block.used_to;
        block.used_to = block.used_to.max(start + len);
        TryTake::Found {
            piece: Piece { block: i, start, len },
            memory: block.memory.clone(),
            again,
            need_more,
        }
    }

    /// Inserts `memory` as a new block and immediately cuts `(len, align)` from
    /// it (used after a fallback allocation completed outside the pool lock).
    fn add_and_take(&mut self, memory: B, made: u64, len: u64, align: u64, ahead: usize) -> Option<(Piece, B, bool, bool)> {
        let i = self.add(memory, made);
        let start = self.blocks[i].as_mut()?.free.take(len, align)?;
        let need_more = ahead > 0 && self.empty_blocks() < ahead;
        let block = self.blocks[i].as_mut()?;
        let again = start < block.used_to;
        block.used_to = block.used_to.max(start + len);
        Some((Piece { block: i, start, len }, block.memory.clone(), again, need_more))
    }

    fn add(&mut self, memory: B, made: u64) -> usize {
        let i = match self.blocks.iter().position(Option::is_none) {
            Some(i) => i,
            None => {
                self.blocks.push(None);
                self.blocks.len() - 1
            }
        };
        self.blocks[i] = Some(Block { memory, free: Ranges::new(self.block), used_to: 0, made, since: crate::host::now_ms() });
        i
    }

    fn give(&mut self, piece: Piece, now: u64) {
        self.resting.push_back((now + REST, piece));
    }

    /// Makes the pieces that rested long enough free, and lets go of empty
    /// blocks past [`SPARES`] (VRAM pools) or [`HOST_RAM_SPARES`] (host-RAM pools).
    fn rested(&mut self, now: u64) {
        let limit = if self.host_ram {
            HOST_RAM_SPARES.load(Ordering::Relaxed)
        } else {
            SPARES.load(Ordering::Relaxed)
        };
        self.reclaim(now, limit, false);
    }

    fn reclaim(&mut self, now: u64, limit: usize, sweep: bool) {
        let mut any = false;
        while let Some(&(ready, piece)) = self.resting.front()
            && ready <= now
        {
            self.resting.pop_front();
            if let Some(block) = self.blocks[piece.block].as_mut() {
                block.free.give(piece.start, piece.len);
                any |= block.free.whole(self.block);
            }
        }
        // (Without a piece given back too, once there is less room.)
        let most = limit.min(self.most);
        let previous = std::mem::replace(&mut self.spare, most);
        if sweep || any || most < previous {
            let mut spare = 0;
            for block in &mut self.blocks {
                if block.as_ref().is_some_and(|b| b.free.whole(self.block)) {
                    spare += 1;
                    if spare > most {
                        *block = None;
                    }
                }
            }
        }
    }

    fn blocks(&self) -> usize {
        self.blocks.iter().flatten().count()
    }
}

/// A mapped upload buffer's memory.
#[derive(Clone)]
struct Upload(ID3D12Resource, *mut u8);
// SAFETY: the mapping is written by the owners of its pieces, each its own.
unsafe impl Send for Upload {}

static TEXTURES: Mutex<Pool<ID3D12Heap>> = Mutex::new(Pool::new(SPARE_KEPT));
static STAGING: Mutex<Pool<Upload>> = Mutex::new(Pool::new_host_ram(STAGING_KEPT));
static UAV_BUFFERS: Mutex<Pool<ID3D12Heap>> = Mutex::new(Pool::new(UAV_KEPT));
static SHARED_BUFFERS: Mutex<Pool<ID3D12Heap>> = Mutex::new(Pool::new_host_ram(SHARED_KEPT));

fn lock<T>(m: &'static Mutex<T>) -> std::sync::MutexGuard<'static, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Bytes of the pieces held now per pool.
static HELD: AtomicU64 = AtomicU64::new(0);
static UAV_HELD: AtomicU64 = AtomicU64::new(0);
static SHARED_HELD: AtomicU64 = AtomicU64::new(0);

/// Pool telemetry counters:
/// `[0]` tex bg blocks, `[1]` tex sync fallback blocks,
/// `[2]` uav hits, `[3]` uav bg blocks, `[4]` uav sync fallback blocks,
/// `[5]` shared hits, `[6]` shared bg blocks, `[7]` shared sync fallback blocks.
static POOL_STATS: [AtomicU64; 8] = [const { AtomicU64::new(0) }; 8];

#[derive(Clone, Copy, Default)]
pub(crate) struct PoolStats {
    pub(crate) tex_blocks: usize,
    pub(crate) tex_bg: u64,
    pub(crate) tex_sync: u64,
    pub(crate) uav_hits: u64,
    pub(crate) uav_blocks: usize,
    pub(crate) uav_bg: u64,
    pub(crate) uav_sync: u64,
    pub(crate) shared_hits: u64,
    pub(crate) shared_blocks: usize,
    pub(crate) shared_bg: u64,
    pub(crate) shared_sync: u64,
    pub(crate) staging_blocks: usize,
}

pub(crate) fn stats() -> PoolStats {
    PoolStats {
        tex_blocks: TEXTURES.try_lock().map_or(0, |p| p.blocks()),
        tex_bg: POOL_STATS[0].load(Ordering::Relaxed),
        tex_sync: POOL_STATS[1].load(Ordering::Relaxed),
        uav_hits: POOL_STATS[2].load(Ordering::Relaxed),
        uav_blocks: UAV_BUFFERS.try_lock().map_or(0, |p| p.blocks()),
        uav_bg: POOL_STATS[3].load(Ordering::Relaxed),
        uav_sync: POOL_STATS[4].load(Ordering::Relaxed),
        shared_hits: POOL_STATS[5].load(Ordering::Relaxed),
        shared_blocks: SHARED_BUFFERS.try_lock().map_or(0, |p| p.blocks()),
        shared_bg: POOL_STATS[6].load(Ordering::Relaxed),
        shared_sync: POOL_STATS[7].load(Ordering::Relaxed),
        staging_blocks: STAGING.try_lock().map_or(0, |p| p.blocks()),
    }
}

struct GrowerState {
    device: Option<ID3D12Device>,
    pending: bool,
}
// SAFETY: `ID3D12Device` is free-threaded.
unsafe impl Send for GrowerState {}

struct Grower {
    state: Mutex<GrowerState>,
    wake: Condvar,
}

static GROWER: OnceLock<Grower> = OnceLock::new();
static GROWER_STARTED: std::sync::Once = std::sync::Once::new();

fn wake_grower(device: &ID3D12Device) {
    let grower = GROWER.get_or_init(|| Grower {
        state: Mutex::new(GrowerState { device: Some(device.clone()), pending: true }),
        wake: Condvar::new(),
    });
    GROWER_STARTED.call_once(|| {
        let _ = std::thread::Builder::new()
            .name("victory-pool-grow".to_owned())
            .spawn(grower_loop);
    });
    let mut s = grower.state.lock().unwrap_or_else(|e| e.into_inner());
    if s.device.is_none() {
        s.device = Some(device.clone());
    }
    s.pending = true;
    grower.wake.notify_one();
}

fn make_texture_block(device: &ID3D12Device, size: u64) -> Option<ID3D12Heap> {
    let desc = D3D12_HEAP_DESC {
        SizeInBytes: size,
        Properties: D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() },
        Alignment: 0,
        Flags: D3D12_HEAP_FLAG_ALLOW_ONLY_NON_RT_DS_TEXTURES | crate::host::not_resident(),
    };
    let mut heap: Option<ID3D12Heap> = None;
    // SAFETY: plain heap creation on live device.
    if let Err(e) = unsafe { device.CreateHeap(&desc, &mut heap) } {
        crate::residency::allocation_error(&e);
        tracing::warn!(error = %e, "no block of texture memory");
        return None;
    }
    let heap = heap?;
    if let Err(e) = crate::host::resident_and_mapped_sync(device, &heap) {
        crate::residency::allocation_error(&e);
        tracing::warn!(error = %e, "a block of texture memory is not resident");
        return None;
    }
    Some(heap)
}

fn make_uav_block(device: &ID3D12Device, size: u64) -> Option<ID3D12Heap> {
    let desc = D3D12_HEAP_DESC {
        SizeInBytes: size,
        Properties: D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() },
        Alignment: 0,
        Flags: D3D12_HEAP_FLAG_ALLOW_ONLY_BUFFERS | crate::host::not_resident(),
    };
    let mut heap: Option<ID3D12Heap> = None;
    // SAFETY: plain heap creation on live device.
    if let Err(e) = unsafe { device.CreateHeap(&desc, &mut heap) } {
        crate::residency::allocation_error(&e);
        tracing::warn!(error = %e, "no block of GPU buffer memory");
        return None;
    }
    let heap = heap?;
    if let Err(e) = crate::host::resident_and_mapped_sync(device, &heap) {
        crate::residency::allocation_error(&e);
        tracing::warn!(error = %e, "a block of GPU buffer memory is not resident");
        return None;
    }
    Some(heap)
}

fn make_shared_block(device: &ID3D12Device, size: u64) -> Option<ID3D12Heap> {
    let desc = D3D12_HEAP_DESC {
        SizeInBytes: size,
        Properties: D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_CUSTOM,
            CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_WRITE_BACK,
            MemoryPoolPreference: D3D12_MEMORY_POOL_L0,
            ..Default::default()
        },
        Alignment: 0,
        Flags: D3D12_HEAP_FLAG_ALLOW_ONLY_BUFFERS | crate::host::not_resident(),
    };
    let mut heap: Option<ID3D12Heap> = None;
    // SAFETY: plain heap creation on live device.
    if let Err(e) = unsafe { device.CreateHeap(&desc, &mut heap) } {
        crate::residency::allocation_error(&e);
        tracing::warn!(error = %e, "no block of shared buffer memory");
        return None;
    }
    let heap = heap?;
    if let Err(e) = crate::host::resident_and_mapped_sync(device, &heap) {
        crate::residency::allocation_error(&e);
        tracing::warn!(error = %e, "a block of shared buffer memory is not resident");
        return None;
    }
    Some(heap)
}

fn grower_loop() {
    // SAFETY: plain priority adjustment on our own helper thread.
    unsafe {
        use windows::Win32::System::Threading::*;
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
    }
    crate::cores::keep_background_on_aux_cores("victory-pool-grow");
    let Some(grower) = GROWER.get() else { return };
    loop {
        let device = {
            let mut s = grower.state.lock().unwrap_or_else(|e| e.into_inner());
            while !s.pending || s.device.is_none() {
                s = grower.wake.wait(s).unwrap_or_else(|e| e.into_inner());
            }
            s.pending = false;
            s.device.clone().unwrap()
        };
        // Round-robin 1 block per pool per pass so textures, UAV buffers and
        // shared buffers all warm up concurrently without starving each other.
        loop {
            let pressure = crate::residency::pressured();
            let low_phys_ram = crate::host::low_physical_ram();
            let vram_stage = crate::residency::vram_stage();
            let vram_pressure = vram_stage >= crate::residency::VramStage::Alert;
            let critical = crate::residency::critical_commit()
                || low_phys_ram
                || vram_stage == crate::residency::VramStage::Critical;
            let vram = crate::host::video_memory_ready();
            let mut made_any = false;
            let mut backoff_waiting = false;

            // 1. Texture pool ahead blocks.
            let tex_target = bg_ahead_for(
                ahead().min(SPARES.load(Ordering::Relaxed)),
                vram,
                pressure || vram_pressure,
                critical,
                true,
            );
            let tex_block = texture_block_bytes();
            let tex_need = {
                let tick = now();
                let mut pool = lock(&TEXTURES);
                let _ = pool.ensure_block(tex_block);
                if tex_target > 0 && pool.empty_blocks() < tex_target {
                    if pool.can_grow(tick) {
                        true
                    } else {
                        backoff_waiting = true;
                        false
                    }
                } else {
                    false
                }
            };
            if tex_need {
                let (started, _) = crate::host::submissions();
                let block_sz = lock(&TEXTURES).ensure_block(tex_block);
                if let Some(heap) = make_texture_block(&device, block_sz) {
                    lock(&TEXTURES).add(heap, started);
                    POOL_STATS[0].fetch_add(1, Ordering::Relaxed);
                    made_any = true;
                } else {
                    lock(&TEXTURES).note_growth_failed(now());
                    backoff_waiting = true;
                }
            }

            if buffer_pool_on() {
                let buf_block = buffer_block_bytes();
                // 2. GPU UAV buffer pool ahead blocks (1-2 compact blocks).
                let uav_target = bg_ahead_for(2, vram, pressure || vram_pressure, critical, true);
                let uav_need = {
                    let tick = now();
                    let mut pool = lock(&UAV_BUFFERS);
                    let _ = pool.ensure_block(buf_block);
                    if uav_target > 0 && pool.empty_blocks() < uav_target {
                        if pool.can_grow(tick) {
                            true
                        } else {
                            backoff_waiting = true;
                            false
                        }
                    } else {
                        false
                    }
                };
                if uav_need {
                    let (started, _) = crate::host::submissions();
                    let block_sz = lock(&UAV_BUFFERS).ensure_block(buf_block);
                    if let Some(heap) = make_uav_block(&device, block_sz) {
                        lock(&UAV_BUFFERS).add(heap, started);
                        POOL_STATS[3].fetch_add(1, Ordering::Relaxed);
                        made_any = true;
                    } else {
                        lock(&UAV_BUFFERS).note_growth_failed(now());
                        backoff_waiting = true;
                    }
                }

                // 3. Shared L0 WRITE_BACK buffer pool ahead blocks (1-2 compact blocks; 0 under < 1,500 MiB RAM).
                let base_shared = shared_ahead_target_for(low_phys_ram, pressure, crate::residency::limited_host_ram());
                let shared_target = bg_ahead_for(base_shared, vram, pressure, critical, false);
                let shared_need = {
                    let tick = now();
                    let mut pool = lock(&SHARED_BUFFERS);
                    let _ = pool.ensure_block(buf_block);
                    if shared_target > 0 && pool.empty_blocks() < shared_target {
                        if pool.can_grow(tick) {
                            true
                        } else {
                            backoff_waiting = true;
                            false
                        }
                    } else {
                        false
                    }
                };
                if shared_need {
                    let (started, _) = crate::host::submissions();
                    let block_sz = lock(&SHARED_BUFFERS).ensure_block(buf_block);
                    if let Some(heap) = make_shared_block(&device, block_sz) {
                        lock(&SHARED_BUFFERS).add(heap, started);
                        POOL_STATS[6].fetch_add(1, Ordering::Relaxed);
                        made_any = true;
                    } else {
                        lock(&SHARED_BUFFERS).note_growth_failed(now());
                        backoff_waiting = true;
                    }
                }
            }

            if !made_any {
                if backoff_waiting {
                    let s = grower.state.lock().unwrap_or_else(|e| e.into_inner());
                    let _ = grower.wake.wait_timeout(s, Duration::from_millis(500));
                }
                break;
            }
        }
    }
}

/// Bytes of the texture blocks no texture holds: the card's memory in use
/// by us that new textures take first.
pub(crate) fn texture_room() -> u64 {
    let pool = lock(&TEXTURES);
    (pool.blocks() as u64 * pool.block).saturating_sub(HELD.load(Ordering::Relaxed))
}

/// For the memory line: blocks of textures and of staging buffers, in MiB.
#[cfg(feature = "diagnostics")]
pub(crate) fn memory() -> [u64; 2] {
    let t = lock(&TEXTURES);
    let s = lock(&STAGING);
    [t.blocks() as u64 * (t.block >> 20), s.blocks() as u64 * (s.block >> 20)]
}

/// Part of a block of mapped upload memory, back in the pool when dropped.
pub(crate) struct StagingPiece(Piece);

impl Drop for StagingPiece {
    fn drop(&mut self) {
        lock(&STAGING).give(self.0, now());
    }
}

/// `bytes` of upload memory, all zeros: the buffer it lies in, where in
/// that buffer, and its mapping. `None`: not pooled.
pub(crate) fn staging(host: &crate::host::Host, bytes: u64) -> Option<(StagingPiece, ID3D12Resource, u64, *mut u8)> {
    if !on() {
        return None;
    }
    // Copies from a buffer begin at multiples of 512.
    let bytes = bytes.next_multiple_of(512);
    let tick = now();
    let buf_block = buffer_block_bytes();
    let (block_sz, taken) = {
        let mut pool = lock(&STAGING);
        let block_sz = pool.ensure_block(buf_block);
        let taken = pool.try_take_ripe(bytes, 512, tick, Ripe::ALL);
        (block_sz, taken)
    };
    let (piece, memory, again) = match taken {
        TryTake::Found { piece, memory, again, .. } => (piece, memory, again),
        TryTake::TooLarge => return None,
        TryTake::Empty { can_grow: false } => return None,
        TryTake::Empty { can_grow: true } => {
            // Only gate speculative bindless prefetch on staging-pool growth
            // headroom; table-bound textures and decoration bakes are sampled
            // immediately and would otherwise read uninitialized/reused VRAM.
            let speculative = crate::texture::PICKING.get().is_some() && !crate::texture::BAKING.get();
            if speculative && !crate::residency::allow_staging_growth(false) {
                lock(&STAGING).note_growth_failed(tick);
                return None;
            }
            // Allocate outside `STAGING`'s lock so drops/reclaims never stall.
            let Some((buffer, data)) = host.upload_buffer(block_sz) else {
                lock(&STAGING).note_growth_failed(now());
                return None;
            };
            crate::host::name(&buffer, || "staging of textures (a block)".into());
            let (piece, memory, again, _) = lock(&STAGING).add_and_take(Upload(buffer, data), 0, bytes, 512, 0)?;
            (piece, memory, again)
        }
    };
    let owned = StagingPiece(piece);
    // SAFETY: the piece lies in the block's mapping.
    let data = unsafe { memory.1.add(piece.start as usize) };
    if again {
        // What is not detiled into it (an unmapped texture) uploads zeros,
        // as from a buffer of its own.
        // SAFETY: the piece is ours alone.
        unsafe { std::ptr::write_bytes(data, 0, bytes as usize) };
    }
    Some((owned, memory.0, piece.start, data))
}

const POOL_TEXTURES: u8 = 0;
const POOL_UAV: u8 = 1;
const POOL_SHARED: u8 = 2;

/// An object a pooled resource holds and releases when it goes: its piece
/// is free then.
#[repr(C)]
struct Reclaim {
    methods: &'static [*const (); 3],
    references: AtomicU32,
    piece: Piece,
    pool: u8,
}

struct Methods([*const (); 3]);
// SAFETY: function addresses only.
unsafe impl Sync for Methods {}

static RECLAIM: Methods = Methods([query as *const (), add_ref as *const (), release as *const ()]);

unsafe extern "system" fn query(this: *mut c_void, iid: *const GUID, out: *mut *mut c_void) -> HRESULT {
    const E_NOINTERFACE: HRESULT = HRESULT(0x8000_4002_u32 as i32);
    // SAFETY: COM's arguments.
    unsafe {
        if out.is_null() {
            return HRESULT(0x8000_4003_u32 as i32);
        }
        if iid.as_ref().is_some_and(|iid| *iid == IUnknown::IID) {
            add_ref(this);
            *out = this;
            return HRESULT(0);
        }
        *out = std::ptr::null_mut();
    }
    E_NOINTERFACE
}

unsafe extern "system" fn add_ref(this: *mut c_void) -> u32 {
    // SAFETY: one of ours, alive while a reference to it is.
    unsafe { (*this.cast::<Reclaim>()).references.fetch_add(1, Ordering::Relaxed) + 1 }
}

unsafe extern "system" fn release(this: *mut c_void) -> u32 {
    // SAFETY: as above; the last reference frees the box it was made as.
    unsafe {
        let left = (*this.cast::<Reclaim>()).references.fetch_sub(1, Ordering::AcqRel) - 1;
        if left == 0 {
            let gone = Box::from_raw(this.cast::<Reclaim>());
            match gone.pool {
                POOL_TEXTURES => {
                    HELD.fetch_sub(gone.piece.len, Ordering::Relaxed);
                    lock(&TEXTURES).give(gone.piece, now());
                }
                POOL_UAV => {
                    UAV_HELD.fetch_sub(gone.piece.len, Ordering::Relaxed);
                    lock(&UAV_BUFFERS).give(gone.piece, now());
                }
                POOL_SHARED => {
                    SHARED_HELD.fetch_sub(gone.piece.len, Ordering::Relaxed);
                    lock(&SHARED_BUFFERS).give(gone.piece, now());
                }
                _ => {}
            }
        }
        left
    }
}

/// Textures made in the last second ([`fresh`]), and when.
static RECENT: Mutex<VecDeque<(usize, u64)>> = Mutex::new(VecDeque::new());

/// Whether `texture` was made in a block in the last second.
pub(crate) fn fresh(texture: &ID3D12Resource) -> bool {
    let raw = texture.as_raw() as usize;
    let since = now().saturating_sub(1000);
    RECENT.lock().unwrap_or_else(|e| e.into_inner()).iter().rev().take_while(|r| r.1 >= since).any(|r| r.0 == raw)
}

fn made_now(texture: &ID3D12Resource) {
    let now = now();
    let mut recent = RECENT.lock().unwrap_or_else(|e| e.into_inner());
    while recent.front().is_some_and(|r| r.1 + 1000 < now) {
        recent.pop_front();
    }
    recent.push_back((texture.as_raw() as usize, now));
}

/// Makes `resource` give `piece` back to `pool` when its last reference goes.
fn give_back_with(resource: &ID3D12Resource, piece: Piece, pool: u8) {
    const KEY: GUID = GUID::from_u128(0x6f0e2f0c_5a59_4b1e_9a3f_7f4d2c9be713);
    let made = Box::into_raw(Box::new(Reclaim { methods: &RECLAIM.0, references: AtomicU32::new(1), piece, pool }));
    // SAFETY: a COM object of ours with one reference, which this takes and
    // drops at the end; the texture takes its own.
    unsafe {
        let object = IUnknown::from_raw(made.cast());
        if resource.SetPrivateDataInterface(&KEY, &object).is_err() {
            // Never given back: the piece stays taken, nothing is reused
            // under a texture that may be alive.
            std::mem::forget(object);
        }
    }
}

/// A texture of `desc` (no target, depth or unordered access) in a block;
/// `None`: not pooled, or the host refused.
///
/// # Safety
/// `desc` is a valid description.
pub(crate) unsafe fn texture(
    device: &ID3D12Device,
    desc: *const D3D12_RESOURCE_DESC,
    state: D3D12_RESOURCE_STATES,
) -> Option<ID3D12Resource> {
    if !on() {
        return None;
    }
    // Small textures lie at multiples of 4 KiB where the host allows it
    // (the top mip is 64 KiB at most): 64 KiB each otherwise.
    // SAFETY: the caller's description.
    let mut d = unsafe { *desc };
    d.Alignment = u64::from(D3D12_SMALL_RESOURCE_PLACEMENT_ALIGNMENT);
    // SAFETY: plain queries.
    let mut info = unsafe { device.GetResourceAllocationInfo(0, &[d]) };
    if info.SizeInBytes == u64::MAX || info.Alignment != d.Alignment {
        d.Alignment = 0;
        info = unsafe { device.GetResourceAllocationInfo(0, &[d]) };
    }
    if info.SizeInBytes == u64::MAX {
        return None;
    }
    let align = info.Alignment.max(1);
    let tick = now();
    let ripe = Ripe::now();
    let tex_block = texture_block_bytes();
    let (block_sz, taken) = {
        let mut pool = lock(&TEXTURES);
        let block_sz = pool.ensure_block(tex_block);
        let taken = pool.try_take_ripe(info.SizeInBytes, align, tick, ripe);
        (block_sz, taken)
    };
    #[cfg_attr(not(feature = "diagnostics"), allow(unused_variables))]
    let (piece, heap, again, need_more) = match taken {
        TryTake::Found { piece, memory, again, need_more } => (piece, memory, again, need_more),
        TryTake::TooLarge => return None,
        TryTake::Empty { can_grow: false } => return None,
        TryTake::Empty { can_grow: true } => {
            // Fallback only when background blocks ran dry: allocate outside
            // `TEXTURES`'s lock so drops and concurrent lookups never stall.
            let Some(heap) = make_texture_block(device, block_sz) else {
                lock(&TEXTURES).note_growth_failed(now());
                return None;
            };
            POOL_STATS[1].fetch_add(1, Ordering::Relaxed);
            lock(&TEXTURES).add_and_take(heap, ripe.started, info.SizeInBytes, align, ripe.ahead)?
        }
    };
    if need_more {
        wake_grower(device);
    }
    let mut made: Option<ID3D12Resource> = None;
    // SAFETY: the piece is no one else's; the heap lives while a piece of
    // it is taken.
    let result = unsafe { device.CreatePlacedResource(&heap, piece.start, &d, state, None, &mut made) };
    match (result, made) {
        (Ok(()), Some(made)) => {
            HELD.fetch_add(piece.len, Ordering::Relaxed);
            give_back_with(&made, piece, POOL_TEXTURES);
            made_now(&made);
            #[cfg(feature = "diagnostics")]
            {
                crate::book::made(&made, || "a texture in a block".into());
                crate::book::block(&heap, &made, piece.start, block_sz);
                crate::book::note(made.as_raw() as usize, || {
                    let block = lock(&TEXTURES).blocks.get(piece.block).and_then(Option::as_ref).map(|b| (b.made, b.since));
                    let (submission, since) = block.unwrap_or_default();
                    format!(
                        "; piece {:#x}+{:#x} of block {} {:#x} made {:.1} s before it at submission {submission}, used before: {again}, made at submissions {:?}",
                        piece.start,
                        piece.len,
                        piece.block,
                        heap.as_raw() as usize,
                        now().saturating_sub(since) as f64 / 1e3,
                        crate::host::submissions()
                    )
                });
            }
            Some(made)
        }
        _ => {
            lock(&TEXTURES).give(piece, now());
            None
        }
    }
}

/// A device-local unordered-access buffer placed in a pre-mapped 64 MiB heap
/// (`D3D12_HEAP_FLAG_ALLOW_ONLY_BUFFERS`). `None` if unpooled, larger than
/// `BLOCK / 2`, or the host refused.
pub(crate) fn uav_buffer(device: &ID3D12Device, size: u64) -> Option<ID3D12Resource> {
    if !buffer_pool_on() {
        return None;
    }
    let width = size.max(1);
    let align = u64::from(D3D12_DEFAULT_RESOURCE_PLACEMENT_ALIGNMENT);
    let alloc = width.next_multiple_of(align);
    let buf_block = buffer_block_bytes();
    let (block_sz, taken, started) = {
        let (started, finished) = crate::host::submissions();
        let ripe = Ripe { started, finished, ahead: 2 };
        let tick = now();
        let mut pool = lock(&UAV_BUFFERS);
        let block_sz = pool.ensure_block(buf_block);
        if alloc > block_sz / 2 {
            return None;
        }
        let taken = pool.try_take_ripe(alloc, align, tick, ripe);
        (block_sz, taken, started)
    };
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Alignment: align,
        Width: width,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
        ..Default::default()
    };
    let (piece, heap, _again, need_more) = match taken {
        TryTake::Found { piece, memory, again, need_more } => (piece, memory, again, need_more),
        TryTake::TooLarge => return None,
        TryTake::Empty { can_grow: false } => return None,
        TryTake::Empty { can_grow: true } => {
            let Some(heap) = make_uav_block(device, block_sz) else {
                lock(&UAV_BUFFERS).note_growth_failed(now());
                return None;
            };
            POOL_STATS[4].fetch_add(1, Ordering::Relaxed);
            lock(&UAV_BUFFERS).add_and_take(heap, started, alloc, align, 2)?
        }
    };
    if need_more {
        wake_grower(device);
    }
    let mut made: Option<ID3D12Resource> = None;
    // SAFETY: `piece` is exclusively owned, `heap` is already resident and
    // mapped in GPU page tables, and `heap` outlives `made` via `Reclaim`.
    let result = unsafe {
        device.CreatePlacedResource(&heap, piece.start, &desc, D3D12_RESOURCE_STATE_COMMON, None, &mut made)
    };
    match (result, made) {
        (Ok(()), Some(made)) => {
            UAV_HELD.fetch_add(piece.len, Ordering::Relaxed);
            POOL_STATS[2].fetch_add(1, Ordering::Relaxed);
            give_back_with(&made, piece, POOL_UAV);
            Some(made)
        }
        _ => {
            lock(&UAV_BUFFERS).give(piece, now());
            None
        }
    }
}

/// A CPU-mapped write-back (`L0`) unordered-access buffer placed in a
/// pre-mapped 64 MiB custom heap (`D3D12_HEAP_FLAG_ALLOW_ONLY_BUFFERS`), zeroed
/// on piece reuse. `None` if unpooled, larger than `BLOCK / 2`, or refused.
pub(crate) fn shared_buffer(device: &ID3D12Device, size: u64) -> Option<(ID3D12Resource, *mut u8)> {
    if !buffer_pool_on() {
        return None;
    }
    let width = size.max(1);
    let align = u64::from(D3D12_DEFAULT_RESOURCE_PLACEMENT_ALIGNMENT);
    let alloc = width.next_multiple_of(align);
    let ahead_target = shared_ahead_target_for(
        crate::host::low_physical_ram(),
        crate::residency::pressured(),
        crate::residency::limited_host_ram(),
    );
    let buf_block = buffer_block_bytes();
    let (block_sz, taken, started) = {
        let (started, finished) = crate::host::submissions();
        let ripe = Ripe { started, finished, ahead: ahead_target };
        let tick = now();
        let mut pool = lock(&SHARED_BUFFERS);
        let block_sz = pool.ensure_block(buf_block);
        if alloc > block_sz / 2 {
            return None;
        }
        let taken = pool.try_take_ripe(alloc, align, tick, ripe);
        (block_sz, taken, started)
    };
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Alignment: align,
        Width: width,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
        ..Default::default()
    };
    let (piece, heap, again, need_more) = match taken {
        TryTake::Found { piece, memory, again, need_more } => (piece, memory, again, need_more),
        TryTake::TooLarge => return None,
        TryTake::Empty { can_grow: false } => return None,
        TryTake::Empty { can_grow: true } => {
            let Some(heap) = make_shared_block(device, block_sz) else {
                lock(&SHARED_BUFFERS).note_growth_failed(now());
                return None;
            };
            POOL_STATS[7].fetch_add(1, Ordering::Relaxed);
            lock(&SHARED_BUFFERS).add_and_take(heap, started, alloc, align, ahead_target)?
        }
    };
    if need_more {
        wake_grower(device);
    }
    let mut made: Option<ID3D12Resource> = None;
    let mut data = std::ptr::null_mut();
    // SAFETY: `piece` is exclusively owned, `heap` is CPU-visible L0 WRITE_BACK
    // and already resident & mapped; `Reclaim` keeps the piece alive until the
    // last COM reference to `made` is released.
    let result = unsafe {
        device
            .CreatePlacedResource(&heap, piece.start, &desc, D3D12_RESOURCE_STATE_COMMON, None, &mut made)
            .and_then(|()| {
                let b = made.as_ref().ok_or_else(windows::core::Error::empty)?;
                b.Map(0, None, Some(&mut data))
            })
    };
    match (result, made) {
        (Ok(()), Some(made)) => {
            let ptr = data.cast::<u8>();
            if again {
                // SAFETY: `ptr` maps at least `width` exclusively owned bytes.
                unsafe { std::ptr::write_bytes(ptr, 0, width as usize) };
            }
            SHARED_HELD.fetch_add(piece.len, Ordering::Relaxed);
            POOL_STATS[5].fetch_add(1, Ordering::Relaxed);
            give_back_with(&made, piece, POOL_SHARED);
            Some((made, ptr))
        }
        _ => {
            lock(&SHARED_BUFFERS).give(piece, now());
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_are_cut_aligned_and_join_again() {
        let mut r = Ranges::new(1024);
        assert_eq!(r.take(100, 64), Some(0));
        assert_eq!(r.take(100, 64), Some(128));
        assert_eq!(r.take(28, 4), Some(100));
        assert_eq!(r.take(2000, 4), None);
        r.give(128, 100);
        r.give(0, 100);
        assert!(!r.whole(1024));
        r.give(100, 28);
        assert!(r.whole(1024));
    }

    #[test]
    fn empty_blocks_stay_while_the_card_has_room() {
        assert_eq!(spares(Some([100, 84, 0, 0]), true), SPARE_KEPT_8GB);
        assert_eq!(spares(Some([12 << 30, 10 << 30, 0, 0]), true), SPARE_KEPT);
        assert_eq!(spares(Some([100, 85, 0, 0]), true), SPARE);
        assert_eq!(spares(Some([100, 10, 0, 0]), false), SPARE);
        assert_eq!(spares(None, true), SPARE);
    }

    #[test]
    fn a_failed_block_is_not_retried_for_each_small_upload() {
        let mut pool: Pool<u32> = Pool::new(2);
        let calls = std::cell::Cell::new(0);
        let refuse = || { calls.set(calls.get() + 1); None };
        for size in [704008, 179720, 179720, 24080, 48648, 524288, 89616, 351760] {
            assert!(pool.take(size, 512, 12, refuse).is_none());
        }
        assert_eq!(calls.get(), 1);
        assert!(pool.take(24080, 512, 1012, || Some(1)).is_some());
    }

    #[test]
    fn budget_tick_reclaims_idle_blocks_without_reusing_live_or_resting_pieces() {
        let mut pool = Pool::new(STAGING_KEPT);
        let (held, _, _) = pool.take(BLOCK / 2, 1, 0, || Some(1)).unwrap();
        let (resting, _, _) = pool.take(BLOCK / 2, 1, 0, || Some(2)).unwrap();
        pool.give(resting, 0);
        for i in 0..12 {
            pool.add(i, 0);
        }
        pool.reclaim(0, SPARE_KEPT, true);
        assert_eq!(pool.blocks(), STAGING_KEPT + 1);
        pool.reclaim(REST - 1, SPARE, true);
        assert_eq!(pool.blocks(), SPARE + 1);
        assert_eq!(pool.resting.len(), 1);
        pool.reclaim(REST, 0, true);
        assert_eq!(pool.blocks(), 1);
        assert!(pool.resting.is_empty());
        pool.give(held, REST);
        pool.reclaim(2 * REST, 0, true);
        assert_eq!(pool.blocks(), 0);
    }

    #[test]
    fn pieces_never_overlap_and_rest_before_they_are_used_again() {
        let mut pool: Pool<u32> = Pool::new(SPARE);
        let made = std::cell::Cell::new(0);
        let make = || {
            made.set(made.get() + 1);
            Some(made.get())
        };
        let mut taken: Vec<Piece> = Vec::new();
        for i in 0..500u64 {
            let (p, _, again) = pool.take(65536 * (1 + i % 7), 65536, 0, make).unwrap();
            assert!(!again);
            assert!(taken.iter().all(|q| q.block != p.block || p.start + p.len <= q.start || q.start + q.len <= p.start));
            taken.push(p);
        }
        let blocks = pool.blocks();
        assert!(blocks >= 2);
        // Given back at 10: not used again before 10 + REST.
        let first = taken[0];
        pool.give(first, 10);
        let (p, _, _) = pool.take(first.len, 65536, 10 + REST - 1, make).unwrap();
        assert_ne!((p.block, p.start), (first.block, first.start));
        let (q, _, again) = pool.take(first.len, 65536, 10 + REST, make).unwrap();
        assert_eq!((q.block, q.start, again), (first.block, first.start, true));
        // Everything back: the empty blocks past the spare ones go.
        for p in taken.drain(1..).chain([p, q]) {
            pool.give(p, 20);
        }
        pool.rested(20 + REST);
        assert_eq!(pool.blocks(), blocks.min(SPARE));
        // Too big for a block, and a block that cannot be made.
        assert!(pool.take(BLOCK, 65536, 30, make).is_none());
        let mut none: Pool<u32> = Pool::new(SPARE);
        assert!(none.take(65536, 65536, 0, || None).is_none());
    }

    #[test]
    fn pieces_come_from_blocks_older_than_the_lists_that_run() {
        let mut pool: Pool<u64> = Pool::new(SPARE_KEPT);
        let made = std::cell::Cell::new(0);
        let make = || {
            made.set(made.get() + 1);
            Some(made.get())
        };
        let at = |started, finished| Ripe { started, finished, ahead: 2 };
        let take = |pool: &mut Pool<u64>, ripe| pool.take_ripe(BLOCK / 2, 65536, 0, ripe, make).unwrap().1;
        // Nothing runs: at once, and two are made ahead, one a time.
        assert_eq!(take(&mut pool, at(0, 0)), 1);
        assert_eq!(made.get(), 2);
        assert_eq!(take(&mut pool, at(0, 0)), 1);
        assert_eq!(made.get(), 3);
        // 4 is made while submission 5 runs.
        assert_eq!(take(&mut pool, at(5, 4)), 2);
        assert_eq!(made.get(), 4);
        assert_eq!(take(&mut pool, at(5, 4)), 2);
        assert_eq!(take(&mut pool, at(6, 4)), 3);
        assert_eq!(take(&mut pool, at(6, 4)), 3);
        // None is ripe: the oldest; it is ripe once 5 has finished.
        assert_eq!(take(&mut pool, at(7, 4)), 4);
        assert_eq!(take(&mut pool, at(7, 5)), 4);
        // A whole block's worth too.
        assert_eq!(pool.take_ripe(BLOCK, 65536, 0, at(8, 7), make).unwrap().1, 5);
        // Off: the first with room, whatever its age, none ahead.
        let n = made.get();
        assert_eq!(pool.take(BLOCK / 2, 65536, 0, make).unwrap().1, 6);
        assert_eq!(made.get(), n);
        assert!(pool.take(BLOCK, 65536, 0, make).is_none());
    }

    /// A pooled texture's piece comes back when its last reference goes, not
    /// before; its staging piece begins as zeros.
    #[test]
    fn a_texture_gives_its_piece_back_when_it_goes() {
        let Some(host) = crate::host::host() else { return };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: 256,
            Height: 256,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            ..Default::default()
        };
        // SAFETY: a valid description.
        let Some(t) = (unsafe { texture(&host.device, &desc, D3D12_RESOURCE_STATE_COMMON) }) else { return };
        assert_eq!(crate::host::references(&t), 1, "what the texture holds is no reference to it");
        let resting = || lock(&TEXTURES).resting.len();
        let before = resting();
        let other = t.clone();
        drop(t);
        assert_eq!(resting(), before);
        drop(other);
        assert_eq!(resting(), before + 1);
        // SAFETY: the piece's mapping, 4096 bytes of it ours.
        unsafe {
            let (piece, _, at, data) = staging(host, 4096).unwrap();
            assert!(at.is_multiple_of(512));
            std::ptr::write_bytes(data, 0xAB, 4096);
            drop(piece);
            lock(&STAGING).rested(u64::MAX);
            let (_piece, _, _, data) = staging(host, 4096).unwrap();
            assert!((0..4096).all(|i| *data.add(i) == 0));
            // SAFETY: plain query.
            assert!(host.device.GetDeviceRemovedReason().is_ok());
        }
    }

    #[test]
    fn try_take_ripe_signals_background_refill_without_growing_under_lock() {
        let mut pool: Pool<u64> = Pool::new(4);
        let ripe = Ripe { started: 5, finished: 5, ahead: 2 };
        assert!(matches!(pool.try_take_ripe(BLOCK / 2, 65536, 0, ripe), TryTake::Empty { can_grow: true }));
        pool.note_growth_failed(100);
        assert!(matches!(pool.try_take_ripe(BLOCK / 2, 65536, 599, ripe), TryTake::Empty { can_grow: false }));
        assert!(matches!(pool.try_take_ripe(BLOCK / 2, 65536, 600, ripe), TryTake::Empty { can_grow: true }));

        pool.add(1, 4);
        pool.add(2, 4);
        match pool.try_take_ripe(BLOCK / 2, 65536, 600, ripe) {
            TryTake::Found { memory, need_more, .. } => {
                assert_eq!(memory, 1);
                // Only 1 empty block remains (< ahead 2), so background grower is woken.
                assert!(need_more);
            }
            _ => panic!("expected hit from pre-allocated block"),
        }
        pool.add(3, 5);
        pool.add(4, 5);
        match pool.try_take_ripe(BLOCK / 2, 65536, 600, ripe) {
            TryTake::Found { memory, need_more, .. } => {
                assert_eq!(memory, 1);
                // 3 empty blocks remain (>= ahead 2), so no background wake needed.
                assert!(!need_more);
            }
            _ => panic!("expected hit from remaining half of block 1"),
        }
    }

    #[test]
    fn background_ahead_target_respects_vram_and_memory_pressure() {
        let mib = 1024 * 1024u64;
        let gib = 1024 * mib;
        assert_eq!(bg_ahead_for(12, Some([8 * gib, 4 * gib, 0, 0]), false, false, true), 12);
        // 8 GiB GPU (dynamic 75% Alert = 6,144 MiB, 85% Critical = 6,963 MiB):
        // Stage 2 (Alert): cap at 1 empty block; Stage 3 (Critical): stop pre-allocation (0).
        assert_eq!(bg_ahead_for(12, Some([8 * gib, 6_200 * mib, 0, 0]), false, false, true), 1);
        assert_eq!(bg_ahead_for(12, Some([8 * gib, 7 * gib, 0, 0]), false, false, true), 0);
        // 12 GiB GPU (dynamic 75% Alert = 9,216 MiB, 85% Critical = 10,444 MiB):
        assert_eq!(bg_ahead_for(12, Some([12 * gib, 9_500 * mib, 0, 0]), false, false, true), 1);
        assert_eq!(bg_ahead_for(12, Some([12 * gib, 10_500 * mib, 0, 0]), false, false, true), 0);
        // Host RAM pressure or critical commit always stops background pre-allocation.
        assert_eq!(bg_ahead_for(2, Some([8 * gib, 2 * gib, 0, 0]), true, false, true), 0);
        assert_eq!(bg_ahead_for(2, Some([8 * gib, 2 * gib, 0, 0]), false, true, false), 0);
    }

    #[test]
    fn uav_and_shared_buffer_pools_place_reclaim_and_zero_on_reuse() {
        let Some(host) = crate::host::host() else { return };
        let Some(uav) = uav_buffer(&host.device, 4096) else { return };
        assert_eq!(crate::host::references(&uav), 1);
        let uav_before = lock(&UAV_BUFFERS).resting.len();
        drop(uav);
        assert_eq!(lock(&UAV_BUFFERS).resting.len(), uav_before + 1);

        let Some((sh, ptr)) = shared_buffer(&host.device, 4096) else { return };
        assert_eq!(crate::host::references(&sh), 1);
        let sh_before = lock(&SHARED_BUFFERS).resting.len();
        // SAFETY: `ptr` maps 4096 exclusively owned bytes of the placed buffer.
        unsafe {
            std::ptr::write_bytes(ptr, 0x5A, 4096);
            assert_eq!(*ptr, 0x5A);
        }
        drop(sh);
        assert_eq!(lock(&SHARED_BUFFERS).resting.len(), sh_before + 1);
        lock(&SHARED_BUFFERS).rested(u64::MAX);
        let Some((sh2, ptr2)) = shared_buffer(&host.device, 4096) else { return };
        // SAFETY: reused piece must be zeroed.
        unsafe {
            assert!((0..4096).all(|i| *ptr2.add(i) == 0));
        }
        drop(sh2);
        // SAFETY: plain device health query.
        assert!(unsafe { host.device.GetDeviceRemovedReason() }.is_ok());
    }

    #[test]
    fn block_size_and_spares_calibrate_for_eight_gib_vram() {
        // <= 8 GiB VRAM: 32 MiB texture heaps and 32 MiB staging/buffer heaps
        // so 4 MiB and 8 MiB shared buffers fit comfortably in `alloc <= block_sz / 2`.
        let buf_8gb = block_size_for(Some(7_200 << 20), false, None);
        assert_eq!(block_size_for(Some(7_200 << 20), true, None), 32 << 20);
        assert_eq!(buf_8gb, 32 << 20);
        assert_eq!(block_size_for(Some(8 << 30), false, None), 32 << 20);
        assert!(4 << 20 <= buf_8gb / 2);
        assert!(8 << 20 <= buf_8gb / 2);
        // > 8 GiB VRAM: 64 MiB texture heaps, 32 MiB staging/buffer heaps.
        assert_eq!(block_size_for(Some(12 << 30), true, None), 64 << 20);
        assert_eq!(block_size_for(Some(12 << 30), false, None), 32 << 20);
        // Explicit override clamped to 16..=64 MiB.
        assert_eq!(block_size_for(Some(7_200 << 20), false, Some(24)), 24 << 20);
        assert_eq!(block_size_for(Some(7_200 << 20), false, Some(8)), 16 << 20);
        assert_eq!(block_size_for(Some(7_200 << 20), false, Some(128)), 64 << 20);
        // Spare retention stays compact on <= 8 GiB VRAM (4 texture blocks = 128 MiB,
        // 2 staging blocks, 2 shared blocks).
        assert_eq!(SPARE_KEPT_8GB, 4);
        assert_eq!(STAGING_KEPT, 2);
        assert_eq!(SHARED_KEPT, 2);
        assert_eq!(spares(Some([7_200 << 20, 3_000 << 20, 0, 0]), true), SPARE_KEPT_8GB);
        assert_eq!(spares(Some([12 << 30, 3_000 << 20, 0, 0]), true), SPARE_KEPT);
        // 3-stage spare limits: Safe keeps base spares, Alert caps to 1, Critical purges to 0.
        assert_eq!(spare_limit_for_stage(crate::residency::VramStage::Safe, SPARE_KEPT_8GB), 4);
        assert_eq!(spare_limit_for_stage(crate::residency::VramStage::Alert, SPARE_KEPT_8GB), 1);
        assert_eq!(spare_limit_for_stage(crate::residency::VramStage::Critical, SPARE_KEPT_8GB), 0);
        // Host RAM shield: free physical RAM < 1,500 MiB forces host_ram_limit = 0 even in Safe VRAM,
        // and blocks shared_buffer ahead pre-allocation.
        assert_eq!(host_ram_limit_for(crate::residency::VramStage::Safe, false, false, false, 4), 4);
        assert_eq!(host_ram_limit_for(crate::residency::VramStage::Alert, false, false, false, 1), 1);
        assert_eq!(host_ram_limit_for(crate::residency::VramStage::Safe, false, false, true, 4), 0);
        assert_eq!(host_ram_limit_for(crate::residency::VramStage::Safe, false, true, false, 4), 0);
        assert_eq!(shared_ahead_target_for(true, false, false), 0);
        assert_eq!(shared_ahead_target_for(false, true, false), 0);
        assert_eq!(shared_ahead_target_for(false, false, true), 1);
        assert_eq!(shared_ahead_target_for(false, false, false), 2);

        // Host-RAM pools obey HOST_RAM_SPARES (0) on `rested()` even when VRAM SPARES stays > 0.
        let prev_vram = SPARES.swap(4, Ordering::Relaxed);
        let prev_host = HOST_RAM_SPARES.swap(0, Ordering::Relaxed);
        let mut ram_pool: Pool<u64> = Pool::new_host_ram(SHARED_KEPT);
        let (piece, _, _) = ram_pool.take(BLOCK / 2, 65536, 0, || Some(7)).unwrap();
        ram_pool.give(piece, 10);
        ram_pool.rested(10 + REST);
        assert_eq!(ram_pool.blocks(), 0);
        SPARES.store(prev_vram, Ordering::Relaxed);
        HOST_RAM_SPARES.store(prev_host, Ordering::Relaxed);
    }
}
