//! The title's FidelityFX upscaler path on Series X (`VICTORY_UPSCALER_PATH=<mode>`): the stock Series X image
//! upscales with its own temporal pass (renderer 6) and never calls FidelityFX, so a PC upscaler
//! (`VICTORY_FSR_DLL`, OptiScaler: DLSS, XeSS, FSR) has nothing to take over. Patched before the image is
//! mapped, the title renders through its FidelityFX path (renderer 7, as Series S does) with the scene at
//! `mode`'s scale of the output, and `victory_hle::modules::fidelityfx` hands those calls to the PC DLL.
//!
//! Ported from foxhound (readmet3xt-lab/foxhound, `apps/victory/src/native_1440p.rs`, `prepare_upscaler`,
//! GPL-3.0-or-later), itself from DexNom's fork (`prepare_fsr_4k`). Every patched instruction is checked
//! against the stock bytes of the retail build first; nothing is written if one differs.

use anyhow::{Context, Result, bail, ensure};
use victory_formats::texture::{Dimension, Swizzle, Texture, TextureDesc};
use victory_loader::PreparedImage;

const RETAIL_BUILD: &str = "b99f876b74d4a77f03af5f878223b27bd647b5c35a79d5decd9f716504239e93";

/// 2560x1440 SDR output patch constants (`VICTORY_NATIVE_1440P=1`, `VICTORY_RENDER_1080P=1`),
/// ported from `DexNom/VIctory` via `foxhound` / `VIctory-NVIDIA`.
pub(crate) const NATIVE_1440P_WIDTH: u32 = 2560;
pub(crate) const NATIVE_1440P_HEIGHT: u32 = 1440;
const SDR_MODE: u32 = 0x0615_c100;
const STOCK_SDR_MODE: [u8; 16] = [0, 15, 0, 0, 112, 8, 0, 0, 60, 0, 0, 0, 1, 0, 0, 0];
const SCANOUT_RESERVATION: u32 = 0x0247_ad4a;
const UI_VIEWPORT_LOAD: u32 = 0x0580_48d4;

/// Patches the Series X retail image for a native 2560x1440 SDR display output
/// (scene at 2560x1440, or 1920x1080 with `render_1080p`), eliminating the 4K
/// scanout & post-processing cost. Every byte is checked before patching.
pub(crate) fn prepare_1440p(image: &mut PreparedImage, render_1080p: bool) -> Result<(u32, u32)> {
    ensure!(
        image.original_hash.to_hex().as_str() == RETAIL_BUILD,
        "1440p output is only known for the retail build {RETAIL_BUILD}"
    );

    let mode = field::<16>(image, SDR_MODE)?;
    ensure!(*mode == STOCK_SDR_MODE, "unexpected Series X SDR mode");
    mode[..4].copy_from_slice(&NATIVE_1440P_WIDTH.to_le_bytes());
    mode[4..8].copy_from_slice(&NATIVE_1440P_HEIGHT.to_le_bytes());

    let texture = Texture::new(&TextureDesc {
        dimension: Dimension::Tex2d,
        width: NATIVE_1440P_WIDTH,
        height: NATIVE_1440P_HEIGHT,
        depth_or_array: 1,
        mips: 1,
        format: 28,
        samples: 1,
        swizzle: Swizzle::SW_64KB_D_X,
    })
    .map_err(|error| anyhow::anyhow!("1440p scanout layout: {error:?}"))?;
    let pool_size =
        u32::try_from((texture.size.next_multiple_of(64 * 1024) * 2).next_multiple_of(2 << 20))?;
    let reservation = field::<5>(image, SCANOUT_RESERVATION)?;
    ensure!(*reservation == [0xb9, 0, 0, 0, 4], "unexpected Series X scanout reservation");
    reservation[1..].copy_from_slice(&pool_size.to_le_bytes());

    let scale = field::<4>(image, TEMPORAL_SCALE)?;
    ensure!(*scale == (2.0f32 / 3.0).to_le_bytes(), "unexpected Series X temporal scene scale");
    let (render_width, render_height) =
        if render_1080p { (1920, 1080) } else { (NATIVE_1440P_WIDTH, NATIVE_1440P_HEIGHT) };
    *scale = (render_width as f32 / NATIVE_1440P_WIDTH as f32).to_le_bytes();

    let viewport = field::<8>(image, UI_VIEWPORT_LOAD)?;
    ensure!(
        *viewport == [0xc5, 0xfb, 0x10, 0x0d, 0x10, 0xf0, 0x13, 0x05],
        "unexpected Settings viewport size load"
    );
    let displacement = (SDR_MODE as i32) - (UI_VIEWPORT_LOAD as i32 + 8);
    viewport[4..].copy_from_slice(&displacement.to_le_bytes());
    Ok((render_width, render_height))
}

