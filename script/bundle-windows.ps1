[CmdletBinding()]
Param(
    [Parameter()][Alias('i')][switch]$Install,
    [Parameter()][Alias('h')][switch]$Help
)

$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $true

$buildSuccess = $false

if ($Help) {
    Write-Output "Usage: bundle-windows.ps1 [-Install] [-Help]"
    Write-Output "Build the installer for DeepTerm on Windows.`n"
    Write-Output "Options:"
    Write-Output "  -Install, -i      Run the installer after building."
    Write-Output "  -Help, -h         Show this help message."
    exit 0
}

$innoSetupPath = "C:\Program Files (x86)\Inno Setup 6\ISCC.exe"
if (-not (Test-Path $innoSetupPath)) {
    $innoSetupPath = "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe"
}
if (-not (Test-Path $innoSetupPath)) {
    Write-Output "Inno Setup 6 not found. Installing via winget..."
    winget install -e --id=JRSoftware.InnoSetup --accept-source-agreements --accept-package-agreements
    if (-not (Test-Path $innoSetupPath)) {
        $innoSetupPath = "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe"
    }
    if (-not (Test-Path $innoSetupPath)) {
        Write-Error "Inno Setup installation failed. Please install manually from https://jrsoftware.org/isdl.php"
        exit 1
    }
}

Write-Output "========================================"
Write-Output "Building DeepTerm Windows Installer"
Write-Output "========================================"

Write-Output "`n[1/4] Checking Rust target..."
rustup target add x86_64-pc-windows-msvc

Write-Output "`n[2/4] Building release binary..."
cargo build --release

if ($LASTEXITCODE -ne 0) {
    Write-Error "Cargo build failed"
    exit 1
}

$bundleDir = ".\target\release\bundle"
if (-not (Test-Path $bundleDir)) {
    New-Item -ItemType Directory -Path $bundleDir -Force | Out-Null
}

Write-Output "`n[3/4] Building installer with Inno Setup..."
$process = Start-Process -FilePath $innoSetupPath -ArgumentList ".\windows\deepterm.iss" -NoNewWindow -Wait -PassThru

if ($process.ExitCode -eq 0) {
    $buildSuccess = $true
    Write-Output "`nInstaller built successfully!"
    $installerPath = Resolve-Path "$bundleDir\DeepTerm-0.1.0-setup.exe"
    Write-Output "   Location: $installerPath"
} else {
    Write-Output "`nInno Setup failed with exit code: $($process.ExitCode)"
    $buildSuccess = $false
}

if ($buildSuccess -and $Install) {
    Write-Output "`n[4/4] Installing DeepTerm..."
    Start-Process -FilePath "$bundleDir\DeepTerm-0.1.0-setup.exe" -Wait
    Write-Output "Installation complete."
} elseif ($buildSuccess) {
    Write-Output "`n[4/4] Done! Run with -Install flag to install automatically."
}

if ($buildSuccess) {
    exit 0
} else {
    exit 1
}
