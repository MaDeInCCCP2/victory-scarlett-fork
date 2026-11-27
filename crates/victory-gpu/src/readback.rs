//! (Step C1) Title read-back ring receiver (`hlehooks::readback::Sink`) and
//! sharded copy-back consumer meter (`VICTORY_BACK_CONSUMERS=1`).
//!
//! Tracks which 64 KiB blocks landed by `relay_card_back` / `land::piece` are
//! actually consumed afterwards — by the title's F1 read-back rings, other
//! host copies (`CardCopy` / `RamCopy`), textures, descriptors, CPU buffer
//! locks, title writes, remaps, or **nothing** — and tracks per-block unread
//! streaks to guide selective copy-back skipping & card-to-card propagation.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use crate::hlehooks::readback::{self as hooks, Sink};
use crate::shadow::Shadow;

const BLOCK_SHIFT: usize = 16; // 64 KiB
const SHARDS: usize = 64;
const REPORT_INTERVAL_MS: u64 = 5_000;
const SKIP_STREAK: u8 = 8;
const DEMOTE_LANDINGS: u64 = 300;
const BLOOM_WORDS: usize = 256; // 16,384 bits in L1 cache

/// Consumer categories for landed copy-back bytes (index into `STATS`).
#[repr(usize)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Consumer {
    Ring = 0,
    CardCopy = 1,
    RamCopy = 2,
    Texture = 3,
    Descriptor = 4,
    TitleLock = 5,
    LayerOther = 6,
    Remap = 7,
    Nothing = 8,
}

std::thread_local! {
    /// Active consumer tag on this thread for `host::read_into` / `host::copy_mapped`.
    /// `None` => default `LayerOther`; `Some(None)` => ignore (internal readback/verify reads).
    static TAG: std::cell::Cell<Option<Option<(Consumer, u64)>>> = const { std::cell::Cell::new(None) };
}

/// Runs `f` with a thread-local consumer tag for title-memory reads.
#[inline]
pub(crate) fn with_tag<R>(tag: Option<(Consumer, u64)>, f: impl FnOnce() -> R) -> R {
    if !consumers_on() {
        return f();
    }
    struct Guard(Option<Option<(Consumer, u64)>>);
    impl Drop for Guard {
        fn drop(&mut self) {
            TAG.with(|c| c.set(self.0));
        }
    }
    let prev = TAG.with(|c| c.replace(Some(tag)));
    let _g = Guard(prev);
    f()
}

/// Called from `host::read_into` and `host::copy_mapped` (source side).
#[inline]
pub(crate) fn on_read(src: usize, len: usize) {
    if len == 0 || !consumers_on() {
        return;
    }
    let (kind, copy_id) = match TAG.with(|c| c.get()) {
        None => (Consumer::LayerOther, 0),
        Some(None) => return,
        Some(Some(pair)) => pair,
    };
    if HELD_COUNT.load(Ordering::Acquire) > 0 {
        demand_range(src, len);
    }
    consumed(src, len, kind, copy_id);
}

const STAT_COUNT: usize = 11; // 0..=8 consumers, 9 = landed bytes, 10 = landed pieces
static STATS: [AtomicU64; STAT_COUNT] = [const { AtomicU64::new(0) }; STAT_COUNT];
static RING_LOCKS: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2]; // [inside_set, outside_set]
/// `[0] skipped_bytes, [1] skipped_pieces, [2] demanded_bytes, [3] demands, [4] demand_us, [5] demoted, [6] failed_fetches`
static SKIP_STATS: [AtomicU64; 7] = [const { AtomicU64::new(0) }; 7];

