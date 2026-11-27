//! (Neptune, ported to the merged Scarlett Fox build) A performance overlay: a small rounded,
//! translucent panel (per-pixel alpha through `UpdateLayeredWindow`) in a click-through, topmost
//! tool window at the game window's client-area top-left, on a thread of its own, refreshed twice
//! a second. Being a window of its own, it is never part of the picture a frame generator
//! interpolates. `VICTORY_OVERLAY=0`: off. F10 shows and hides it while one of this process's
//! windows is in front. Not for exclusive fullscreen (the swap chain is windowed, flip model, so a
//! topmost window above it is composed as usual).
//!
//! (merged) Real frames are the title's: our presents ([`presented`]). Frames shown on screen are
//! the swap chain's own present count (`GetFrameStatistics`), which counts what a frame generator
//! behind the swap chain (OptiScaler's) presents in between; with frame generation asked for
//! (`VICTORY_OVERLAY_FG=<multiple>`) and no such count to be had, the shown rate is the real one
//! times the multiple, marked as an estimate.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, Once};
use std::time::Instant;

use windows::Win32::Graphics::Dxgi::{
    DXGI_FRAME_STATISTICS, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, DXGI_QUERY_VIDEO_MEMORY_INFO, IDXGIAdapter3,
    IDXGISwapChain3,
};
use windows_sys::Win32::Foundation::{FILETIME, HWND, POINT, RECT, SIZE};
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX};
use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetCurrentProcessId, GetSystemTimes};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::pixels::{Bitmap, short_cpu, short_gpu};

/// Our presents (the title's real frames), and the swap chain's present count as last read.
static PRESENTS: AtomicU64 = AtomicU64::new(0);
static SHOWN: AtomicU64 = AtomicU64::new(0);
/// Whether the swap chain's count was ever read.
static SHOWN_KNOWN: AtomicBool = AtomicBool::new(false);
/// Frames DLSS Frame Generation says it put on screen (`crate::streamline`), when it runs: the
/// exact count, used in place of the others.
static FG_PRESENTED: AtomicU64 = AtomicU64::new(0);
static FG_COUNTED: AtomicBool = AtomicBool::new(false);

/// DLSS Frame Generation put `n` frames on screen for one of ours (its own and the generated).
pub(crate) fn framegen_presented(n: u32) {
    FG_PRESENTED.fetch_add(u64::from(n), Ordering::Relaxed);
    FG_COUNTED.store(true, Ordering::Relaxed);
}
/// The newest machine sample.
static STATS: Mutex<Option<Stats>> = Mutex::new(None);
static VISIBLE: AtomicBool = AtomicBool::new(true);

const VK_F10: i32 = 0x79;
const REFRESH: usize = 1;
const KEYS: usize = 2;

/// Called after each of our presents, on the presenting thread: counts the real frame and, at
/// most every 100 ms, reads the swap chain's present count.
pub(crate) fn presented(swap_chain: &IDXGISwapChain3) {
    PRESENTS.fetch_add(1, Ordering::Relaxed);
    static LAST: Mutex<Option<Instant>> = Mutex::new(None);
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    if last.is_some_and(|t| t.elapsed().as_millis() < 100) {
        return;
    }
    *last = Some(Instant::now());
    let mut stats = DXGI_FRAME_STATISTICS::default();
    // SAFETY: a plain query on our live swap chain, on the thread that presents to it.
    if unsafe { swap_chain.GetFrameStatistics(&mut stats) }.is_ok() && stats.PresentCount > 0 {
        SHOWN.store(u64::from(stats.PresentCount), Ordering::Relaxed);
        SHOWN_KNOWN.store(true, Ordering::Relaxed);
    }
}

