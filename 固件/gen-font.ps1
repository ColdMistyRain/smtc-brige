#!/usr/bin/env pwsh
<#
.SYNOPSIS
    Generate LVGL binary font files for the SMTC firmware.
.DESCRIPTION
    Uses lv_font_conv (Rust tool) to convert a TTF font into LVGL .bin format
    with comprehensive CJK character coverage.
.PARAMETER FontPath
    Path to the input .ttf font file (default: looks for msyh.ttf in current dir).
.PARAMETER Size
    Font size in pixels (default: 16).
.PARAMETER OutputDir
    Output directory (default: ./font).
.EXAMPLE
    .\gen-font.ps1 -FontPath C:\Windows\Fonts\msyh.ttf -Size 16
#>

param(
    [string]$FontPath,
    [int]$Size = 16,
    [string]$OutputDir = "./font"
)

$ErrorActionPreference = "Stop"

# --- Config ---
$LV_FONT_CONV_VERSION = "0.4.0"
$BPP = 4  # 4 bits per pixel (16 gray levels, good for anti-aliased CJK)

# Comprehensive character ranges for CJK music display
$RANGES = @(
    "0x0020-0x007F",   # Basic Latin
    "0x00A0-0x00FF",   # Latin-1 Supplement
    "0x0100-0x024F",   # Latin Extended-A/B (accents, etc.)
    "0x2000-0x206F",   # General Punctuation
    "0x2100-0x214F",   # Letterlike Symbols
    "0x2200-0x22FF",   # Mathematical Operators
    "0x2500-0x257F",   # Box Drawing
    "0x2600-0x26FF",   # Misc Symbols
    "0x3000-0x303F",   # CJK Symbols and Punctuation
    "0x3040-0x309F",   # Hiragana
    "0x30A0-0x30FF",   # Katakana
    "0x3100-0x312F",   # Bopomofo
    "0x31F0-0x31FF",   # Katakana Phonetic Extensions
    "0x3400-0x4DBF",   # CJK Unified Ideographs Extension A
    "0x4E00-0x9FFF",   # CJK Unified Ideographs (most common Chinese)
    "0xFF00-0xFFEF",   # Halfwidth and Fullwidth Forms
    "0xFFF0-0xFFFF",   # Specials
    "0xFE30-0xFE4F",   # CJK Compatibility Forms
    "0x2010-0x2027",   # Dashes, Hyphens
    "0x2030-0x205E",   # General Punctuation
    "0x2018-0x201D",   # Quotation marks
    "0x201C-0x201D"    # Smart quotes
)

$RANGE_ARG = ($RANGES -join ",")

# --- Find or install lv_font_conv ---
function Get-LvFontConv {
    $cargo_bin = if (Get-Command cargo -ErrorAction SilentlyContinue) { cargo } else { $null }
    
    # Try system-installed
    $installed = Get-Command lv_font_conv -ErrorAction SilentlyContinue
    if ($installed) {
        Write-Host "Found lv_font_conv: $($installed.Source)" -ForegroundColor Green
        return "lv_font_conv"
    }
    
    # Try cargo install
    if ($cargo_bin) {
        Write-Host "Installing lv_font_conv via cargo..." -ForegroundColor Yellow
        cargo install lv_font_conv --version $LV_FONT_CONV_VERSION
        if ($LASTEXITCODE -eq 0) {
            Write-Host "Installed successfully." -ForegroundColor Green
            return "lv_font_conv"
        }
    }
    
    # Try downloading prebuilt binary
    $url = "https://github.com/lvgl/lv_font_conv/releases/download/v$LV_FONT_CONV_VERSION/lv_font_conv-v$LV_FONT_CONV_VERSION-x86_64-pc-windows-msvc.zip"
    $zip = "$env:TEMP\lv_font_conv.zip"
    $extract = "$env:TEMP\lv_font_conv"
    
    Write-Host "Downloading lv_font_conv v$LV_FONT_CONV_VERSION..." -ForegroundColor Yellow
    Invoke-WebRequest -Uri $url -OutFile $zip
    Expand-Archive -Path $zip -DestinationPath $extract -Force
    $exe = Get-ChildItem -Path $extract -Recurse -Filter "lv_font_conv.exe" | Select-Object -First 1
    if ($exe) {
        Write-Host "Downloaded to: $($exe.FullName)" -ForegroundColor Green
        return $exe.FullName
    }
    
    throw "Could not install lv_font_conv. Install manually: cargo install lv_font_conv"
}

# --- Find font ---
if (-not $FontPath) {
    $candidates = @(
        ".\msyh.ttf",
        ".\font\msyh.ttf",
        "C:\Windows\Fonts\msyh.ttf",
        "C:\Windows\Fonts\msyhbd.ttf",
        "C:\Windows\Fonts\simsun.ttc"
    )
    foreach ($c in $candidates) {
        if (Test-Path $c) {
            $FontPath = $c
            break
        }
    }
}

if (-not $FontPath -or -not (Test-Path $FontPath)) {
    throw @"
No TTF font found. Please provide one:
  .\gen-font.ps1 -FontPath "C:\Windows\Fonts\msyh.ttf"
Recommended free CJK fonts:
  - Noto Sans CJK SC (Google)
  - Microsoft YaHei (Windows built-in)
  - Source Han Sans (Adobe)
"@
}

Write-Host "Font : $FontPath" -ForegroundColor Cyan
Write-Host "Size : ${Size}px" -ForegroundColor Cyan
Write-Host "BPP  : $BPP" -ForegroundColor Cyan

# --- Generate ---
$converter = Get-LvFontConv
$outDir = New-Item -ItemType Directory -Path $OutputDir -Force
$outName = "smtc_cjk_${Size}.bin"
$outPath = Join-Path $outDir $outName

Write-Host "`nGenerating $outName ..." -ForegroundColor Yellow
Write-Host "This may take 2-5 minutes for full CJK range..." -ForegroundColor DarkGray

$cmd = "& '$converter' --font '$FontPath' --size $Size --bpp $BPP --format bin --range '$RANGE_ARG' -o '$outPath'"
Write-Host "Command: $cmd" -ForegroundColor DarkGray

Invoke-Expression $cmd

if ($LASTEXITCODE -ne 0) {
    throw "lv_font_conv failed with exit code $LASTEXITCODE"
}

$fileSize = (Get-Item $outPath).Length
$sizeMB = [math]::Round($fileSize / 1MB, 2)
Write-Host "`nDone: $outPath ($sizeMB MB)" -ForegroundColor Green
Write-Host "Copy this file to your firmware's font/ directory and update load_font() in ui.lua."
