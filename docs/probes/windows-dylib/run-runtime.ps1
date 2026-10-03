# Builds the runtime probe and runs W2, W3, W4 (imports), W5, W6, W7, W8.
# Usage (pwsh, from anywhere): ./run-runtime.ps1 [-Work <dir>]
# Prints "RESULT ..." lines; does not stop on a failing case.
param([string]$Work = (Join-Path ([System.IO.Path]::GetTempPath()) 'rutis-windows-dylib'))

$ErrorActionPreference = 'Continue'
$probe = $PSScriptRoot
$runtime = Join-Path $probe 'runtime'
if (Test-Path $Work) { Remove-Item -Recurse -Force $Work }
New-Item -ItemType Directory -Force $Work | Out-Null
Write-Host "work dir: $Work"
rustc -vV

function Must($what) {
    if ($LASTEXITCODE -ne 0) { Write-Host "RESULT build: FAIL $what (exit $LASTEXITCODE)"; exit 1 }
}
function Sha($path) { (Get-FileHash -Algorithm SHA256 $path).Hash.ToLower() }
function Run($name, $exe, [string[]]$argv) {
    Write-Host "---- $name"
    & $exe @argv 2>&1 | ForEach-Object { "$_" }
    Write-Host "EXIT $name=$LASTEXITCODE"
}

# Tools and builds -----------------------------------------------------------
$env:CARGO_TARGET_DIR = Join-Path $Work 'pecount-target'
cargo build --release --manifest-path (Join-Path $probe 'pecount/Cargo.toml'); Must 'pecount'
$pecount = Join-Path $Work 'pecount-target/release/pecount.exe'

Push-Location $runtime
# v1 of the plugin, the host, the W5 plugins; the SDK is built as a dependency.
$env:CARGO_TARGET_DIR = Join-Path $Work 't1'
cargo build --release -p host -p greeter -p w5plugin -p w5bad; Must 't1'
# v2 of the same plugin crate, separate target dir.
$env:CARGO_TARGET_DIR = Join-Path $Work 't2'
$env:GREETER_VERSION = '2'
cargo build --release -p host -p greeter; Must 't2'
Remove-Item Env:GREETER_VERSION
# A planted SDK: same crate, same symbols, different mark().
$env:CARGO_TARGET_DIR = Join-Path $Work 't3'
$env:SDK_MARK = 'evil'
cargo build --release -p host; Must 't3'
Remove-Item Env:SDK_MARK
Pop-Location
Remove-Item Env:CARGO_TARGET_DIR

$t1 = Join-Path $Work 't1/release'
$t2 = Join-Path $Work 't2/release'
$t3 = Join-Path $Work 't3/release'
Write-Host "sdk.dll sha256 t1=$(Sha "$t1/sdk.dll") t2=$(Sha "$t2/sdk.dll") t3(evil)=$(Sha "$t3/sdk.dll")"
Write-Host "greeter.dll sha256 v1=$(Sha "$t1/greeter.dll") v2=$(Sha "$t2/greeter.dll")"

# The std DLL the host imports, taken from the toolchain.
$stdName = (& $pecount imports "$t1/host.exe" | Where-Object { $_ -match 'IMPORT (std-.*\.dll)' } | ForEach-Object { $Matches[1] } | Select-Object -First 1)
$sysroot = (rustc --print sysroot).Trim()
Write-Host "host imports: "; & $pecount imports "$t1/host.exe"
Write-Host "std DLLs in sysroot:"
Get-ChildItem -Recurse -Path $sysroot -Filter 'std-*.dll' | ForEach-Object { Write-Host "  $($_.FullName) $(Sha $_.FullName)" }
$stdFile = Get-ChildItem -Recurse -Path $sysroot -Filter $stdName | Select-Object -First 1
Write-Host "using std: $($stdFile.FullName)"