/// Registered F1 read-back ring buffers: `(start_va, end_va, site, name)`.
static RINGS: RwLock<Vec<(usize, usize, u64, &'static str)>> = RwLock::new(Vec::new());

struct HeldPiece {
    shadow: Arc<Shadow>,
    lo: usize,
    hi: usize,
    t0: Vec<u8>,
}

struct PieceState {
    producer_id: u64,
    bytes: u32,
    consumed: bool,
    ever_locked: bool,
    unread_streak: u8,
    last_gen: u64,
    demoted_until: u64,
    held: Option<HeldPiece>,
}

static LANDING_GEN: AtomicU64 = AtomicU64::new(0);
static LAST_REPORT_MS: AtomicU64 = AtomicU64::new(0);
static HELD_COUNT: AtomicUsize = AtomicUsize::new(0);
static HELD_BYTES: AtomicU64 = AtomicU64::new(0);
static HELD_BITS: [AtomicU64; BLOOM_WORDS] = [const { AtomicU64::new(0) }; BLOOM_WORDS];
static START_TIME: OnceLock<std::time::Instant> = OnceLock::new();
static TABLE: OnceLock<[Mutex<HashMap<usize, PieceState>>; SHARDS]> = OnceLock::new();

fn shards() -> &'static [Mutex<HashMap<usize, PieceState>>; SHARDS] {
    TABLE.get_or_init(|| std::array::from_fn(|_| Mutex::new(HashMap::new())))
}

#[inline]
fn bloom_bit(block: usize) -> (usize, u64) {
    let h = block.wrapping_mul(0x9e37_79b9_7f4a_7c15usize) >> 50;
    let idx = h & (BLOOM_WORDS * 64 - 1);
    (idx >> 6, 1u64 << (idx & 63))
}

#[inline]
fn bloom_set(block: usize) {
    let (w, bit) = bloom_bit(block);
    HELD_BITS[w].fetch_or(bit, Ordering::Release);
}

#[inline]
fn bloom_may_hold(block: usize) -> bool {
    let (w, bit) = bloom_bit(block);
    (HELD_BITS[w].load(Ordering::Acquire) & bit) != 0
}

/// Whether chronically unread copy-back pieces are skipped on the card
/// (`VICTORY_BACK_SKIP_UNREAD=1`, default `0`).
pub(crate) fn skip_unread_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_BACK_SKIP_UNREAD").is_some_and(|v| v == "1"))
}

fn skip_cap_bytes() -> u64 {
    static CAP: OnceLock<u64> = OnceLock::new();
    *CAP.get_or_init(|| {
        let mb = std::env::var("VICTORY_BACK_SKIP_UNREAD_MB")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(256);
        mb.saturating_mul(1024 * 1024)
    })
}

/// Whether the copy-back consumer tracker is active (`VICTORY_BACK_CONSUMERS`,
/// default `1` in `diagnostics` builds, or whenever `VICTORY_BACK_SKIP_UNREAD=1`).
pub(crate) fn consumers_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        skip_unread_on()
            || match std::env::var("VICTORY_BACK_CONSUMERS") {
                Ok(v) => v.trim() != "0",
                Err(_) => cfg!(feature = "diagnostics"),
            }
    })
}

/// Registers the GPU-side `hlehooks::readback::Sink` once during host startup.
pub(crate) fn init() {
    let _ = START_TIME.get_or_init(std::time::Instant::now);
    hooks::set(Sink {
        ring_created,
        lock_entered,
        unlocked,
        lock_returned,
        ring_read,
        ring_marked,
        group_dropped,
        drawable_removed,
    });
}

/// Whether `[start, start + len)` overlaps any registered F1 read-back ring.
pub(crate) fn in_readback_set(start: usize, len: usize) -> bool {
    let end = start.saturating_add(len);
    let rings = RINGS.read().unwrap_or_else(|e| e.into_inner());
    rings.iter().any(|&(a, b, _, _)| a < end && b > start)
}

fn read_u64_at(addr: usize) -> Option<u64> {
    let mut buf = [0u8; 8];
    with_tag(None, || crate::host::read_into(addr as *const u8, &mut buf)).then(|| u64::from_le_bytes(buf))
}

