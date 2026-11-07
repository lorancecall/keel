# Builds the release exes, runs the tests and makes the installer (dist\keel-setup-<version>.exe).
$ErrorActionPreference = 'Stop'
Set-Location $PSScriptRoot
# No build-machine paths in the exes: source paths in panic messages become generic names and the
# PDB is named without its folder.
$cargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $env:USERPROFILE '.cargo' }
$rustupHome = if ($env:RUSTUP_HOME) { $env:RUSTUP_HOME } else { Join-Path $env:USERPROFILE '.rustup' }
$env:RUSTFLAGS = @(
    "--remap-path-prefix=$PSScriptRoot=keel",
    "--remap-path-prefix=$cargoHome=cargo",
    "--remap-path-prefix=$rustupHome=rustup",
    '-C', 'link-arg=/PDBALTPATH:%_PDB%'
) -join ' '
cargo build --release
if ($LASTEXITCODE) { throw 'cargo build failed' }
cargo test --release
if ($LASTEXITCODE) { throw 'cargo test failed' }
& "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe" /Q installer\keel.iss
if ($LASTEXITCODE) { throw 'ISCC failed' }
# SHA-256 next to the installer (published with each release so downloads can be checked).
Get-ChildItem dist\*.exe | Sort-Object LastWriteTime | Select-Object -Last 1 | ForEach-Object {
    $h = (Get-FileHash $_.FullName -Algorithm SHA256).Hash
    Set-Content "$($_.FullName).sha256" "$h  $($_.Name)" -Encoding ascii
    '{0}  {1:N1} MB  SHA-256 {2}' -f $_.Name, ($_.Length / 1MB), $h
}