# Release layout -------------------------------------------------------------
$app = Join-Path $Work 'app'
New-Item -ItemType Directory $app | Out-Null
Copy-Item "$t1/host.exe" $app
Copy-Item "$t1/sdk.dll" $app
Copy-Item $stdFile.FullName $app
$host_exe = Join-Path $app 'host.exe'
$cache = Join-Path $Work 'cache'
$hA = (Sha "$t1/greeter.dll").Substring(0, 16)
$hB = (Sha "$t2/greeter.dll").Substring(0, 16)
$pA = Join-Path $cache "$hA/greeter.dll"
$pB = Join-Path $cache "$hB/greeter.dll"
New-Item -ItemType Directory (Split-Path $pA), (Split-Path $pB) | Out-Null
Copy-Item "$t1/greeter.dll" $pA
Copy-Item "$t2/greeter.dll" $pB
$neutral = Join-Path $Work 'neutral'
New-Item -ItemType Directory $neutral | Out-Null
Set-Location $neutral

# W8 -------------------------------------------------------------------------
Run 'W8 v1' $host_exe @('w8', $pA)
Run 'W8 v2' $host_exe @('w8', $pB)

# W2 -------------------------------------------------------------------------
Run 'W2' $host_exe @('w2', $pA, $pB)

# W3 -------------------------------------------------------------------------
Write-Host "host.exe sections (an embedded manifest would be in .rsrc):"
& $pecount sections $host_exe
function Plant($dir) {
    New-Item -ItemType Directory -Force $dir | Out-Null
    Copy-Item "$t3/sdk.dll" (Join-Path $dir 'sdk.dll')
    [System.IO.File]::WriteAllBytes((Join-Path $dir $stdName), [byte[]](0x4d, 0x5a, 0x00, 0x01, 0x02, 0x03))
}
Run 'W3 baseline' $host_exe @('w3', $pA)

$evilCwd = Join-Path $Work 'evil-cwd'; Plant $evilCwd
Set-Location $evilCwd
Run 'W3 cwd' $host_exe @('w3', $pA)
Set-Location $neutral

$evilPath = Join-Path $Work 'evil-path'; Plant $evilPath
$savedPath = $env:PATH
$env:PATH = "$evilPath;$savedPath"
Run 'W3 PATH' $host_exe @('w3', $pA)
$env:PATH = $savedPath

$dotLocal = Join-Path $app 'host.exe.local'; Plant $dotLocal
Run 'W3 .local dir' $host_exe @('w3', $pA)
Remove-Item -Recurse -Force $dotLocal
# Also the file form of DotLocal redirection (empty host.exe.local file).
New-Item -ItemType File (Join-Path $app 'host.exe.local') | Out-Null
Plant (Join-Path $Work 'evil-cwd2'); Set-Location (Join-Path $Work 'evil-cwd2')
Run 'W3 .local file + cwd' $host_exe @('w3', $pA)
Set-Location $neutral
Remove-Item -Force (Join-Path $app 'host.exe.local')

# Planted copies next to the plugin in its cache directory.
$pP = Join-Path $cache 'planted/greeter.dll'
Plant (Split-Path $pP); Copy-Item "$t1/greeter.dll" $pP
Run 'W3 plugin dir' $host_exe @('w3', $pP)

# Controls: SDK missing from the app dir.
$app2 = Join-Path $Work 'app-missing-sdk'
New-Item -ItemType Directory $app2 | Out-Null
Copy-Item "$t1/host.exe" $app2; Copy-Item $stdFile.FullName $app2
Run 'W3 control: sdk missing, nothing planted' (Join-Path $app2 'host.exe') @('w3', $pA)
$env:PATH = "$evilPath;$savedPath"
Remove-Item (Join-Path $evilPath $stdName)
Run 'W3 control: sdk missing, planted in PATH' (Join-Path $app2 'host.exe') @('w3', $pA)
$env:PATH = $savedPath

# W4 (imports; dumpbin output is printed by the workflow) ---------------------
Write-Host "W4 imports of $stdName"; & $pecount imports $stdFile.FullName
Write-Host "W4 imports of sdk.dll"; & $pecount imports "$t1/sdk.dll"
Write-Host "W4 imports of greeter.dll"; & $pecount imports "$t1/greeter.dll"

