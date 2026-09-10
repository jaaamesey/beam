$ErrorActionPreference = "Stop"

$RepoRoot = Split-Path -Parent $PSScriptRoot
$VcpkgRoot = Join-Path $RepoRoot ".vcpkg"
$VcpkgRevision = "3723ec118c8354290925feb58d021a9205a3e772"
$Triplet = "x64-windows-static-md-release"

if (-not (Test-Path (Join-Path $VcpkgRoot ".git"))) {
    git clone https://github.com/microsoft/vcpkg.git $VcpkgRoot
    if ($LASTEXITCODE -ne 0) { throw "failed to clone vcpkg" }
}

git -C $VcpkgRoot fetch --depth 1 origin $VcpkgRevision
if ($LASTEXITCODE -ne 0) { throw "failed to fetch pinned vcpkg revision" }
git -C $VcpkgRoot checkout --detach $VcpkgRevision
if ($LASTEXITCODE -ne 0) { throw "failed to check out pinned vcpkg revision" }

& (Join-Path $VcpkgRoot "bootstrap-vcpkg.bat") -disableMetrics
if ($LASTEXITCODE -ne 0) { throw "failed to bootstrap vcpkg" }

& (Join-Path $VcpkgRoot "vcpkg.exe") install `
    "--triplet=$Triplet" `
    "--overlay-triplets=$RepoRoot/vcpkg-triplets" `
    "--x-install-root=$VcpkgRoot/installed" `
    --clean-buildtrees-after-build `
    --clean-packages-after-build `
    --x-feature=windows-hardware
if ($LASTEXITCODE -ne 0) { throw "failed to build static FFmpeg" }

$Avcodec = Join-Path $VcpkgRoot "installed/$Triplet/lib/avcodec.lib"
if (-not (Test-Path $Avcodec)) { throw "static avcodec.lib was not installed" }
Write-Host "Static FFmpeg is ready for Cargo ($Triplet)."
