# Starts the title with this fork's settings (scripts\play-scarlett.ps1, Series X profile) plus:
#
#   .\Play.ps1 -Game 'D:\Games\MyDump'           # first start: your dump's folder (remembered afterwards)
#   .\Play.ps1 -Dlss                             # DLSS (Streamline) instead of the title's upscaler, 1440p -> 4K
#   .\Play.ps1 -Dlss -FrameGen -FgFrames 3       # ... with DLSS Frame Generation 4x (-FgFrames 1..5 = 2x..6x)
#   .\Play.ps1 -Dlss -DlssPreset M -Quality balanced
#   .\Play.ps1 -GDeflatePack 'D:\pack\textures.gdeflate'   # GPU GDeflate decompression (see README)
#   .\Play.ps1 -OptiScaler -Upscaler dlss         # DLSS through OptiScaler instead of Streamline
#   .\Play.ps1 -Fps 60                           # a frame-rate cap (default: unlocked; -1: the title's own 30)
#   .\Play.ps1 -Extra @{ VICTORY_LOG = 'warn' }  # any other VICTORY_* setting
#
# F10 shows or hides the stats overlay. The mouse turns the camera; Left Alt gives the mouse back.
param(
  # Your own dump (the folder with game_scarlett_master.exe); remembered after the first start.
  [string]$Game = '',
  # Shader and pipeline caches; default %LOCALAPPDATA%\victory-scarlett\cache.
  [string]$Cache = '',
  [ValidateSet('diag', 'release')][string]$Build = 'diag',
  # The title's frame rate: 0 (default) no limit, N a cap of N, -1 the title's own 30.
  [int]$Fps = 0,

  # DLSS and DLSS Frame Generation through NVIDIA Streamline (foxhound's way). NVIDIA's files are not part
  # of this repository: unpack the Streamline SDK and point -StreamlineDir at its bin\x64 folder.
  [switch]$Dlss,
  [string]$StreamlineDir = (Join-Path $PSScriptRoot 'streamline-sdk\bin\x64'),
  [switch]$FrameGen,
  # Frames generated between two real ones: 1 (2x) to 5 (6x), held to what the GPU says it can do.
  [ValidateRange(1, 5)][int]$FgFrames = 1,
  # DLSS model: L and M are DLSS 4.5's second-generation transformer, K DLSS 4's, auto the DLL's own pick.
  [ValidateSet('L', 'M', 'K', 'auto')][string]$DlssPreset = 'L',
  # The scene's share of the output (native = DLAA).
  [ValidateSet('native', 'quality', 'balanced', 'performance', 'ultra')][string]$Quality = 'quality',

  # Optional: GPU GDeflate (Neptune-GDeflate's verified pack and its fx1-storage bridge). Off without a pack.
  [string]$GDeflatePack = '',
  [string]$GDeflateBridge = '',
  [int]$WaitUs = 1000,
  [string]$Workers = 'physical',

  # Optional: OptiScaler in place of Streamline (its folder, with nvngx_dlss.dll for DLSS).
  [switch]$OptiScaler,
  [string]$OptiDir = (Join-Path $PSScriptRoot 'OptiScaler'),
  [ValidateSet('dlss', 'xess', 'fsr31', 'fsr21')][string]$Upscaler = 'dlss',
  [switch]$SeriesS,

  [switch]$NoHooks,
  [hashtable]$Extra = @{}
)
$ErrorActionPreference = 'Stop'
$victory = Join-Path $PSScriptRoot "target\$Build\victory.exe"
if (-not (Test-Path $victory)) { "No $victory. Build it first: cargo build --profile $Build -p victory --features diagnostics --locked"; exit 2 }

$more = @{
  # (cool-fox) 12 ms left a face of coloured noise in the first cutscene; 40 baked it whole, no slower frames.
  VICTORY_EARLY_TEXTURES_MS = '40'
}
if ($Workers) { $more.VICTORY_DSTORAGE_WORKERS = $Workers }
if ($Fps -eq 0) { $more.VICTORY_FRAME_INTERVAL_MS = '0' }   # no wait: the title's own "no limit"
elseif ($Fps -gt 0) { $more.VICTORY_FRAME_INTERVAL_MS = (1000.0 / $Fps).ToString('0.000', [Globalization.CultureInfo]::InvariantCulture) }