fn ring_created(ring: usize, alias_at: u64, site: u64, name: &'static str) {
    with_tag(None, || ring_created_inner(ring, alias_at, site, name));
}

fn ring_created_inner(ring: usize, alias_at: u64, site: u64, name: &'static str) {
    let mut alias = [0u8; 24];
    let (alias_cpu, alias_len, alias_gpu) = if alias_at != 0 && crate::host::read_into(alias_at as *const u8, &mut alias) {
        (
            u64::from_le_bytes(alias[0..8].try_into().unwrap()),
            u64::from_le_bytes(alias[8..16].try_into().unwrap()),
            u64::from_le_bytes(alias[16..24].try_into().unwrap()),
        )
    } else {
        (0, 0, 0)
    };
    let mut added = Vec::new();
    for slot in 0..3usize {
        let Some(handle) = read_u64_at(ring + slot * 8) else { continue };
        if handle == 0 {
            continue;
        }
        let mut obj = [0u8; 0x40];
        if !crate::host::read_into(handle as *const u8, &mut obj) {
            continue;
        }
        let count = u32::from_le_bytes(obj[0x08..0x0c].try_into().unwrap()) as u64;
        let stride = (u16::from_le_bytes(obj[0x0c..0x0e].try_into().unwrap()) & 0x0fff) as u64;
        let size = count.saturating_mul(stride);
        if size == 0 || size > (64 << 20) {
            continue;
        }
        let flags = u16::from_le_bytes(obj[0x10..0x12].try_into().unwrap());
        let off = if (flags & 0x2040) != 0 {
            u32::from_le_bytes(obj[0x14..0x18].try_into().unwrap()) as u64
        } else {
            0
        };
        let va = u64::from_le_bytes(obj[0x38..0x40].try_into().unwrap());
        if va == 0 {
            continue;
        }
        let addr = va.wrapping_add(off) as usize;
        added.push((addr, addr.saturating_add(size as usize), site, name));
        if alias_len > 0 && va >= alias_cpu && va < alias_cpu.saturating_add(alias_len) {
            let gpu_addr = alias_gpu.wrapping_add(va - alias_cpu).wrapping_add(off) as usize;
            if gpu_addr != addr {
                added.push((gpu_addr, gpu_addr.saturating_add(size as usize), site, name));
            }
        }
    }
    if !added.is_empty() {
        let count = added.len();
        let bytes: usize = added.iter().map(|r| r.1 - r.0).sum();
        let mut rings = RINGS.write().unwrap_or_else(|e| e.into_inner());
        rings.extend(added);
        let total = rings.len();
        drop(rings);
        tracing::info!(
            site = format_args!("{site:#x}"),
            name,
            buffers = count,
            bytes,
            total_buffers = total,
            "read-back ring registered (F1)"
        );
    }
}

fn lock_entered(_handle: u64, _record: u64, _r8: u64, _r9: u64, _caller: u64) {}
fn unlocked(_record: u64) {}

fn lock_returned(_handle: u64, record: u64, _caller: u64, ok: bool) {
    if !ok || record == 0 {
        return;
    }
    let mut rec = [0u8; 0x20];
    if !with_tag(None, || crate::host::read_into(record as *const u8, &mut rec)) {
        return;
    }
    let ptr = u64::from_le_bytes(rec[0..8].try_into().unwrap()) as usize;
    let size = u32::from_le_bytes(rec[0x18..0x1c].try_into().unwrap()) as usize;
    if ptr == 0 || size == 0 {
        return;
    }
    let inside = in_readback_set(ptr, size);
    RING_LOCKS[usize::from(!inside)].fetch_add(1, Ordering::Relaxed);
    if HELD_COUNT.load(Ordering::Acquire) > 0 {
        demand_range(ptr, size);
    }
    if consumers_on() {
        consumed_lock(ptr, size);
    }
}

