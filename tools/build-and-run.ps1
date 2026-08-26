# Builds the viewer when the sources are newer than the binary, then launches it.
# Driven by "Open 3D Viewer.vbs" at the repository root, which runs this without a console.

$ErrorActionPreference = 'Stop'
# Loaded up front: both failure paths below report through a message box, and this script runs
# with no console to print to.
Add-Type -AssemblyName System.Windows.Forms

$root = Split-Path -Parent $PSScriptRoot
$log = Join-Path $PSScriptRoot 'build.log'
$exe = Join-Path $root 'target\release\viewer.exe'

function Log($msg) {
    "$(Get-Date -Format 'HH:mm:ss')  $msg" | Add-Content -Path $log -Encoding utf8
}

"" | Set-Content -Path $log -Encoding utf8
Log "root $root"

# Cargo, installed by rustup under the user profile.
$cargoBin = Join-Path $env:USERPROFILE '.cargo\bin'
if (-not (Test-Path (Join-Path $cargoBin 'cargo.exe'))) {
    Log 'cargo not found'
    [System.Windows.Forms.MessageBox]::Show(
        'The Rust toolchain is missing. Install it with: winget install Rustlang.Rustup',
        '3D Viewer') | Out-Null
    exit 1
}
$env:Path = "$cargoBin;$env:Path"

# The windows-gnu target links through MinGW binutils, which rustup does not ship.
$mingw = Get-ChildItem -Path (Join-Path $env:LOCALAPPDATA 'Microsoft\WinGet\Packages') `
    -Filter 'dlltool.exe' -Recurse -ErrorAction SilentlyContinue |
    Select-Object -First 1
if ($mingw) {
    $env:Path = "$($mingw.Directory.FullName);$env:Path"
    Log "mingw $($mingw.Directory.FullName)"
}

# Rebuild only when something under crates/ is newer than the binary.
$newest = Get-ChildItem (Join-Path $root 'crates') -Recurse -Include *.rs, *.wgsl, *.toml -ErrorAction SilentlyContinue |
    Sort-Object LastWriteTime -Descending | Select-Object -First 1
$stale = $true
if ((Test-Path $exe) -and $newest) {
    $stale = $newest.LastWriteTime -gt (Get-Item $exe).LastWriteTime
}

if ($stale) {
    Log 'building'
    Push-Location $root
    & cargo build --release -p viewer-app 2>&1 | Add-Content -Path $log -Encoding utf8
    $code = $LASTEXITCODE
    Pop-Location
    if ($code -ne 0) {
        Log "build failed with $code"
        [System.Windows.Forms.MessageBox]::Show(
            "The build failed. See tools\build.log.", '3D Viewer') | Out-Null
        exit $code
    }
}

Log 'launching'
$viewerArgs = $args | Where-Object { $_ }
if ($viewerArgs) {
    Start-Process -FilePath $exe -ArgumentList $viewerArgs
} else {
    Start-Process -FilePath $exe
}