/// The frame generator's multiple asked for (`VICTORY_OVERLAY_FG`), 1 without one.
fn fg_multiple() -> f64 {
    static M: std::sync::OnceLock<f64> = std::sync::OnceLock::new();
    *M.get_or_init(|| {
        std::env::var("VICTORY_OVERLAY_FG").ok().and_then(|v| v.trim().parse::<f64>().ok()).filter(|m| *m >= 1.0).unwrap_or(1.0)
    })
}

/// The meter between two refreshes: real and shown frame rates, ms per real frame, draws per frame.
struct Meter {
    at: Instant,
    presents: u64,
    shown: u64,
    fg: u64,
    drawn: u64,
    /// One-second real rates, for the 30-second figures.
    second: (Instant, u64),
}

impl Meter {
    fn new() -> Self {
        let now = Instant::now();
        let presents = PRESENTS.load(Ordering::Relaxed);
        Meter {
            at: now,
            presents,
            shown: SHOWN.load(Ordering::Relaxed),
            fg: FG_PRESENTED.load(Ordering::Relaxed),
            drawn: crate::draw::drawn(),
            second: (now, presents),
        }
    }

    fn read(&mut self) -> Frames {
        let now = Instant::now();
        let dt = now.duration_since(self.at).as_secs_f64().max(1e-3);
        let (presents, shown, drawn) = (PRESENTS.load(Ordering::Relaxed), SHOWN.load(Ordering::Relaxed), crate::draw::drawn());
        let frames = presents.saturating_sub(self.presents);
        let real = frames as f64 / dt;
        let measured = SHOWN_KNOWN.load(Ordering::Relaxed).then(|| shown.saturating_sub(self.shown) as f64 / dt);
        let fg_now = FG_PRESENTED.load(Ordering::Relaxed);
        let fg_rate = fg_now.saturating_sub(self.fg) as f64 / dt;
        self.fg = fg_now;
        let draws = if frames > 0 { drawn.saturating_sub(self.drawn) as f64 / frames as f64 } else { 0.0 };
        (self.at, self.presents, self.shown, self.drawn) = (now, presents, shown, drawn);
        let elapsed = now.duration_since(self.second.0).as_secs_f64();
        if elapsed >= 1.0 {
            let rate = presents.saturating_sub(self.second.1) as f64 / elapsed;
            self.second = (now, presents);
            let mut rates = RATES.lock().unwrap_or_else(|e| e.into_inner());
            rates.push_back(rate);
            while rates.len() > 30 {
                rates.pop_front();
            }
        }
        let m = fg_multiple();
        if FG_COUNTED.load(Ordering::Relaxed) {
            // DLSS Frame Generation's own count of what it put on screen.
            let shown = fg_rate.max(real);
            return Frames { real, shown, estimated: false, ms: if real > 0.0 { 1000.0 / real } else { 0.0 }, draws, fg: true };
        }
        // The swap chain's count, if it shows a frame generator at work; else, with one asked for,
        // the estimate.
        let (shown, estimated) = match measured {
            Some(s) if m > 1.0 && s > real * 1.25 => (s, false),
            _ if m > 1.0 => (real * m, true),
            _ => (real, false),
        };
        Frames { real, shown, estimated, ms: if real > 0.0 { 1000.0 / real } else { 0.0 }, draws, fg: m > 1.0 }
    }
}

#[derive(Clone, Copy, Default)]
struct Frames {
    real: f64,
    shown: f64,
    estimated: bool,
    ms: f64,
    draws: f64,
    fg: bool,
}

/// What the panel shows.
#[derive(Clone, Default)]
pub(crate) struct Stats {
    frames: Frames,
    cpu: f64,
    gpu: Option<f64>,
    /// This process's working set, system memory used and total, in GB.
    ram: (f64, f64, f64),
    /// Video memory used and budget, in GB.
    vram: Option<(f64, f64)>,
    cpu_name: String,
    gpu_name: String,
    /// Of the last thirty one-second real frame rates: the highest, the lowest and the mean (none
    /// before there are two).
    half_minute: Option<(f64, f64, f64)>,
}

