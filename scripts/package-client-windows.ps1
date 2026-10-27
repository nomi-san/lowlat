#!/usr/bin/env pwsh
# The client's Windows zip (docs/impl-plan-windows.md W1.10): the client-only library as a DLL with
# its import library and as a static library, the header, the demo linked from that static
# library, and the toolkit's and SDL's notices beside the project's own; the DLL's and the demo's
# symbols go in a second zip. Every check reads the zip's own copies: the DLL says it is the client
# half and exports no host entry point (describe, compiled here from the header that ships), and
# the demo started with no peer names the library linked into it and leaves with 2.
#
#   scripts/package-client-windows.ps1 [-OutDir target\dist-client] [-Name <name>]
#
# Needs cargo, CMake and Ninja, clang-cl, vcpkg (VCPKG_ROOT) for the demo's SDL, and Visual
# Studio's x64 developer environment, which is taken from Visual Studio when `cl` is not on the
# path: describe is compiled with `cl`, and the toolkit's one build, when it has not been made,
# runs its own nmake, which wants `fxc` and Khronos' glslangValidator on the path as well.

[CmdletBinding()]
param(
    [string]$OutDir = "",
    [string]$Name = ""
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
if (-not $OutDir) { $OutDir = Join-Path $root "target\dist-client" }
if (-not $Name) {
    $sha = (git -C $root rev-parse --short=7 HEAD).Trim()
    $Name = "lowlat-sdk-windows-x86_64-client-$sha"
}

function Step([string]$What) { Write-Host "== $What" }
function Check([string]$What) {
    if ($LASTEXITCODE -ne 0) { throw "$What failed, exit code $LASTEXITCODE" }
}

. (Join-Path $PSScriptRoot "vs-env.ps1")
if (-not $env:VCPKG_ROOT) { throw "VCPKG_ROOT names no vcpkg" }
# The demo's check starts it with no peer: a session named in this environment would connect.
Remove-Item Env:\LOWLAT_PEER, Env:\LOWLAT_SESSION -ErrorAction SilentlyContinue

$manifest = Join-Path $root "Cargo.toml"
$target = (cargo metadata --format-version 1 --no-deps --manifest-path $manifest | ConvertFrom-Json).target_directory
$stage = Join-Path $OutDir $Name
$symbols = Join-Path $OutDir "$Name-symbols"
foreach ($old in $stage, $symbols, "$stage.zip", "$symbols.zip") {
    if (Test-Path -LiteralPath $old) { Remove-Item -Recurse -Force -LiteralPath $old }
}
New-Item -ItemType Directory -Force "$stage\bin", "$stage\lib", "$stage\include", $symbols | Out-Null

# Both forms land in the same target directory as any other build of the library, so each is
# copied out the moment it is made.
Step "the client-only library, shared"
cargo build --release -p lowlat-sdk --no-default-features --features client --manifest-path $manifest
Check "the shared library's build"
Copy-Item "$target\release\lowlat.dll", "$target\release\lowlat.dll.lib" "$stage\lib\"
Copy-Item "$target\release\lowlat.pdb" $symbols

Step "the client-only library, static"
cargo rustc --release -p lowlat-sdk --no-default-features --features client --crate-type staticlib `
    --manifest-path $manifest
Check "the static library's build"
Copy-Item "$target\release\lowlat.lib" "$stage\lib\"

# Named, the static library is linked as it is: the demo's own build of it, with every half, is
# not made.
Step "the demo, from that static library"
$demo = Join-Path $target "client-demo"
Push-Location (Join-Path $root "examples\client")
try {
    cmake --preset win-x64-clang-release -B $demo "-DLOWLAT_LIB=$stage\lib\lowlat.lib"
    Check "the demo's configure"
    cmake --build $demo
    Check "the demo's build"
} finally {
    Pop-Location
}
Copy-Item "$demo\client.exe" "$stage\bin\"
Copy-Item "$demo\client.pdb" $symbols

Copy-Item (Join-Path $root "include\lowlat.h") "$stage\include\"
Copy-Item (Join-Path $root "LICENSE") $stage
Copy-Item (Join-Path $root "third_party\matoya\LICENSE") "$stage\LICENSE.matoya"
Copy-Item "$demo\vcpkg_installed\x64-windows-static\share\sdl3\copyright" "$stage\LICENSE.sdl3"
Copy-Item (Join-Path $root "docs\06-api.md"), (Join-Path $root "docs\10-client.md") $stage
Copy-Item (Join-Path $root "examples\client\README.md") $stage

Step "what the zip's library says it is"
$describe = Join-Path $target "describe.exe"
cl /nologo /W4 /WX /I (Join-Path $root "include") (Join-Path $root "crates\sdk\tests\c\describe.c") `
    "/Fe:$describe" "/Fo:$target\describe.obj"
Check "describe's build"
& $describe "$stage\lib\lowlat.dll" 0x2 | Tee-Object -FilePath "$stage\FEATURES"
Check "describe"
$exports = @(dumpbin /nologo /exports "$stage\lib\lowlat.dll" | Select-String ' lowlat_').Count
Write-Host "lowlat_ entry points exported: $exports"

Step "the demo with no peer"
$out = & "$stage\bin\client.exe" 2>&1 | Out-String
$code = $LASTEXITCODE
Write-Host $out
if ($code -ne 2) { throw "the demo left with $code, not 2" }
if ($out -notmatch 'features=0x2\b') { throw "the demo does not carry the client half alone" }

Step "the zips"
Compress-Archive -Path $stage -DestinationPath "$stage.zip" -CompressionLevel Optimal
Compress-Archive -Path $symbols -DestinationPath "$symbols.zip" -CompressionLevel Optimal
Get-ChildItem -Recurse -File $stage, $symbols | ForEach-Object {
    "{0,12} {1}" -f $_.Length, $_.FullName.Substring($OutDir.Length + 1)
}
Get-Item "$stage.zip", "$symbols.zip" | ForEach-Object { "{0,12} {1}" -f $_.Length, $_.Name }
# The demo's 2 is still the last native exit code, which a caller would take for the script's.
exit 0
