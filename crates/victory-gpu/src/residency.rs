//! Host-memory admission for speculative texture work and reusable upload storage.
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;
static PRESSURED: AtomicBool = AtomicBool::new(false);
static HEADROOM: AtomicU64 = AtomicU64::new(u64::MAX);

// Live GPU VRAM telemetry (LOCAL + NON_LOCAL from IDXGIAdapter3::QueryVideoMemoryInfo).
static VRAM_BUDGET: AtomicU64 = AtomicU64::new(0);
static VRAM_USED: AtomicU64 = AtomicU64::new(0);
static VRAM_NON_LOCAL_BUDGET: AtomicU64 = AtomicU64::new(0);
static VRAM_NON_LOCAL_USED: AtomicU64 = AtomicU64::new(0);
static VRAM_EVICTED_HEAPS: AtomicU64 = AtomicU64::new(0);

/// Records the latest DXGI video memory sample `[local_budget, local_used, non_local_budget, non_local_used]`.
pub(crate) fn update_vram(memory: Option<[u64; 4]>) {
    if let Some([budget, used, nl_budget, nl_used]) = memory {
        VRAM_BUDGET.store(budget, Ordering::Relaxed);
        VRAM_USED.store(used, Ordering::Relaxed);
        VRAM_NON_LOCAL_BUDGET.store(nl_budget, Ordering::Relaxed);
        VRAM_NON_LOCAL_USED.store(nl_used, Ordering::Relaxed);
    }
}

pub(crate) fn vram_budget() -> u64 {
    VRAM_BUDGET.load(Ordering::Relaxed)
}

#[allow(dead_code)]
pub(crate) fn vram_used() -> u64 {
    VRAM_USED.load(Ordering::Relaxed)
}

#[allow(dead_code)]
pub(crate) fn vram_non_local() -> (u64, u64) {
    (VRAM_NON_LOCAL_BUDGET.load(Ordering::Relaxed), VRAM_NON_LOCAL_USED.load(Ordering::Relaxed))
}

/// True when local VRAM usage is at or above 80 % of the adapter budget.
pub(crate) fn vram_pressured() -> bool {
    let (budget, used) = (VRAM_BUDGET.load(Ordering::Relaxed), VRAM_USED.load(Ordering::Relaxed));
    budget > 0 && u128::from(used) * 100 >= u128::from(budget) * 80
}

/// Budget-proportional cap on empty 64 MiB VRAM blocks kept idle in a pool.
/// Prevents hoarding up to 4 GiB of empty blocks on 8-12 GB discrete GPUs.
pub(crate) fn recommended_spare_blocks(memory: Option<[u64; 4]>, keep: bool) -> usize {
    let Some([budget, used, ..]) = memory else {
        return if keep { 2 } else { 1 };
    };
    if budget == 0 {
        return if keep { 2 } else { 1 };
    }
    let pct = (u128::from(used) * 100 / u128::from(budget)) as u64;
    if !keep || pct >= 90 {
        0
    } else if pct >= 85 {
        1
    } else if pct >= 75 {
        2
    } else {
        // ~5 % of local VRAM budget in 64 MiB blocks, clamped to [2, 6]:
        // 8 GB -> 2 (128 MiB), 12 GB -> 3 (192 MiB), 16 GB -> 5 (320 MiB), 24 GB -> 6 (384 MiB).
        let cap = (budget / (3 * GIB)) as usize;
        cap.clamp(2, 6)
    }
}

/// Budget-proportional count of pre-allocated ripe 64 MiB blocks so placed and paged
/// pools do not each hoard 12 empty blocks (1.5 GiB) simultaneously on 12 GB GPUs.
pub(crate) fn recommended_ripe_blocks(budget: u64, page_pool_on: bool, is_pages: bool) -> usize {
    if page_pool_on && !is_pages {
        // Placed pool is only the fallback for <64 KiB / non-2D textures when page pool is on.
        return 1;
    }
    if budget == 0 || budget <= 9 * GIB {
        1
    } else if budget <= 13 * GIB {
        2
    } else {
        3
    }
}

/// Explicitly evicts a cold D3D12 heap from local VRAM via `ID3D12Device::Evict` before drop
/// or under high VRAM pressure, returning true on success.
#[allow(dead_code)]
pub(crate) fn evict_heap(
    device: &windows::Win32::Graphics::Direct3D12::ID3D12Device,
    heap: &windows::Win32::Graphics::Direct3D12::ID3D12Heap,
) -> bool {
    use windows::Win32::Graphics::Direct3D12::ID3D12Pageable;
    use windows::core::Interface;
    let Ok(pageable) = heap.cast::<ID3D12Pageable>() else {
        return false;
    };
    // SAFETY: `pageable` is a live D3D12 heap owned by our device.
    let ok = unsafe { device.Evict(&[Some(pageable)]) }.is_ok();
    if ok {
        VRAM_EVICTED_HEAPS.fetch_add(1, Ordering::Relaxed);
    }
    ok
}

#[allow(dead_code)]
pub(crate) fn vram_evicted_heaps() -> u64 {
    VRAM_EVICTED_HEAPS.load(Ordering::Relaxed)
}

fn memory_status() -> Option<MEMORYSTATUSEX> {
    // SAFETY: the API initializes the fields of a correctly sized structure.
    let mut status: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    status.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    (unsafe { GlobalMemoryStatusEx(&mut status) } != 0).then_some(status)
}

// The texture sweep may run for seconds inside a single submitted frame.
static NEXT_COMMIT_SAMPLE: AtomicU64 = AtomicU64::new(0);
static AVAILABLE_COMMIT: AtomicU64 = AtomicU64::new(u64::MAX);
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