/// The last thirty one-second real frame rates.
static RATES: Mutex<std::collections::VecDeque<f64>> = Mutex::new(std::collections::VecDeque::new());

/// The highest, lowest and mean of the last thirty seconds' real frame rates.
fn half_minute() -> Option<(f64, f64, f64)> {
    let rates = RATES.lock().unwrap_or_else(|e| e.into_inner());
    (rates.len() >= 2).then(|| {
        let (high, low) = rates.iter().fold((f64::MIN, f64::MAX), |(h, l), &r| (h.max(r), l.min(r)));
        (high, low, rates.iter().sum::<f64>() / rates.len() as f64)
    })
}

/// Starts the overlay thread for the game window `hwnd` (the first call only).
pub(crate) fn start(hwnd: windows::Win32::Foundation::HWND, adapter: Option<IDXGIAdapter3>) {
    static ONCE: Once = Once::new();
    if std::env::var_os("VICTORY_OVERLAY").is_some_and(|v| v == "0") {
        return;
    }
    let game = hwnd.0 as usize;
    ONCE.call_once(|| {
        let spawned = std::thread::Builder::new()
            .name("victory-overlay".into())
            .spawn(move || run(game as HWND, adapter));
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "overlay thread not started");
        }
    });
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

fn ft(t: FILETIME) -> u64 {
    u64::from(t.dwHighDateTime) << 32 | u64::from(t.dwLowDateTime)
}

/// System CPU load since the last call, in percent.
struct Cpu(u64, u64);
impl Cpu {
    fn read() -> (u64, u64) {
        let z = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
        let (mut idle, mut kernel, mut user) = (z, z, z);
        // SAFETY: plain out-parameters.
        unsafe { GetSystemTimes(&mut idle, &mut kernel, &mut user) };
        // Kernel time includes idle.
        (ft(idle), ft(kernel) + ft(user))
    }
    fn percent(&mut self) -> f64 {
        let (idle, total) = Cpu::read();
        let (di, dt) = (idle.saturating_sub(self.0), total.saturating_sub(self.1));
        *self = Cpu(idle, total);
        if dt == 0 { 0.0 } else { (dt - di.min(dt)) as f64 * 100.0 / dt as f64 }
    }
}

/// The 3D engines' load summed over the instances (PDH, English names so any Windows language works).
struct Gpu3d(*mut std::ffi::c_void, *mut std::ffi::c_void);
impl Gpu3d {
    fn new() -> Option<Self> {
        use windows_sys::Win32::System::Performance::*;
        let (mut query, mut counter) = (std::ptr::null_mut(), std::ptr::null_mut());
        let path = wide(r"\GPU Engine(*engtype_3D)\Utilization Percentage");
        // SAFETY: out handles and a NUL-terminated path.
        unsafe {
            if PdhOpenQueryW(std::ptr::null(), 0, &mut query) != 0 {
                return None;
            }
            if PdhAddEnglishCounterW(query, path.as_ptr(), 0, &mut counter) != 0 {
                PdhCloseQuery(query);
                return None;
            }
            PdhCollectQueryData(query);
        }
        Some(Gpu3d(query, counter))
    }
    fn percent(&self) -> Option<f64> {
        use windows_sys::Win32::System::Performance::*;
        // SAFETY: the sizing call, then a buffer of the size it named (8-aligned).
        unsafe {
            if PdhCollectQueryData(self.0) != 0 {
                return None;
            }
            let (mut size, mut count) = (0u32, 0u32);
            PdhGetFormattedCounterArrayW(self.1, PDH_FMT_DOUBLE, &mut size, &mut count, std::ptr::null_mut());
            if size == 0 {
                return None;
            }
            let mut buf = vec![0u64; (size as usize).div_ceil(8)];
            let items = buf.as_mut_ptr().cast::<PDH_FMT_COUNTERVALUE_ITEM_W>();
            if PdhGetFormattedCounterArrayW(self.1, PDH_FMT_DOUBLE, &mut size, &mut count, items) != 0 {
                return None;
            }
            let sum = (0..count as usize)
                .map(|i| &*items.add(i))
                .filter(|item| item.FmtValue.CStatus == 0 || item.FmtValue.CStatus == 1)
                .map(|item| item.FmtValue.Anonymous.doubleValue)
                .sum::<f64>();
            Some(sum.clamp(0.0, 100.0))
        }
    }
}