/// The title's own temporal pass's scene scale (2/3 on Series X: a 2560x1440 scene for a 4K output).
const TEMPORAL_SCALE: u32 = 0x028f_1e7e;
/// The renderer selection: `mov eax, 6; cmovne eax, ecx` (the temporal pass, or on Series S with the
/// upscaler there the FidelityFX path, 7).
const RENDERER_SELECTION: u32 = 0x007e_9421;
const STOCK_RENDERER_SELECTION: [u8; 8] = [0xb8, 6, 0, 0, 0, 0x0f, 0x45, 0xc1];
/// The FidelityFX path's render size: the console check's result (Series S) is noted as the FidelityFX
/// quality mode and picks the scaled branch (`movzx eax, al; mov [r14+0x90], eax; test al, al; jne`).
const UPSCALER_SIZING: u32 = 0x028e_f69a;
const STOCK_UPSCALER_SIZING: [u8; 14] = [0x0f, 0xb6, 0xc0, 0x41, 0x89, 0x86, 0x90, 0, 0, 0, 0x84, 0xc0, 0x75, 0x0a];
/// The native branch's load of 1.0, and the scaled branch's of the scale.
const NATIVE_LOAD: u32 = 0x028e_f6a8;
const STOCK_NATIVE_LOAD: [u8; 10] = [0xc5, 0xfa, 0x10, 0x05, 0x28, 0x78, 0x6d, 0x03, 0xeb, 0x08];
const SCALED_LOAD: u32 = 0x028e_f6b2;
const STOCK_SCALED_LOAD: [u8; 8] = [0xc5, 0xfa, 0x10, 0x05, 0xa2, 0xe2, 0x86, 0x03];
/// The scale the scaled branch loads (shared with other code: not written).
const UPSCALER_SCALE: u32 = 0x0615_d95c;

/// The scene's share of the output, with the FidelityFX quality mode of the same ratio.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    Native,
    Quality,
    Balanced,
    Performance,
    UltraPerformance,
}

impl Mode {
    pub(crate) fn parse(text: &str) -> Result<Self> {
        Ok(match text.trim().to_ascii_lowercase().as_str() {
            "native" | "dlaa" | "aa" => Mode::Native,
            "quality" | "1" => Mode::Quality,
            "balanced" => Mode::Balanced,
            "performance" => Mode::Performance,
            "ultra" | "ultraperformance" | "ultra-performance" => Mode::UltraPerformance,
            other => bail!("VICTORY_UPSCALER_PATH={other:?}: native, quality, balanced, performance or ultra"),
        })
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Mode::Native => "native",
            Mode::Quality => "quality",
            Mode::Balanced => "balanced",
            Mode::Performance => "performance",
            Mode::UltraPerformance => "ultra",
        }
    }

    pub(crate) fn scale(self) -> f32 {
        match self {
            Mode::Native => 1.0,
            Mode::Quality => 2.0 / 3.0,
            Mode::Balanced => 0.58,
            Mode::Performance => 0.5,
            Mode::UltraPerformance => 1.0 / 3.0,
        }
    }

    fn ffx_mode(self) -> u8 {
        match self {
            Mode::Native => 0,
            Mode::Quality => 1,
            Mode::Balanced => 2,
            Mode::Performance => 3,
            Mode::UltraPerformance => 4,
        }
    }
}

/// Makes the title render through its FidelityFX path at `mode`'s scale of the output.
pub(crate) fn prepare(image: &mut PreparedImage, mode: Mode) -> Result<()> {
    ensure!(
        image.original_hash.to_hex().as_str() == RETAIL_BUILD,
        "the FidelityFX path is only known for the retail build {RETAIL_BUILD}"
    );
    ensure!(*field::<8>(image, RENDERER_SELECTION)? == STOCK_RENDERER_SELECTION, "unexpected renderer selection");
    ensure!(*field::<14>(image, UPSCALER_SIZING)? == STOCK_UPSCALER_SIZING, "unexpected upscaler render size selection");
    ensure!(*field::<10>(image, NATIVE_LOAD)? == STOCK_NATIVE_LOAD, "unexpected upscaler native scale load");
    ensure!(*field::<8>(image, SCALED_LOAD)? == STOCK_SCALED_LOAD, "unexpected upscaler scaled load");
    ensure!(*field::<4>(image, UPSCALER_SCALE)? == (2.0f32 / 3.0).to_le_bytes(), "unexpected upscaler scale");
    let temporal = f32::from_le_bytes(*field::<4>(image, TEMPORAL_SCALE)?);
    ensure!(
        temporal == 2.0 / 3.0 || temporal == 1.0 || temporal == 0.75,
        "unexpected temporal scene scale ({temporal})"
    );
    // Renderer 7 every time it is asked (the console check has run).
    *field::<8>(image, RENDERER_SELECTION)? = [0xb8, 7, 0, 0, 0, 0x90, 0x90, 0x90];
    // `mov eax, mode; mov [r14+0x90], eax; jmp scaled`.
    *field::<14>(image, UPSCALER_SIZING)? = [0xb8, mode.ffx_mode(), 0, 0, 0, 0x41, 0x89, 0x86, 0x90, 0, 0, 0, 0xeb, 0x0a];
    if mode != Mode::Quality {
        // The jump leaves the native branch unreached: its load's operand becomes a literal of ours, which
        // the scaled branch loads.
        let literal = NATIVE_LOAD + 4;
        *field::<4>(image, literal)? = mode.scale().to_le_bytes();
        let displacement = literal as i32 - (SCALED_LOAD as i32 + 8);
        *field::<4>(image, SCALED_LOAD + 4)? = displacement.to_le_bytes();
    }
    *field::<4>(image, TEMPORAL_SCALE)? = mode.scale().to_le_bytes();
    Ok(())
}

fn field<const N: usize>(image: &mut PreparedImage, rva: u32) -> Result<&mut [u8; N]> {
    let offset = image.info.rva_to_file_offset(rva).context("upscaler path field missing")? as usize;
    image
        .bytes
        .get_mut(offset..offset + N)
        .context("incomplete upscaler path field")?
        .try_into()
        .context("invalid upscaler path field length")
}