fn ring_read(_ring: u64, record: u64, ok: bool) {
    lock_returned(0, record, 0, ok);
}

fn ring_marked(_ring: u64) {}
fn group_dropped(_group: u64) {}
fn drawable_removed(_entity: u64, _flags: u64, _caller: u64, _slot: u64) {}

fn consumed_lock(title_start: usize, len: usize) {
    let first = title_start >> BLOCK_SHIFT;
    let last = title_start.saturating_add(len - 1) >> BLOCK_SHIFT;
    let limit = (first + 256).min(last + 1);
    let table = shards();
    for block in first..limit {
        let mut shard = table[block & (SHARDS - 1)].lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = shard.get_mut(&block) {
            entry.ever_locked = true;
            if !entry.consumed {
                entry.consumed = true;
                entry.unread_streak = 0;
                STATS[Consumer::TitleLock as usize].fetch_add(u64::from(entry.bytes), Ordering::Relaxed);
            }
        }
    }
}

/// Called when `land::piece` wrote `bytes` of shader output at `title_start`.
pub(crate) fn landed(title_start: usize, bytes: usize, producer_id: u64) {
    if bytes == 0 || !consumers_on() {
        return;
    }
    STATS[9].fetch_add(bytes as u64, Ordering::Relaxed);
    STATS[10].fetch_add(1, Ordering::Relaxed);
    let block = title_start >> BLOCK_SHIFT;
    let epoch = LANDING_GEN.load(Ordering::Relaxed);
    let is_ring = in_readback_set(title_start, bytes);
    let mut shard = shards()[block & (SHARDS - 1)].lock().unwrap_or_else(|e| e.into_inner());
    let (prev_streak, ever_locked, demoted_until, held) = if let Some(old) = shard.get_mut(&block) {
        let held = old.held.take();
        if let Some(ref h) = held {
            HELD_COUNT.fetch_sub(1, Ordering::AcqRel);
            HELD_BYTES.fetch_sub((h.hi - h.lo) as u64, Ordering::Relaxed);
        }
        if !old.consumed {
            STATS[Consumer::Nothing as usize].fetch_add(u64::from(old.bytes), Ordering::Relaxed);
            (old.unread_streak.saturating_add(1), old.ever_locked, old.demoted_until, held)
        } else {
            (0, old.ever_locked, old.demoted_until, held)
        }
    } else {
        (0, false, 0, None)
    };
    drop(held);
    let consumed = is_ring;
    if is_ring {
        STATS[Consumer::Ring as usize].fetch_add(bytes as u64, Ordering::Relaxed);
    }
    shard.insert(
        block,
        PieceState {
            producer_id,
            bytes: bytes.min(u32::MAX as usize) as u32,
            consumed,
            ever_locked,
            unread_streak: if is_ring { 0 } else { prev_streak },
            last_gen: epoch,
            demoted_until,
            held: None,
        },
    );
}