fn sample_commit(now: u64) -> u64 {
    let next = NEXT_COMMIT_SAMPLE.load(Ordering::Relaxed);
    if now >= next
        && NEXT_COMMIT_SAMPLE
            .compare_exchange(next, now.saturating_add(32), Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        && let Some(status) = memory_status()
    {
        AVAILABLE_COMMIT.store(status.ullAvailPageFile, Ordering::Relaxed);
    }
    AVAILABLE_COMMIT.load(Ordering::Relaxed)
}

fn has_upload_reserve(commit: u64, bake: bool) -> bool {
    // Keep a protective floor against Rust/game OOM aborts without starving
    // bindless world textures or one-shot ped decoration bakes when steady-state
    // commit headroom sits in the 1-4 GiB range on 32 GiB hosts.
    commit >= if bake { 256 * MIB } else { 512 * MIB }
}

fn has_growth_reserve(commit: u64, bake: bool) -> bool {
    // Growing a fresh 64 MiB upload block for speculative bindless prefetch
    // requires more headroom than reusing slices already committed inside the
    // staging pool.
    commit >= if bake { 256 * MIB } else { 1024 * MIB }
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
    has_growth_reserve(sample_commit(now), bake)
}

pub(crate) fn limited_host_ram() -> bool {
    static LIMITED: OnceLock<bool> = OnceLock::new();
    *LIMITED.get_or_init(|| memory_status().is_some_and(|s| s.ullTotalPhys <= 32 * GIB))
}

fn next_pressure(was: bool, physical: u64, commit: u64) -> bool {
    if was {
        physical < 5 * GIB || commit < 6 * GIB
    } else {
        physical <= 3 * GIB || commit <= 4 * GIB
    }
}

/// Called by the once-per-second submission sweep. A failed sample keeps the last good state.
pub(crate) fn refresh() {
    let Some(status) = memory_status() else { return };
    let (physical, commit) = (status.ullAvailPhys, status.ullAvailPageFile);
    HEADROOM.store(physical.min(commit), Ordering::Relaxed);
    let was = pressured();
    let pressure = next_pressure(was, physical, commit);
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

/// True when OS commit headroom is critically low (< 1.5 GiB) or an allocation
/// just failed with `E_OUTOFMEMORY`, requiring emergency throttling.
pub(crate) fn critical_commit() -> bool {
    let now = crate::host::coarse_ms();
    now < RETRY_UPLOAD_AT.load(Ordering::Relaxed) || sample_commit(now) < 1536 * MIB
}

/// (Fox port C) Which host-RAM caps the texture pass picked by index takes
/// (`VICTORY_TEXTURE_RAM_CAPS`, default 1 = cool_fox's behaviour, all
/// caps; A/B candidate): 0 only the critical clause
/// (commit under 1.5 GiB or an out-of-memory return in the last 500 ms:
/// 16 MiB a frame); 1 also the soft ones of cool_fox (64 MiB a frame under
/// pressure or on a <= 32 GiB PC with >= 1 GiB of staging alive, and the
/// 128 MiB base on a <= 32 GiB PC, as in cool_fox). On our PC (32 GB, run-100) pressure
/// was on for most of the run from 2.9 GB free physical memory alone, and
/// the soft caps cut the pass to a third of ours (192 MiB): textures came
/// later ("due" 58,726 against 29,900). The critical clause alone still
/// guards against aborts.
pub(crate) fn soft_caps() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let on = std::env::var_os("VICTORY_TEXTURE_RAM_CAPS").is_none_or(|v| v != "0");
        tracing::info!(
            "texture pass picked by index under host-RAM pressure: {} (VICTORY_TEXTURE_RAM_CAPS={})",
            if on { "soft caps of 64 / 128 MiB a frame and the critical 16 MiB" } else { "only the critical 16 MiB cap (commit < 1.5 GiB or out of memory)" },
            u8::from(on)
        );
        on
    })
}

/// Frames whose texture pass a host-RAM cap made smaller, since the start:
/// by the critical clause, by the soft ones.
pub(crate) static CAPPED: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// [`prefetch_limit`] as the pass takes it ([`soft_caps`]), counted.
pub(crate) fn texture_pass_limit(bytes: u64) -> u64 {
    let critical = critical_commit();
    let soft = soft_caps();
    let limit = prefetch_limit(bytes, soft && pressured(), critical, soft && limited_host_ram(), crate::texture::live_staging_bytes());
    if limit < bytes {
        CAPPED[usize::from(!critical)].fetch_add(1, Ordering::Relaxed);
    }
    limit
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
    fn vram_spare_and_ripe_blocks_scale_with_adapter_budget() {
        let g12 = 11 * GIB; // ~11 GiB usable D3D12 budget on 12 GB RTX 4070 SUPER
        assert_eq!(recommended_spare_blocks(Some([g12, 5 * GIB, 0, 0]), true), 3);
        assert_eq!(recommended_spare_blocks(Some([g12, 9 * GIB, 0, 0]), true), 2);
        assert_eq!(recommended_spare_blocks(Some([g12, 10 * GIB, 0, 0]), true), 0);
        assert_eq!(recommended_spare_blocks(Some([g12, 5 * GIB, 0, 0]), false), 0);
        assert_eq!(recommended_ripe_blocks(g12, true, false), 1);
        assert_eq!(recommended_ripe_blocks(g12, true, true), 2);
        assert_eq!(recommended_ripe_blocks(8 * GIB, false, false), 1);
        assert_eq!(recommended_ripe_blocks(16 * GIB, true, true), 3);
    }
}