fn cpu_name() -> String {
    use windows_sys::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW};
    let key = wide(r"HARDWARE\DESCRIPTION\System\CentralProcessor\0");
    let value = wide("ProcessorNameString");
    let mut buf = [0u16; 128];
    let mut bytes = std::mem::size_of_val(&buf) as u32;
    // SAFETY: a buffer of the size passed.
    let rc = unsafe {
        RegGetValueW(HKEY_LOCAL_MACHINE, key.as_ptr(), value.as_ptr(), RRF_RT_REG_SZ, std::ptr::null_mut(), buf.as_mut_ptr().cast(), &mut bytes)
    };
    if rc != 0 {
        return "CPU".into();
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len]).trim().to_string()
}

const MB: f64 = 1024.0 * 1024.0;
const GB: f64 = MB * 1024.0;

fn sample(cpu: &mut Cpu, gpu: Option<&Gpu3d>, adapter: Option<&IDXGIAdapter3>, names: &(String, String), meter: &mut Meter) -> Stats {
    // SAFETY: plain structs filled by the calls.
    let (working, used, total) = unsafe {
        let mut pm = std::mem::zeroed::<PROCESS_MEMORY_COUNTERS_EX>();
        pm.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
        GetProcessMemoryInfo(GetCurrentProcess(), std::ptr::from_mut(&mut pm).cast(), pm.cb);
        let mut ms = std::mem::zeroed::<MEMORYSTATUSEX>();
        ms.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        GlobalMemoryStatusEx(&mut ms);
        (pm.WorkingSetSize as f64, (ms.ullTotalPhys - ms.ullAvailPhys) as f64, ms.ullTotalPhys as f64)
    };
    let mut vram = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
    // SAFETY: a plain query filling our struct.
    let vram = match adapter.map(|a| unsafe { a.QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, &mut vram) }) {
        Some(Ok(())) => Some((vram.CurrentUsage as f64 / GB, vram.Budget as f64 / GB)),
        _ => None,
    };
    Stats {
        frames: meter.read(),
        half_minute: half_minute(),
        cpu: cpu.percent(),
        gpu: gpu.and_then(Gpu3d::percent),
        ram: (working / GB, used / GB, total / GB),
        vram,
        cpu_name: names.0.clone(),
        gpu_name: names.1.clone(),
    }
}

/// Whether a window of this process is in front.
fn ours_in_front() -> bool {
    let mut pid = 0u32;
    // SAFETY: plain queries.
    unsafe {
        let front = GetForegroundWindow();
        GetWindowThreadProcessId(front, &mut pid);
        !front.is_null() && pid == GetCurrentProcessId()
    }
}

/// The panel's gap to the window corner, and its scale, for a frame `h` pixels tall (the design is
/// made for 1080).
pub(crate) fn scale(h: usize) -> f32 {
    (h as f32 / 1080.0).clamp(0.75, 4.0)
}

pub(crate) fn margin(h: usize) -> usize {
    (8.0 * scale(h)).round() as usize
}

const FACE: &str = "Segoe UI";
const BG: [u8; 3] = [12, 13, 16];
const DIM: [u8; 3] = [140, 146, 158];
const DIMMER: [u8; 3] = [104, 110, 122];
const WHITE: [u8; 3] = [240, 242, 246];
/// Font slots: big number, semibold value, regular row, small hardware line.
const BIG: usize = 0;
const VALUE: usize = 1;
const ROW: usize = 2;
const SMALL: usize = 3;