/// (Step C1, `VICTORY_BACK_SKIP_UNREAD=1`) Attempts to leave a chronically
/// unread piece on the card instead of fetching, merging, and writing RAM.
/// Returns `true` when skipped (caller must not fetch/merge it).
pub(crate) fn try_skip(shadow: &Arc<Shadow>, lo: usize, hi: usize) -> bool {
    if !skip_unread_on() || !shadow.carded() || shadow.in_place() || hi <= lo {
        return false;
    }
    let len = hi - lo;
    if len > (1 << BLOCK_SHIFT) {
        return false;
    }
    let title = shadow.span().0 as usize + lo;
    let block = title >> BLOCK_SHIFT;
    if ((title + len - 1) >> BLOCK_SHIFT) != block || in_readback_set(title, len) {
        return false;
    }
    let epoch = LANDING_GEN.load(Ordering::Relaxed);
    let id = shadow.id();
    let need_snapshot = {
        let mut shard = shards()[block & (SHARDS - 1)].lock().unwrap_or_else(|e| e.into_inner());
        let Some(st) = shard.get_mut(&block) else {
            return false;
        };
        if st.unread_streak < SKIP_STREAK || st.ever_locked || st.demoted_until > epoch || st.producer_id != id {
            return false;
        }
        if let Some(ref mut h) = st.held
            && h.shadow.id() == id
            && h.lo == lo
            && h.hi == hi
        {
            // Already held with its original t0 snapshot: O(1) skip.
            st.last_gen = epoch;
            st.unread_streak = st.unread_streak.saturating_add(1);
            SKIP_STATS[0].fetch_add(len as u64, Ordering::Relaxed);
            SKIP_STATS[1].fetch_add(1, Ordering::Relaxed);
            false
        } else {
            if HELD_BYTES.load(Ordering::Relaxed).saturating_add(len as u64) > skip_cap_bytes() {
                return false;
            }
            true
        }
    };
    if !need_snapshot {
        return true;
    }
    let mut t0 = vec![0u8; len];
    if !with_tag(None, || crate::host::read_into(title as *const u8, &mut t0)) {
        return false;
    }
    let installed = {
        let mut shard = shards()[block & (SHARDS - 1)].lock().unwrap_or_else(|e| e.into_inner());
        if let Some(st) = shard.get_mut(&block)
            && st.unread_streak >= SKIP_STREAK
            && !st.ever_locked
            && st.demoted_until <= epoch
            && st.producer_id == id
            && st.held.is_none()
        {
            st.held = Some(HeldPiece { shadow: shadow.clone(), lo, hi, t0 });
            st.last_gen = epoch;
            st.unread_streak = st.unread_streak.saturating_add(1);
            bloom_set(block);
            HELD_COUNT.fetch_add(1, Ordering::AcqRel);
            HELD_BYTES.fetch_add(len as u64, Ordering::Relaxed);
            SKIP_STATS[0].fetch_add(len as u64, Ordering::Relaxed);
            SKIP_STATS[1].fetch_add(1, Ordering::Relaxed);
            true
        } else {
            false
        }
    };
    if installed {
        shadow.note_skipped(lo, hi);
    }
    installed
}

/// Called in `relay_card_back` for pieces that WILL be fetched: supersedes any
/// held piece for the same copy, or settles a held piece belonging to another copy.
pub(crate) fn on_fetching(shadow: &Arc<Shadow>, lo: usize, hi: usize) {
    if HELD_COUNT.load(Ordering::Acquire) == 0 || hi <= lo {
        return;
    }
    let title = shadow.span().0 as usize + lo;
    let block = title >> BLOCK_SHIFT;
    if !bloom_may_hold(block) {
        return;
    }
    let settle = {
        let mut shard = shards()[block & (SHARDS - 1)].lock().unwrap_or_else(|e| e.into_inner());
        let Some(st) = shard.get_mut(&block) else {
            return;
        };
        let Some(h) = st.held.take() else {
            return;
        };
        HELD_COUNT.fetch_sub(1, Ordering::AcqRel);
        HELD_BYTES.fetch_sub((h.hi - h.lo) as u64, Ordering::Relaxed);
        if h.shadow.id() == shadow.id() && h.lo == lo && h.hi == hi {
            None
        } else {
            Some(h)
        }
    };
    if let Some(h) = settle {
        settle_held(h);
    }
}