# W5 -------------------------------------------------------------------------
$p5 = Join-Path $cache 'w5/w5plugin.dll'; New-Item -ItemType Directory (Split-Path $p5) | Out-Null
Copy-Item "$t1/w5plugin.dll" $p5
Run 'W5' $host_exe @('w5', $p5)
$p5b = Join-Path $cache 'w5bad/w5bad.dll'; New-Item -ItemType Directory (Split-Path $p5b) | Out-Null
Copy-Item "$t1/w5bad.dll" $p5b
Run 'W5bad' $host_exe @('w5bad', $p5b)

# W6 -------------------------------------------------------------------------
Write-Host "W6 sections of greeter.dll (release, MSVC link.exe):"
& $pecount sections $pA
& $pecount blob $pA '.rutism'
& $pecount blob $pB '.rutism'

# W7 -------------------------------------------------------------------------
Run 'W7' $host_exe @('w7', "$t1/greeter.dll", (Join-Path $Work 'w7'))

# W8 with the real SDK and the greeter fixture copy ----------------------------
Push-Location (Join-Path $probe 'w8-real')
$env:CARGO_TARGET_DIR = Join-Path $Work 't8'
$env:RUTIS_SDK_ARTIFACT_SHA256 = '0' * 64
cargo build --release -p w8-real-host -p w8-real-greeter
$built8 = $LASTEXITCODE
Remove-Item Env:CARGO_TARGET_DIR
Pop-Location
if ($built8 -eq 0) {
    $t8 = Join-Path $Work 't8/release'
    $app8 = Join-Path $Work 'app-real'
    New-Item -ItemType Directory $app8 | Out-Null
    Copy-Item "$t8/w8-real-host.exe", "$t8/rutis_sdk.dll", $stdFile.FullName $app8
    $p8 = Join-Path $cache "real-$((Sha "$t8/greeter.dll").Substring(0, 16))/greeter.dll"
    New-Item -ItemType Directory (Split-Path $p8) | Out-Null
    Copy-Item "$t8/greeter.dll" $p8
    Run 'W8 real' (Join-Path $app8 'w8-real-host.exe') @($p8, (Join-Path $Work 'drop-marker.txt'))
    Write-Host "W8 real greeter.dll sections (export_plugin! uses .note.rutis.meta today):"
    & $pecount sections $p8
    Write-Host "W8 real rutis_sdk.dll (release, built with this host) exports:"
    & $pecount exports "$t8/rutis_sdk.dll" | Select-Object -First 1
} else {
    Write-Host "RESULT W8.real: FAIL build exit $built8"
}

# Side check: is sdk.dll byte-identical across target dirs (cf. Linux repro)?
Push-Location $runtime
$variants = [ordered]@{
    'remap'                 = ''
    'remap+Brepro'          = '-C link-arg=/Brepro'
    'remap+Brepro+pdbalt'   = '-C link-arg=/Brepro -C link-arg=/PDBALTPATH:%_PDB%'
}
foreach ($name in $variants.Keys) {
    $hashes = @()
    foreach ($n in 1, 2) {
        $td = Join-Path $Work "repro-$($name.Replace('+','-'))-$n"
        $env:CARGO_TARGET_DIR = $td
        $env:RUSTFLAGS = "--remap-path-prefix=$td=/target --remap-path-prefix=$runtime=/src $($variants[$name])"
        cargo build --release -q -p host 2>&1 | Out-Null
        $hashes += (Sha "$td/release/sdk.dll")
    }
    Write-Host "RESULT repro[$name]: identical=$($hashes[0] -eq $hashes[1]) $($hashes -join ' ')"
}
Remove-Item Env:RUSTFLAGS
Remove-Item Env:CARGO_TARGET_DIR
Pop-Location

# Export counts of the probe SDK, for scale.
Write-Host "probe sdk.dll exports:"; & $pecount exports "$t1/sdk.dll"
