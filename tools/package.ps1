# package.ps1 - build the release archives into dist\ (needs cargo on PATH).
#   powershell -ExecutionPolicy Bypass -File tools\package.ps1 [-NoBuild]
#
# dist\unthinkable-search-<ver>-windows-x64.zip      portable program: us.exe (GUI) + usc.exe (CLI)
# dist\unthinkable-search-sdk-<ver>-windows-x64.zip  embedding: DLL + header + docs + examples (+ exes as broker)
# dist\SHA256SUMS.txt
param([switch]$NoBuild)
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot
$ver = (Select-String -Path (Join-Path $root 'Cargo.toml') -Pattern '^version = "(.+)"').Matches[0].Groups[1].Value

if (-not $NoBuild) {
    Push-Location $root
    try {
        cargo build --release
        if ($LASTEXITCODE) { throw 'cargo build --release failed' }
        cargo build --profile release-dll -p us-ffi
        if ($LASTEXITCODE) { throw 'cargo build (dll) failed' }
    } finally { Pop-Location }
}

$rel = Join-Path $root 'target\release'
$dll = Join-Path $root 'target\release-dll\unthinkable_search.dll'
$dist = Join-Path $root 'dist'
if (Test-Path $dist) { Remove-Item -Recurse -Force $dist }
New-Item -ItemType Directory $dist | Out-Null

function Stage([string]$name, [hashtable]$files) {
    $dir = Join-Path $dist $name
    foreach ($dst in $files.Keys) {
        $to = Join-Path $dir $dst
        New-Item -ItemType Directory -Force (Split-Path $to) | Out-Null
        Copy-Item -Recurse $files[$dst] $to
    }
    $zip = "$dir.zip"
    Compress-Archive -Path (Join-Path $dir '*') -DestinationPath $zip -CompressionLevel Optimal
    Remove-Item -Recurse -Force $dir
    Get-Item $zip
}

$app = Stage "unthinkable-search-$ver-windows-x64" @{
    'us.exe'    = Join-Path $rel 'us.exe'
    'usc.exe'   = Join-Path $rel 'usc.exe'
    'README.md' = Join-Path $root 'README.md'
    'README.ru.md' = Join-Path $root 'README.ru.md'
}
$sdk = Stage "unthinkable-search-sdk-$ver-windows-x64" @{
    'unthinkable_search.dll'        = $dll
    'include\unthinkable_search.h'  = Join-Path $root 'crates\us-ffi\include\unthinkable_search.h'
    'us.exe'                        = Join-Path $rel 'us.exe'
    'usc.exe'                       = Join-Path $rel 'usc.exe'
    'docs\INTEGRATION.md'           = Join-Path $root 'docs\INTEGRATION.md'
    'examples'                      = Join-Path $root 'examples'
    'README.md'                     = Join-Path $root 'README.md'
    'README.ru.md'                  = Join-Path $root 'README.ru.md'
}

$sums = foreach ($f in @($app, $sdk)) { '{0}  {1}' -f (Get-FileHash -Algorithm SHA256 $f.FullName).Hash.ToLower(), $f.Name }
$sums | Out-File -Encoding ascii (Join-Path $dist 'SHA256SUMS.txt')
foreach ($f in @($app, $sdk)) { '{0,-48} {1,8:N0} KB' -f $f.Name, ($f.Length / 1KB) }
$sums
