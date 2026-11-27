# Starts the title in Series X mode with this branch's current settings and memory hooks
# (the set the README's figures are measured with).
#
#   .\scripts\play-scarlett.ps1 -Game 'D:\Games\MyDump'
#
# -Game        your own dump: the folder with game_scarlett_master.exe (or that file itself).
#              Remembered in game-folder.txt next to the repository's README, so later starts need no -Game.
# -Build       diag (default: the build all measurements are of) or release
# -Victory     another victory.exe than target\<Build>\victory.exe
# -Cache       shader and pipeline caches (default: %LOCALAPPDATA%\victory-scarlett\cache). A first run fills them
#              and is slow; faces can be missing in that first run and are right from the second.
# -Data        saves (default: %LOCALAPPDATA%\victory-scarlett\data)
# -RayTracing  the title's own ray tracing (VICTORY_BVH_BUILDER=1): better light, much slower. Off by default.
# -AutoStart   presses through the start screens by itself, for a fresh start with no save
# -NoHooks     without the memory hooks: the first thing to try if something looks wrong
# -More        other settings, last word: -More @{ VICTORY_LOG = 'warn' }
#
# Run it from a normal window, not as administrator.
param(
    [string]$Game = '',
    [ValidateSet('diag', 'release')][string]$Build = 'diag',
    [ValidateSet('series-x', 'series-s')][string]$Console = 'series-x',
    [ValidateSet('1440', '4k')][string]$Res = '1440',
    [string]$Resolution = '2560x1440',
    [string]$Victory = '',
    [string]$Cache = '',
    [string]$Data = '',
    [switch]$Dlss,
    [switch]$FrameGen,
    [ValidateRange(1, 5)][int]$FgFrames = 1,
    [ValidateSet('L', 'M', 'K', 'auto')][string]$DlssPreset = 'L',
    [ValidateSet('native', 'quality', 'balanced', 'performance', 'ultra')][string]$Quality = 'quality',
    [string]$StreamlineDir = '',
    [switch]$RayTracing,
    [switch]$SkipRtHashes,
    [switch]$AutoStart,
    [switch]$NoHooks,
    [hashtable]$More = @{}
)
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$remembered = Join-Path $root 'game-folder.txt'

if (-not $Game -and (Test-Path -LiteralPath $remembered)) { $Game = (Get-Content -LiteralPath $remembered -TotalCount 1) }
if (-not $Game) { throw "Say where your dump is: .\scripts\play-scarlett.ps1 -Game 'D:\Games\MyDump'" }
$Game = $Game.Trim().Trim('"')
if ((Test-Path -LiteralPath $Game -PathType Leaf) -and $Game.EndsWith('.exe')) { $Game = Split-Path -Parent $Game }
$exe = Join-Path $Game 'game_scarlett_master.exe'
if (-not (Test-Path -LiteralPath $exe -PathType Leaf)) { throw "No game_scarlett_master.exe in $Game. -Game is the folder of your dump." }
Set-Content -LiteralPath $remembered -Value $Game -Encoding utf8

if (-not $Victory) { $Victory = Join-Path $root "target\$Build\victory.exe" }
if (-not (Test-Path -LiteralPath $Victory -PathType Leaf)) {
    throw "No $Victory. Build it first: cargo build --profile diag -p victory --features diagnostics --locked"
}
if (-not $Cache) { $Cache = Join-Path $env:LOCALAPPDATA 'victory-scarlett\cache' }
if (-not $Data) { $Data = Join-Path $env:LOCALAPPDATA 'victory-scarlett\data' }
New-Item -ItemType Directory -Force -Path $Cache, $Data | Out-Null

# Settings left in this window by an earlier start go: only what is below counts.
Get-ChildItem Env: | Where-Object { $_.Name -like 'VICTORY_*' } | ForEach-Object { Remove-Item "Env:$($_.Name)" }