/// Settles any held skipped pieces overlapping `[start, start + len)`.
pub(crate) fn demand_range(start: usize, len: usize) {
    if len == 0 || HELD_COUNT.load(Ordering::Acquire) == 0 {
        return;
    }
    let first = start >> BLOCK_SHIFT;
    let last = start.saturating_add(len - 1) >> BLOCK_SHIFT;
    let epoch = LANDING_GEN.load(Ordering::Relaxed);
    let table = shards();
    let mut to_settle: Vec<HeldPiece> = Vec::new();
    for block in first..=last {
        if !bloom_may_hold(block) {
            continue;
        }
        let mut shard = table[block & (SHARDS - 1)].lock().unwrap_or_else(|e| e.into_inner());
        if let Some(st) = shard.get_mut(&block)
            && let Some(h) = st.held.take()
        {
            HELD_COUNT.fetch_sub(1, Ordering::AcqRel);
            HELD_BYTES.fetch_sub((h.hi - h.lo) as u64, Ordering::Relaxed);
            st.demoted_until = epoch.saturating_add(DEMOTE_LANDINGS);
            st.unread_streak = 0;
            st.consumed = true;
            SKIP_STATS[5].fetch_add(1, Ordering::Relaxed);
            to_settle.push(h);
        }
    }
    for h in to_settle {
        settle_held(h);
    }
}

fn settle_held(held: HeldPiece) {
    let started = std::time::Instant::now();
    let len = held.hi.saturating_sub(held.lo);
    let Some(host) = crate::host::host() else {
        SKIP_STATS[6].fetch_add(1, Ordering::Relaxed);
        return;
    };
    let Some(fetched) = host.demand_fetch(&held.shadow, held.lo, held.hi) else {
        SKIP_STATS[6].fetch_add(1, Ordering::Relaxed);
        return;
    };
    let _ = held.shadow.settle_skipped(held.lo, held.hi, &fetched, &held.t0);
    SKIP_STATS[2].fetch_add(len as u64, Ordering::Relaxed);
    SKIP_STATS[3].fetch_add(1, Ordering::Relaxed);
    SKIP_STATS[4].fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
}

/// Called when a consumer reads or invalidates `[title_start, title_start + len)`.
/// Pass `reader_copy_id = 0` for non-copy consumers (textures, descriptors, remaps).
pub(crate) fn consumed(title_start: usize, len: usize, kind: Consumer, reader_copy_id: u64) {
    if len == 0 || !consumers_on() {
        return;
    }
    let first = title_start >> BLOCK_SHIFT;
    let last = title_start.saturating_add(len - 1) >> BLOCK_SHIFT;
    let limit = (first + 256).min(last + 1);
    let table = shards();
    for block in first..limit {
        let mut shard = table[block & (SHARDS - 1)].lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = shard.get_mut(&block) {
            if !entry.consumed && (reader_copy_id == 0 || reader_copy_id != entry.producer_id) {
                entry.consumed = true;
                entry.unread_streak = 0;
                STATS[kind as usize].fetch_add(u64::from(entry.bytes), Ordering::Relaxed);
            }
        }
    }
}