if ($GDeflatePack) {
  if (-not (Test-Path $GDeflatePack)) { "No GDeflate pack at $GDeflatePack"; exit 2 }
  if (-not $GDeflateBridge) { $GDeflateBridge = Split-Path -Parent (Resolve-Path $GDeflatePack) }
  foreach ($f in 'fx1-storage.dll', 'dstorage.dll', 'dstoragecore.dll') {
    if (-not (Test-Path (Join-Path $GDeflateBridge $f))) { "Missing $f in $GDeflateBridge"; exit 2 }
  }
  $more.VICTORY_GDEFLATE_PACK = (Resolve-Path $GDeflatePack).Path
  $more.VICTORY_GDEFLATE_BRIDGE_DIR = (Resolve-Path $GDeflateBridge).Path
  # The bridge's own settings: Neptune's tested defaults, with a longer lane wait (fewer CPU fallbacks).
  $more.VICTORY_GDEFLATE_SINGLE_SUBMIT = '1'; $more.VICTORY_GDEFLATE_LANES = '4'; $more.VICTORY_GDEFLATE_BATCH_US = '250'
  $more.VICTORY_GDEFLATE_BATCH_KIB = '512'; $more.VICTORY_GDEFLATE_WAIT_US = "$WaitUs"; $more.VICTORY_GDEFLATE_LRU = '1'
  $more.VICTORY_GDEFLATE_ROLLING = '0'
}

function Set-IniValue([string]$Text, [string]$Section, [string]$Key, [string]$Value) {
  $lines = $Text -split "`r?`n"; $in = $false; $done = $false
  for ($i = 0; $i -lt $lines.Count; $i++) {
    if ($lines[$i] -match '^\[(.+)\]\s*$') { $in = ($Matches[1] -eq $Section); continue }
    if ($in -and -not $done -and $lines[$i] -match ('^\s*' + [regex]::Escape($Key) + '\s*=')) { $lines[$i] = "$Key=$Value"; $done = $true }
  }
  if (-not $done) { Write-Warning "OptiScaler.ini: no $Key in [$Section]" }
  $lines -join "`r`n"
}