/// GDI as a glyph rasteriser only: each run is drawn white on black into a scratch DIB, and its
/// grey level becomes the coverage the pure canvas blends in the run's colour.
struct Gdi {
    dc: HDC,
    fonts: [HFONT; 4],
    /// Ascent of each font, in pixels.
    ascent: [i32; 4],
}

impl Gdi {
    fn new(s: f32) -> Option<Gdi> {
        let face = wide(FACE);
        let make = |px: f32, weight: i32| {
            // SAFETY: a plain font creation.
            unsafe { CreateFontW(-(px * s).round() as i32, 0, 0, 0, weight, 0, 0, 0, 1, 0, 0, ANTIALIASED_QUALITY as u32, 0, face.as_ptr()) }
        };
        // SAFETY: a memory DC compatible with the screen.
        let dc = unsafe { CreateCompatibleDC(std::ptr::null_mut()) };
        let fonts = [make(34.0, 600), make(15.0, 600), make(14.0, 400), make(12.5, 400)];
        let mut g = Gdi { dc, fonts, ascent: [0; 4] };
        if dc.is_null() || fonts.iter().any(|f| f.is_null()) {
            return None; // Drop frees what exists.
        }
        for i in 0..4 {
            // SAFETY: live DC and font; a plain struct filled by the call.
            unsafe {
                SelectObject(dc, g.fonts[i]);
                let mut tm = std::mem::zeroed::<TEXTMETRICW>();
                GetTextMetricsW(dc, &mut tm);
                g.ascent[i] = tm.tmAscent;
            }
        }
        Some(g)
    }

    fn measure(&self, font: usize, text: &str) -> (i32, i32) {
        let t: Vec<u16> = text.encode_utf16().collect();
        let mut size = SIZE { cx: 0, cy: 0 };
        // SAFETY: live DC and font, a slice and its length.
        unsafe {
            SelectObject(self.dc, self.fonts[font]);
            GetTextExtentPoint32W(self.dc, t.as_ptr(), t.len() as i32, &mut size);
        }
        (size.cx, size.cy)
    }

    /// Draws `text` with its top-left at (x, y) in `rgb`.
    fn draw(&self, canvas: &mut Bitmap, font: usize, text: &str, x: i32, y: i32, rgb: [u8; 3]) {
        let t: Vec<u16> = text.encode_utf16().collect();
        let (w, h) = self.measure(font, text);
        let (w, h) = (w + 2, h);
        if w <= 2 || h <= 0 {
            return;
        }
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                biSizeImage: 0,
                biXPelsPerMeter: 0,
                biYPelsPerMeter: 0,
                biClrUsed: 0,
                biClrImportant: 0,
            },
            bmiColors: [RGBQUAD { rgbBlue: 0, rgbGreen: 0, rgbRed: 0, rgbReserved: 0 }],
        };
        let mut bits = std::ptr::null_mut();
        // SAFETY: a DIB section of w x h x 4 bytes (zeroed: black) selected for the text, read after
        // GdiFlush, and freed here.
        unsafe {
            let dib = CreateDIBSection(self.dc, &info, DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0);
            if dib.is_null() || bits.is_null() {
                return;
            }
            let old = SelectObject(self.dc, dib);
            SelectObject(self.dc, self.fonts[font]);
            SetBkMode(self.dc, TRANSPARENT as i32);
            SetTextColor(self.dc, 0x00ff_ffff);
            TextOutW(self.dc, 0, 0, t.as_ptr(), t.len() as i32);
            GdiFlush();
            let grey = std::slice::from_raw_parts(bits.cast::<u8>(), (w * h) as usize * 4);
            for row in 0..h {
                for col in 0..w {
                    let (px, py) = (x + col, y + row);
                    if px >= 0 && py >= 0 {
                        // Green channel: grey-scale anti-aliasing leaves all three equal.
                        canvas.over(px as usize, py as usize, rgb, u32::from(grey[((row * w + col) * 4 + 1) as usize]));
                    }
                }
            }
            SelectObject(self.dc, old);
            DeleteObject(dib);
        }
    }
}