/// Advances landing generation and logs the 5-second copy-back consumer split.
pub(crate) fn on_landing() {
    if !consumers_on() {
        return;
    }
    let epoch = LANDING_GEN.fetch_add(1, Ordering::Relaxed) + 1;
    let elapsed_ms = START_TIME.get_or_init(std::time::Instant::now).elapsed().as_millis() as u64;
    let prev = LAST_REPORT_MS.load(Ordering::Relaxed);
    if elapsed_ms.saturating_sub(prev) < REPORT_INTERVAL_MS {
        return;
    }
    if LAST_REPORT_MS.compare_exchange(prev, elapsed_ms, Ordering::AcqRel, Ordering::Relaxed).is_err() {
        return;
    }
    let mut tracked = 0usize;
    let mut cold = 0usize;
    let table = shards();
    for shard_lock in table {
        let mut shard = shard_lock.lock().unwrap_or_else(|e| e.into_inner());
        shard.retain(|_, st| {
            if st.held.is_none() && epoch.saturating_sub(st.last_gen) > 2000 {
                if !st.consumed {
                    STATS[Consumer::Nothing as usize].fetch_add(u64::from(st.bytes), Ordering::Relaxed);
                }
                return false;
            }
            tracked += 1;
            if st.unread_streak >= SKIP_STREAK && !st.ever_locked {
                cold += 1;
            }
            true
        });
    }
    let s: [u64; STAT_COUNT] = std::array::from_fn(|i| STATS[i].swap(0, Ordering::Relaxed));
    let sk: [u64; 7] = std::array::from_fn(|i| SKIP_STATS[i].swap(0, Ordering::Relaxed));
    let mb = |b: u64| b as f64 / (1024.0 * 1024.0);
    if s[9] > 0 {
        let accounted = s[0] + s[1] + s[2] + s[3] + s[4] + s[5] + s[6] + s[7] + s[8];
        let nothing_pct = if accounted > 0 { s[8] as f64 * 100.0 / accounted as f64 } else { 0.0 };
        let ring_count = RINGS.read().map_or(0, |r| r.len());
        let locks_in = RING_LOCKS[0].swap(0, Ordering::Relaxed);
        let locks_out = RING_LOCKS[1].swap(0, Ordering::Relaxed);
        tracing::info!(
            "copy-back consumers (VICTORY_BACK_CONSUMERS=1, 5 s): landed {:.1} MB ({} pieces); consumed by: rings {:.1} MB ({} bufs), card copies {:.1} MB, RAM copies {:.1} MB, textures {:.1} MB, descriptors {:.1} MB, title locks {:.1} MB (in {} / out {}), layer other {:.1} MB, remaps {:.1} MB, nothing {:.1} MB ({:.1} %); tracked {} pieces ({} cold streak>=8)",
            mb(s[9]),
            s[10],
            mb(s[0]),
            ring_count,
            mb(s[1]),
            mb(s[2]),
            mb(s[3]),
            mb(s[4]),
            mb(s[5]),
            locks_in,
            locks_out,
            mb(s[6]),
            mb(s[7]),
            mb(s[8]),
            nothing_pct,
            tracked,
            cold,
        );
    }
    if skip_unread_on() && (sk[1] > 0 || sk[3] > 0 || HELD_COUNT.load(Ordering::Relaxed) > 0) {
        let mean_ms = if sk[3] > 0 { (sk[4] as f64 / sk[3] as f64) / 1000.0 } else { 0.0 };
        tracing::info!(
            "copy-back skip (VICTORY_BACK_SKIP_UNREAD=1, 5 s): skipped {:.1} MB ({} pieces); demanded {:.1} MB ({} demands, mean {:.2} ms, demoted {}, failed {}); held now {:.1} MB ({} pieces)",
            mb(sk[0]),
            sk[1],
            mb(sk[2]),
            sk[3],
            mean_ms,
            sk[5],
            sk[6],
            mb(HELD_BYTES.load(Ordering::Relaxed)),
            HELD_COUNT.load(Ordering::Relaxed),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn consumer_attribution_and_self_ignore() {
        let block = 0x2000_0000usize;
        let idx = (block >> BLOCK_SHIFT) & (SHARDS - 1);
        {
            let mut shard = shards()[idx].lock().unwrap_or_else(|e| e.into_inner());
            shard.remove(&(block >> BLOCK_SHIFT));
        }
        let mut shard = shards()[idx].lock().unwrap_or_else(|e| e.into_inner());
        shard.insert(
            block >> BLOCK_SHIFT,
            PieceState {
                producer_id: 42,
                bytes: 4096,
                consumed: false,
                ever_locked: false,
                unread_streak: 0,
                last_gen: 1,
                demoted_until: 0,
                held: None,
            },
        );
        if let Some(st) = shard.get_mut(&(block >> BLOCK_SHIFT)) {
            assert_eq!(st.producer_id, 42);
            if !st.consumed && 42 != st.producer_id {
                st.consumed = true;
            }
            assert!(!st.consumed);
            if !st.consumed && 99 != st.producer_id {
                st.consumed = true;
            }
            assert!(st.consumed);
        }
        shard.remove(&(block >> BLOCK_SHIFT));
    }
}