if ($Dlss -and $OptiScaler) { '-Dlss and -OptiScaler are two ways of the same thing: pick one.'; exit 2 }
if ($Dlss) {
  if (-not (Test-Path (Join-Path $StreamlineDir 'sl.interposer.dll'))) { "No sl.interposer.dll in $StreamlineDir (see README: NVIDIA Streamline SDK)"; exit 2 }
  $more.VICTORY_STREAMLINE_DIR = (Resolve-Path $StreamlineDir).Path
  $more.VICTORY_DLSS = '1'
  $more.VICTORY_DLSS_MODE = switch ($Quality) { 'native' { 'dlaa' } 'ultra' { 'ultraperformance' } default { $Quality } }
  if ($DlssPreset -ne 'auto') { $more.VICTORY_DLSS_PRESET = $DlssPreset.ToLowerInvariant() }
  if ($FrameGen) { $more.VICTORY_FRAMEGEN = '1'; $more.VICTORY_FRAMEGEN_FRAMES = "$FgFrames" }
}
if ($OptiScaler) {
  if (-not (Test-Path (Join-Path $OptiDir 'OptiScaler.dll'))) { "No OptiScaler.dll in $OptiDir"; exit 2 }
  # (as Neptune's launcher) A copy of victory.exe in a folder of its own with OptiScaler as its dxgi.dll.
  $to = Join-Path $PSScriptRoot "target\$Build\opti"
  New-Item -ItemType Directory -Force $to | Out-Null
  $stamp = Join-Path $to 'optiscaler.from'
  $from = (Get-Item (Join-Path $OptiDir 'OptiScaler.dll')).LastWriteTimeUtc.Ticks.ToString() + ' ' + $OptiDir
  if (-not (Test-Path $stamp) -or (Get-Content $stamp -Raw).Trim() -ne $from) {
    Get-ChildItem $OptiDir -File | Where-Object { $_.Extension -in '.dll', '.ini' } | Copy-Item -Destination $to -Force
    Get-ChildItem $OptiDir -Directory | Where-Object { $_.Name -ne 'Licenses' } | Copy-Item -Destination $to -Recurse -Force
    Copy-Item (Join-Path $OptiDir 'OptiScaler.dll') (Join-Path $to 'dxgi.dll') -Force
    Remove-Item (Join-Path $to 'OptiScaler.dll') -Force -ErrorAction SilentlyContinue
    Set-Content $stamp $from
  }
  if ($Upscaler -eq 'dlss' -and -not (Test-Path (Join-Path $to 'nvngx_dlss.dll'))) {
    Write-Warning "No nvngx_dlss.dll in $OptiDir : OptiScaler falls back to another upscaler (XeSS)."
  }
  $iniPath = Join-Path $to 'OptiScaler.ini'
  $ini = Get-Content -LiteralPath $iniPath -Raw
  $ini = Set-IniValue $ini 'Upscalers' 'Dx12Upscaler' $Upscaler
  $presetNumber = @{ K = '11'; L = '12'; M = '13' }[$DlssPreset]
  $ini = Set-IniValue $ini 'DLSS' 'RenderPresetOverride' $(if ($presetNumber) { 'true' } else { 'auto' })
  $ini = Set-IniValue $ini 'DLSS' 'RenderPresetForAll' $(if ($presetNumber) { $presetNumber } else { 'auto' })
  $ini = Set-IniValue $ini 'FrameGen' 'Enabled' ($FrameGen.IsPresent.ToString().ToLowerInvariant())
  if ($FrameGen) {
    $more.VICTORY_OVERLAY_FG = '2'   # OptiScaler's FSR frame generation shows two frames a real one
    $ini = Set-IniValue $ini 'FrameGen' 'FGInput' 'upscaler'
    $ini = Set-IniValue $ini 'FrameGen' 'FGOutput' 'fsrfg'
    $ini = Set-IniValue $ini 'FSRFG' 'AllowAsync' 'true'
  } else {
    $ini = Set-IniValue $ini 'FrameGen' 'FGInput' 'nofg'
    $ini = Set-IniValue $ini 'FrameGen' 'FGOutput' 'nofg'
  }
  Set-Content -LiteralPath $iniPath -Value $ini -NoNewline
  $copy = Join-Path $to 'victory.exe'
  if (-not (Test-Path $copy) -or (Get-Item $copy).LastWriteTimeUtc -ne (Get-Item $victory).LastWriteTimeUtc -or (Get-Item $copy).Length -ne (Get-Item $victory).Length) {
    Copy-Item $victory $copy -Force
  }
  $victory = $copy
  $more.VICTORY_FSR_DLL = Join-Path $to 'amd_fidelityfx_dx12.dll'
  if (-not $SeriesS) { $more.VICTORY_UPSCALER_PATH = $Quality }
}
foreach ($k in $Extra.Keys) { $more[$k] = [string]$Extra[$k] }

$opts = @{ Build = $Build; Victory = $victory; More = $more }
if ($Game) { $opts.Game = $Game }
if ($Cache) { $opts.Cache = $Cache }
if ($NoHooks) { $opts.NoHooks = $true } else { $opts.Hooks = $true }
if ($SeriesS) { $opts.SeriesS = $true }
"GDeflate: $(if ($GDeflatePack) { "on, lane wait $WaitUs us" } else { 'off' }); frame rate $(if ($Fps -eq 0) { 'unlocked' } elseif ($Fps -gt 0) { "cap $Fps" } else { 'title (30)' }); upscaler $(if ($Dlss) { "DLSS (Streamline) preset $DlssPreset, $Quality$(if ($FrameGen) { ", Frame Generation $($FgFrames + 1)x" })" } elseif ($OptiScaler) { "OptiScaler $Upscaler" } else { "title's own" })"
& (Join-Path $PSScriptRoot 'scripts\play-scarlett.ps1') @opts
exit $LASTEXITCODE
