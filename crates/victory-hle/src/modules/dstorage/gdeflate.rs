//! Optional GPU GDeflate path, ported from Neptune-GDeflate (SomewhereHigh's
//! integration, `crates/victory-hle/src/modules/dstorage/native.rs`).
//!
//! With `VICTORY_GDEFLATE_PACK` set to a verified pack made by
//! `tools/gdeflate` (Neptune) from the user's own archives, a compressed
//! request whose source bytes are in the pack is decoded by DirectStorage on
//! the GPU through `fx1-storage.dll` (in `VICTORY_GDEFLATE_BRIDGE_DIR`, or
//! beside the pack). The bridge checks the decoded bytes against the digest
//! recorded for them; anything it does not deliver goes to the CPU decoder
//! as before. Decoded bytes are written through `into_destination`, so this
//! fork's own guards over title memory apply unchanged (Neptune's
//! direct-to-title write is not used here).
//!
//! Every 30 s, and whenever the delivered count reaches a power of two, a
//! summary of deliveries and fallbacks is logged at info level under
//! `victory_hle::gdeflate`.

use std::cell::RefCell;
use std::ffi::{OsStr, c_void};
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use windows_sys::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW,
};

use super::{HRESULT, Request};

type PrepareWrite = extern "C" fn(*mut u8, u32);

/// `fx1_read_gdeflate` (status 1: delivered, 0: not, < 0: failed).
type Read = unsafe extern "C" fn(
    *mut c_void,
    *const u16,
    *const u8,
    u32,
    u32,
    u32,
    *mut u8,
    *mut u32,
) -> i32;

/// `fx1_read_gdeflate_detailed_v1`: the same, with the reason a block was not
/// delivered (2 lane busy, 3 not in the pack, 4 disabled after an error,
/// 5 invalid input, 6 size outside a lane, 7 no GPU decompression).
type Detailed = unsafe extern "C" fn(
    *mut c_void,
    *const u16,
    *const u8,
    u32,
    u32,
    u32,
    *mut u8,
    *mut u32,
    Option<PrepareWrite>,
    *const c_void,
) -> i32;

struct Bridge {
    pack: Vec<u16>,
    read: Read,
    detailed: Option<Detailed>,
}

/// The bridge's largest decoded block.
const MOST: u32 = 4 << 20;

#[derive(Clone, Copy)]
#[repr(usize)]
enum Count {
    Delivered,
    DeliveredBytes,
    LaneBusy,
    NotInPack,
    Disabled,
    Invalid,
    Size,
    NoGpu,
    Failed,
    NoDevice,
    Other,
}
const NAMES: [&str; 11] = [
    "delivered", "delivered_mb", "lane_busy", "not_in_pack", "disabled", "invalid",
    "size", "no_gpu", "failed", "no_device", "other",
];
static COUNTS: [AtomicU64; 11] = [const { AtomicU64::new(0) }; 11];
static LAST_REPORT: AtomicU64 = AtomicU64::new(0);

fn count(which: Count, by: u64) -> u64 {
    COUNTS[which as usize].fetch_add(by, Ordering::Relaxed) + by
}

fn report(force: bool) {
    static START: OnceLock<std::time::Instant> = OnceLock::new();
    let now = START.get_or_init(std::time::Instant::now).elapsed().as_secs();
    let last = LAST_REPORT.load(Ordering::Relaxed);
    if !force && now < last + 30 {
        return;
    }
    if LAST_REPORT.compare_exchange(last, now.max(1), Ordering::Relaxed, Ordering::Relaxed).is_err() && !force {
        return;
    }
    let mut text = String::new();
    for (i, name) in NAMES.iter().enumerate() {
        let mut v = COUNTS[i].load(Ordering::Relaxed);
        if i == Count::DeliveredBytes as usize {
            v >>= 20;
        }
        use std::fmt::Write as _;
        let _ = write!(text, "{name}={v} ");
    }
    tracing::info!(target: "victory_hle::gdeflate", "gdeflate: {}", text.trim_end());
}

fn wide(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(Some(0)).collect()
}

fn bridge() -> Option<&'static Bridge> {
    static BRIDGE: OnceLock<Option<Bridge>> = OnceLock::new();
    BRIDGE
        .get_or_init(|| {
            let pack = std::env::var_os("VICTORY_GDEFLATE_PACK").filter(|p| !p.is_empty())?;
            let bridge = load(PathBuf::from(pack));
            if bridge.is_none() {
                tracing::warn!("gdeflate: the bridge could not be loaded; the CPU decoders serve every request");
            }
            bridge
        })
        .as_ref()
}