impl Drop for Gdi {
    fn drop(&mut self) {
        // SAFETY: objects this value made.
        unsafe {
            for f in self.fonts {
                if !f.is_null() {
                    DeleteObject(f);
                }
            }
            if !self.dc.is_null() {
                DeleteDC(self.dc);
            }
        }
    }
}

fn fps_colour(fps: f64) -> [u8; 3] {
    if fps >= 55.0 {
        [96, 220, 130]
    } else if fps >= 30.0 {
        [250, 204, 70]
    } else {
        [255, 98, 88]
    }
}

/// Draws the panel for a frame `frame_h` pixels tall: a premultiplied RGBA bitmap, sized to its
/// content. The overlay window and the screenshot both call this, so they look alike.
pub(crate) fn render(stats: &Stats, frame_h: usize) -> Option<Bitmap> {
    let s = scale(frame_h);
    let px = |v: f32| (v * s).round() as i32;
    let g = Gdi::new(s)?;
    let (pad, gap) = (px(12.0), px(10.0));
    let pct = |p: f64| format!("{p:.0}%");
    let rate = |r: f64| if r < 10.0 { format!("{r:.1} fps") } else { format!("{r:.0} fps") };
    let f = stats.frames;
    let mut rows: Vec<(&str, String)> = Vec::new();
    if f.fg {
        // Frame generation: what is on screen above, the title's own frames and the generated
        // ones here (an estimate when the swap chain did not show the generator's frames).
        rows.push(("Real", format!("{:.1} fps", f.real)));
        let generated = (f.shown - f.real).max(0.0);
        rows.push(("Generated", format!("{}{generated:.0} fps", if f.estimated { "~" } else { "" })));
    }
    rows.extend([
        ("30 s high", stats.half_minute.map_or("...".into(), |(high, _, _)| rate(high))),
        ("30 s low", stats.half_minute.map_or("...".into(), |(_, low, _)| rate(low))),
        ("30 s avg", stats.half_minute.map_or("...".into(), |(_, _, mean)| format!("{mean:.1} fps"))),
        ("Draws", format!("{:.0} / frame", f.draws)),
        ("CPU", pct(stats.cpu)),
        ("GPU", stats.gpu.map_or("n/a".into(), pct)),
        ("RAM", format!("{:.1} GB  \u{b7}  {:.1} / {:.0} GB", stats.ram.0, stats.ram.1, stats.ram.2)),
        ("VRAM", stats.vram.map_or("n/a".into(), |(u, b)| format!("{u:.1} / {b:.1} GB"))),
    ]);
    let hardware = [
        short_cpu(&stats.cpu_name),
        short_gpu(&stats.gpu_name),
        format!("{:.0} GB RAM  \u{b7}  build {}", stats.ram.2.round(), option_env!("VICTORY_BUILD").unwrap_or("?")),
    ];
    // The big number: frames on screen (with frame generation, its frames too); the frame time
    // beside it is the title's real frame's.
    let fps_text = format!("{:.0}", f.shown);
    let unit = match (f.fg, f.estimated) {
        (false, _) => "FPS",
        (true, false) => "FPS (FG)",
        (true, true) => "FPS (FG est.)",
    };
    let ms_text = format!("{:.1} ms", f.ms);
    // Width: the widest line, with at least a comfortable minimum.
    let top = g.measure(BIG, &fps_text).0 + px(6.0) + g.measure(SMALL, unit).0 + gap * 2 + g.measure(VALUE, &ms_text).0;
    let body = rows.iter().map(|(l, v)| g.measure(ROW, l).0 + gap * 2 + g.measure(ROW, v).0);
    let foot = hardware.iter().map(|t| g.measure(SMALL, t).0);
    let inner = body.chain(foot).chain([top]).max().unwrap_or(0).max(px(190.0));
    let (w, row_h) = (inner + 2 * pad, g.measure(ROW, "Ag").1 + px(3.0));
    let big_h = g.measure(BIG, "0").1;
    let small_h = g.measure(SMALL, "Ag").1 + px(1.0);
    let sep = px(9.0);
    let h = pad + big_h + sep + rows.len() as i32 * row_h + sep + hardware.len() as i32 * small_h + pad - px(1.0);
    let mut bm = Bitmap::new(w as usize, h as usize);
    bm.fill_round((0, 0, w as usize, h as usize), 10.0 * s, BG, 218);

    let right = |font, text: &str| w - pad - g.measure(font, text).0;
    let mut y = pad;
    // The big number with FPS beside it on the same baseline; the frame time at the right edge.
    g.draw(&mut bm, BIG, &fps_text, pad, y, fps_colour(f.shown));
    let nx = pad + g.measure(BIG, &fps_text).0 + px(6.0);
    g.draw(&mut bm, SMALL, unit, nx, y + g.ascent[BIG] - g.ascent[SMALL], DIM);
    g.draw(&mut bm, VALUE, &ms_text, right(VALUE, &ms_text), y + g.ascent[BIG] - g.ascent[VALUE], WHITE);
    y += big_h + sep / 2;
    let line = |bm: &mut Bitmap, y: i32| {
        for x in pad..w - pad {
            bm.over(x as usize, y as usize, [255, 255, 255], 24);
        }
    };
    line(&mut bm, y);
    y += sep - sep / 2;
    for (label, value) in &rows {
        g.draw(&mut bm, ROW, label, pad, y, DIM);
        g.draw(&mut bm, ROW, value, right(ROW, value), y, WHITE);
        y += row_h;
    }
    y += sep / 2 - 1;
    line(&mut bm, y);
    y += sep - sep / 2 + 1;
    for (i, t) in hardware.iter().enumerate() {
        g.draw(&mut bm, SMALL, t, pad, y, if i < 2 { DIM } else { DIMMER });
        y += small_h;
    }
    Some(bm)
}