$set = [ordered]@{
    VICTORY_GAME_DIR  = $Game
    VICTORY_CACHE_DIR = $Cache
    VICTORY_DATA_DIR  = $Data
    VICTORY_DISPLAY_MODE = $Resolution

    # The standing set of this fork's runs (some names come from its older tree and are ignored here)
    VICTORY_JOIN_COPIES = '1'; VICTORY_REFRESH_ONCE = '0'; VICTORY_CARD_REHASH_RUNS = '1'; VICTORY_BACK_OWN_VIEW = '1'
    VICTORY_WIDE_MERGE = '1'; VICTORY_MIRROR_FAST = '1'
    VICTORY_FRAME_LOG = $(if ($Build -eq 'diag') { '1' } else { '0' })
    VICTORY_CARD_METER = $(if ($Build -eq 'diag') { '1' } else { '0' })
    VICTORY_PIPELINE_WAIT_MS = '20'; VICTORY_ASYNC_SUBMIT = '1'; VICTORY_QUEUE2 = '1'; VICTORY_INDIRECT_WINDOW = '1'
    VICTORY_TEXTURE_NO_WAIT = '1'; VICTORY_COPY_PLAIN = '1'; VICTORY_WIDE_TEXTURES = '1'; VICTORY_INDIRECT_GATHER = '1'
    VICTORY_LATE_WAIT_LISTS = '1024'; VICTORY_CARD_FILLS_BATCH = '0'; VICTORY_GATHER_DISPATCH = '0'

    # cool_fox's launcher set, without Series S, VICTORY_THIN and the stand-in ambient light; the keep-busy loop off
    VICTORY_RELAY = '2'; VICTORY_WIDE_BACK = '1'; VICTORY_RELAY_COPY = '1'; VICTORY_CARD_STEPS = '2'
    VICTORY_INDIRECT_BATCH = '2'; VICTORY_MAP_THREADS = '1'; VICTORY_CARD_AWAKE = '0'; VICTORY_SHOW_AFTER = '1'
    VICTORY_JOIN_PARTS = '8'; VICTORY_UPLOAD_AHEAD = '1'; VICTORY_CARD_BUFFERS_MIN_KB = '64'

    # Textures keep only the levels their views show (the base's default; cool_fox's launcher turned it off)
    VICTORY_LEVELS_ON_DEMAND = '0'

    # This fork's memory, VRAM residency and CPU-sync switches (Phase 1+2+3 preset)
    VICTORY_POOL_DISCARD = '1'; VICTORY_FILL_TITLE = '1'; VICTORY_PARTIAL_DELTA = '1'; VICTORY_BAKE_DELTA = '1'
    VICTORY_MAP_LOCK_FINE = '1'; VICTORY_POOL_CAP = '1'; VICTORY_PAGE_POOL = '1'; VICTORY_FENCE_RECLAIM = '1'
    VICTORY_XMEM_CHUNKS = '1'; VICTORY_XMEM_CHUNK_MB = '16'; VICTORY_XMEM_COMMIT_CHUNKS = '0'
    VICTORY_GPU_ONLY_LIFETIME = 'dead'; VICTORY_GPU_ONLY_DEAD_S = '5'
    VICTORY_SEND_SPARE_MB = '128'; VICTORY_LIST_POOL_IDLE_S = '5'
    VICTORY_WINDOW_KB = '512'; VICTORY_WINDOW_TAILS = '1'; VICTORY_WINDOW_COVERED = '1'
    VICTORY_CARD_BUFFERS_MB = '1536'; VICTORY_CARD_CPU_STAGING = '1'
    VICTORY_INPLACE = '1'; VICTORY_PRESUBMIT_HEAPS = '1'; VICTORY_AHEAD_SEARCH = '1'
    VICTORY_CLEAN_STAMP = '0'; VICTORY_PREPARE_WIDE = '1'; VICTORY_PIPELINE_WORKERS = '4'
    VICTORY_UNDER_FUSED = '1'; VICTORY_GUARD_REST = '3000'; VICTORY_BIND_FILTER = '1'
    VICTORY_METER_THREAD = '1'; VICTORY_MEMORY_MAP = '0'; VICTORY_PIPELINE_BOOK = '1'
    VICTORY_ASK_KEPT_EARLY = '1'; VICTORY_CARD_LISTS = '1'; VICTORY_EARLY_TEXTURES_MS = '40'
    VICTORY_FRAME_INTERVAL_MS = '0'
    # RAM & VRAM reclamation pack (verified on 32 GB RAM / 12 GB VRAM)
    VICTORY_DROP_DXIL = '1'; VICTORY_UNMAP_EXACT = '1'; VICTORY_WINDOW_UNMAP = '1'
    VICTORY_POOL_DISCARD_BELOW_MB = '12288'; VICTORY_SOFT_VIEWS = 'copies'
    VICTORY_CARD_TRIM_CPU = '1'; VICTORY_BACK_SKIP_UNREAD_MB = '128'
    VICTORY_WINDOW_BUDGET_MB = '1024'; VICTORY_POOL_SMALL = '0'

    # Foxhound / VIctory-NVIDIA + AnyXboxX + Hybrid + Willy mega-pack wins
    VICTORY_LEAVE_OUT = '02384a60fda6f2d0,731a39171b568298'
    VICTORY_WC_NO_BACK = '1'; VICTORY_WC_NO_BACK_MIN_KB = '1024'
    VICTORY_SYNC_IN_PLACE = '1'; VICTORY_SYNC_FINE = '1'; VICTORY_SPANS_QUICK = '1'
    VICTORY_EXACT_NO_WAIT = '1'; VICTORY_RESULT_RING = '1'
    VICTORY_UP_QUEUE = '1'; VICTORY_PORT_JOURNAL = '1'; VICTORY_PORT_GUARD = '1'; VICTORY_PORT_GUARD_PLAN = '1'
    VICTORY_NATIVE_BARRIERS = '0'; VICTORY_SLOT_REMAP = '1'; VICTORY_HEAP_BANKS = '3'
    VICTORY_READBACK_POOL = '1'; VICTORY_RECYCLE_LANDED_EARLY = '1'
    VICTORY_REFRESH_SPLIT_COPY = '1'; VICTORY_BATCH_GUARDS = '1'
    VICTORY_E5_TARGETS = 'r11g11b10'; VICTORY_BAKE_TEXTURES_MS = '5000'; VICTORY_NAN_GUARD_STORES = '1'
    VICTORY_AMBIENT = '0.6'; VICTORY_AMBIENT_AIR = '1.0'; VICTORY_AMBIENT_MARCH = '0'
    VICTORY_AMBIENT_SPECULAR = '0.8'; VICTORY_AMBIENT_ONCE = '1'; VICTORY_AMBIENT_LEFTOVER = 'off'
    VICTORY_THIN = '0'; VICTORY_PORT_CONSTANTS = '0'; VICTORY_WATER_LIGHT = '0'
    VICTORY_MENU_FILE = (Join-Path $root 'victory-settings.txt')
    VICTORY_MENU_PLAY = $PSCommandPath

    # Draw set-up: table entries done earlier in a list are not done again; an aliased texture's copy follows its
    # source's writes within a list (a stale read in the base)
    VICTORY_TABLE_MEMO = '1'; VICTORY_LIST_ALIAS_EXACT = '1'
}
if (-not $RayTracing -and $SkipRtHashes) {
    $rtOffPath = Join-Path $PSScriptRoot 'rt_off_hashes.txt'
    if (Test-Path -LiteralPath $rtOffPath) {
        $hashes = (Get-Content -LiteralPath $rtOffPath | Where-Object { $_ -and -not $_.StartsWith('#') }) -join ','
        if ($hashes) { $set.VICTORY_SKIP_PIPELINES = $hashes }
    }
}
if (-not $NoHooks) {
    # The memory hooks (disable known-writes ledger in release where 6-core CPUs hit its 3 ms cost limit)
    $set.VICTORY_GPU_ONLY = '1'
    $set.VICTORY_CHUNK_FINAL = '1'
    $set.VICTORY_READBACK_SET = '1'
    $set.VICTORY_READBACK_LOCKS = '1'
    $set.VICTORY_BACK_SKIP_UNREAD = '1'
    $set.VICTORY_PORT_DESCRIPTORS = '1'
    $set.VICTORY_KNOWN_WRITES = $(if ($Build -eq 'diag') { '1' } else { '0' })
}
if ($Console -eq 'series-x' -and $Res -eq '1440') {
    $set.VICTORY_NATIVE_1440P = '1'
}
if ($Dlss) {
    if (-not $StreamlineDir) {
        $candidates = @(
            $env:VICTORY_STREAMLINE_DIR,
            (Join-Path $root 'streamline-sdk\bin\x64'),
            (Join-Path $root 'streamline\bin\x64')
        )
        $pdir = $root
        for ($i = 0; $i -lt 3; $i++) {
            $parent = Split-Path -Parent $pdir
            if (-not $parent -or $parent -eq $pdir) { break }
            $candidates += (Join-Path $parent 'streamline-sdk\bin\x64')
            $pdir = $parent
        }
        $StreamlineDir = $candidates | Where-Object { $_ -and (Test-Path (Join-Path $_ 'sl.interposer.dll')) } | Select-Object -First 1
    }
    if (-not $StreamlineDir -or -not (Test-Path (Join-Path $StreamlineDir 'sl.interposer.dll'))) {
        Write-Error "No sl.interposer.dll found. Pass -StreamlineDir <path-to-streamline-sdk\bin\x64> or place Streamline SDK in .\streamline-sdk\bin\x64"
        exit 2
    }
    $set.VICTORY_STREAMLINE_DIR = (Resolve-Path $StreamlineDir).Path
    $set.VICTORY_DLSS = '1'
    $set.VICTORY_DLSS_MODE = switch ($Quality) { 'native' { 'dlaa' } 'ultra' { 'ultraperformance' } default { $Quality } }
    if ($DlssPreset -ne 'auto') { $set.VICTORY_DLSS_PRESET = $DlssPreset.ToLowerInvariant() }
    if ($FrameGen) {
        $set.VICTORY_FRAMEGEN = '1'
        $set.VICTORY_FRAMEGEN_FRAMES = "$FgFrames"
    }
}
if ($RayTracing) { $set.VICTORY_BVH_BUILDER = '1' }
if ($AutoStart) {
    $set.VICTORY_AUTO_INPUT = 'd>=15,rep(A,1,d<10),w3,d>=20,w1,rep(A,1,d>=35),w1.5,Down,w7,Down,w0.6,A,w1.5,A,w0.7,Down,w0.5,A,w0.7,Down,w0.5,A'
}
$menuFile = $set.VICTORY_MENU_FILE
if ($menuFile -and (Test-Path -LiteralPath $menuFile)) {
    foreach ($line in (Get-Content -LiteralPath $menuFile)) {
        if ($line -match '^\s*(VICTORY_[A-Z0-9_]+)\s*=(.*)$') {
            $k = $Matches[1]; $v = $Matches[2].Trim()
            if ($v -eq '') { $set.Remove($k) } else { $set[$k] = $v }
        }
    }
}
foreach ($k in $More.Keys) { $set[$k] = [string]$More[$k] }
foreach ($k in $set.Keys) { Set-Item -Path "Env:$k" -Value $set[$k] }

Write-Host ("{0} mode ({1}), {2} build, ray tracing {3}, memory hooks {4}" -f $Console, $Resolution, $Build, $(if ($RayTracing) { 'on' } else { 'off' }), $(if ($NoHooks) { 'off' } else { 'on' }))
Write-Host "Cache: $Cache"
Write-Host "Saves: $Data"
& $Victory run $exe --execute --console $Console
exit $LASTEXITCODE