fn load(pack: PathBuf) -> Option<Bridge> {
    let pack = pack.canonicalize().inspect_err(|e| tracing::warn!(error = %e, "gdeflate: pack not found")).ok()?;
    let dir = match std::env::var_os("VICTORY_GDEFLATE_BRIDGE_DIR").filter(|p| !p.is_empty()) {
        Some(dir) => PathBuf::from(dir).canonicalize().ok()?,
        None => pack.parent()?.to_owned(),
    };
    let search = LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32;
    // The signed DirectStorage runtime first, from the bridge folder, so the
    // bridge binds to that copy and not to one found elsewhere.
    let core = wide(dir.join("dstoragecore.dll").as_os_str());
    // SAFETY: a full path and a valid search mode.
    if unsafe { LoadLibraryExW(core.as_ptr(), std::ptr::null_mut(), search) }.is_null() {
        tracing::warn!(error = %std::io::Error::last_os_error(), "gdeflate: dstoragecore.dll unavailable");
        return None;
    }
    let dll = wide(dir.join("fx1-storage.dll").as_os_str());
    // SAFETY: as above. The module stays loaded for the process's lifetime.
    let handle = unsafe { LoadLibraryExW(dll.as_ptr(), std::ptr::null_mut(), search) };
    if handle.is_null() {
        tracing::warn!(error = %std::io::Error::last_os_error(), "gdeflate: fx1-storage.dll unavailable");
        return None;
    }
    // SAFETY: these exact signatures are exported by Neptune's
    // tools/gdeflate/runtime.cpp.
    let read: Read = unsafe { std::mem::transmute(GetProcAddress(handle, c"fx1_read_gdeflate".as_ptr().cast())?) };
    let detailed = unsafe { GetProcAddress(handle, c"fx1_read_gdeflate_detailed_v1".as_ptr().cast()) }
        .map(|entry| unsafe { std::mem::transmute::<_, Detailed>(entry) });
    tracing::info!(target: "victory_hle::gdeflate", pack = %pack.display(), bridge = %dir.display(), detailed = detailed.is_some(), "gdeflate: bridge loaded");
    Some(Bridge { pack: wide(pack.as_os_str()), read, detailed })
}

/// Whether a pack was asked for, without loading anything.
pub(super) fn requested() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_GDEFLATE_PACK").is_some_and(|p| !p.is_empty()))
}

/// Serves a compressed request from the pack. `None`: not delivered, the
/// caller decodes `source` as before. `Some`: delivered, with the result of
/// writing it to the title's destination.
pub(super) fn serve(
    request: &Request,
    source: &[u8],
    flags: u32,
    write: impl FnOnce(&[u8]) -> Result<(), HRESULT>,
) -> Option<Result<(), HRESULT>> {
    let bridge = bridge()?;
    let size = request.destination_size;
    if size == 0 || size > MOST {
        count(Count::Size, 1);
        report(false);
        return None;
    }
    let Some(device) = victory_gpu::storage_device() else {
        count(Count::NoDevice, 1);
        return None;
    };
    thread_local! { static SCRATCH: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) }; }
    SCRATCH.with_borrow_mut(|scratch| {
        scratch.resize(size as usize, 0);
        let mut support = 0u32;
        // SAFETY: `source` and `scratch` hold the lengths passed; the device
        // lives as long as the process; no write callback, no marker origin.
        let status = unsafe {
            match bridge.detailed {
                Some(read) => read(
                    device, bridge.pack.as_ptr(), source.as_ptr(), source.len() as u32, size, flags,
                    scratch.as_mut_ptr(), &mut support, None, std::ptr::null(),
                ),
                None => (bridge.read)(
                    device, bridge.pack.as_ptr(), source.as_ptr(), source.len() as u32, size, flags,
                    scratch.as_mut_ptr(), &mut support,
                ),
            }
        };
        if status != 1 {
            let which = match status {
                s if s < 0 => {
                    tracing::warn!(hr = format_args!("{:#x}", s as u32), "gdeflate: bridge failed; the CPU decoder serves this request");
                    Count::Failed
                }
                0 if bridge.detailed.is_none() => Count::NotInPack,
                2 => Count::LaneBusy,
                3 => Count::NotInPack,
                4 => Count::Disabled,
                5 => Count::Invalid,
                6 => Count::Size,
                7 => Count::NoGpu,
                _ => Count::Other,
            };
            count(which, 1);
            report(false);
            return None;
        }
        let result = write(scratch);
        if result.is_ok() {
            count(Count::DeliveredBytes, u64::from(size));
            let n = count(Count::Delivered, 1);
            report(n.is_power_of_two() && n >= 1024);
        }
        Some(result)
    })
}