/// Sets the layered window's picture and position in one go.
///
/// # Safety
/// `window` is a live layered window of this thread.
unsafe fn present(window: HWND, bm: &Bitmap, at: POINT) {
    // SAFETY: a 32-bit top-down DIB the size of the bitmap, filled with its premultiplied pixels
    // as BGRA, handed to UpdateLayeredWindow, then freed.
    unsafe {
        let screen = GetDC(std::ptr::null_mut());
        let dc = CreateCompatibleDC(screen);
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: bm.w as i32,
                biHeight: -(bm.h as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                biSizeImage: 0,
                biXPelsPerMeter: 0,
                biYPelsPerMeter: 0,
                biClrUsed: 0,
                biClrImportant: 0,
            },
            bmiColors: [RGBQUAD { rgbBlue: 0, rgbGreen: 0, rgbRed: 0, rgbReserved: 0 }],
        };
        let mut bits = std::ptr::null_mut();
        let dib = CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0);
        if !dib.is_null() && !bits.is_null() {
            let dst = std::slice::from_raw_parts_mut(bits.cast::<u8>(), bm.px.len());
            for (d, s) in dst.chunks_exact_mut(4).zip(bm.px.chunks_exact(4)) {
                d.copy_from_slice(&[s[2], s[1], s[0], s[3]]);
            }
            let old = SelectObject(dc, dib);
            let size = SIZE { cx: bm.w as i32, cy: bm.h as i32 };
            let from = POINT { x: 0, y: 0 };
            let blend = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: 255, AlphaFormat: AC_SRC_ALPHA as u8 };
            UpdateLayeredWindow(window, screen, &at, &size, dc, &from, 0, &blend, ULW_ALPHA);
            SelectObject(dc, old);
            DeleteObject(dib);
        }
        DeleteDC(dc);
        ReleaseDC(std::ptr::null_mut(), screen);
    }
}

unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wp: usize, lp: isize) -> isize {
    // SAFETY: the standard handlers; the picture is set by `present`, not by painting.
    unsafe {
        match msg {
            WM_NCHITTEST => HTTRANSPARENT as isize,
            WM_MOUSEACTIVATE => MA_NOACTIVATE as isize,
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

fn run(game: HWND, adapter: Option<IDXGIAdapter3>) {
    let gpu_name = adapter
        .as_ref()
        // SAFETY: a plain query.
        .and_then(|a| unsafe { a.GetDesc().ok() })
        .map(|d| String::from_utf16_lossy(&d.Description).trim_end_matches('\0').trim().to_string())
        .unwrap_or_else(|| "GPU".into());
    let names = (cpu_name(), gpu_name.clone());
    let class = wide("NeptuneOverlay");
    // SAFETY: window class and window made and used on this thread only.
    unsafe {
        let instance = GetModuleHandleW(std::ptr::null());
        let wc = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: instance,
            hIcon: std::ptr::null_mut(),
            hCursor: std::ptr::null_mut(),
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: class.as_ptr(),
        };
        RegisterClassW(&wc);
        let window = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            class.as_ptr(),
            class.as_ptr(),
            WS_POPUP,
            0,
            0,
            10,
            10,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            instance,
            std::ptr::null(),
        );
        if window.is_null() {
            tracing::warn!("overlay window not created");
            return;
        }
        tracing::info!(gpu = %gpu_name, "overlay on (VICTORY_OVERLAY=0 turns it off)");
        let mut cpu = Cpu(0, 0);
        cpu.percent();
        let gpu = Gpu3d::new();
        let mut f10 = false;
        let mut meter = Meter::new();
        SetTimer(window, REFRESH, 500, None);
        SetTimer(window, KEYS, 40, None);
        let mut refresh = |cpu: &mut Cpu| {
            let stats = sample(cpu, gpu.as_ref(), adapter.as_ref(), &names, &mut meter);
            *STATS.lock().unwrap_or_else(|e| e.into_inner()) = Some(stats.clone());
            let mut client = std::mem::zeroed::<RECT>();
            let mut origin = POINT { x: 0, y: 0 };
            let show = VISIBLE.load(Ordering::Relaxed)
                && IsWindow(game) != 0
                && IsIconic(game) == 0
                && IsWindowVisible(game) != 0
                && ours_in_front()
                && GetClientRect(game, &mut client) != 0
                && ClientToScreen(game, &mut origin) != 0;
            let Some(bm) = show.then(|| render(&stats, client.bottom.max(1) as usize)).flatten() else {
                ShowWindow(window, SW_HIDE);
                return;
            };
            let m = margin(client.bottom.max(1) as usize) as i32;
            let at = POINT { x: origin.x + m, y: origin.y + m };
            present(window, &bm, at);
            SetWindowPos(window, HWND_TOPMOST, at.x, at.y, bm.w as i32, bm.h as i32, SWP_NOACTIVATE | SWP_SHOWWINDOW);
        };
        refresh(&mut cpu);
        let mut msg = std::mem::zeroed::<MSG>();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            if msg.message == WM_TIMER && msg.hwnd == window {
                if msg.wParam == REFRESH {
                    refresh(&mut cpu);
                } else {
                    // Edge-triggered: acts on the press, not the hold.
                    let d10 = GetAsyncKeyState(VK_F10) < 0;
                    if ours_in_front() && d10 && !f10 {
                        VISIBLE.fetch_xor(true, Ordering::Relaxed);
                        refresh(&mut cpu);
                    }
                    f10 = d10;
                }
                continue;
            }
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}
