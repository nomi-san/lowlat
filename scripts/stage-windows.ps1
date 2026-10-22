#!/usr/bin/env pwsh
# Build the folder W2.5's install copies from: the host, the display
# controller, the display driver's package when one has been built, the login
# tool and the configuration template.
#
#   scripts/stage-windows.ps1 [-Profile release] [-OutDir <path>] [-Portable]
#
# The package takes the driver from target/vdd-package, which
# scripts/package-vdd.sh signs; a folder without one is still staged, with a
# warning, for a machine that already has the driver. The result is what the
# Windows zip will carry (docs/impl-plan-windows.md W2.5).
#
# -Portable builds the folder that runs the host with nothing installed
# instead, target/dist by default: no template, but packaging/portable's
# launcher and README, the driver's install script with devcon and the
# certificate the package is signed with, and PsExec64 from the PATH. The
# driver is required there.

[CmdletBinding()]
param(
    [string]$Profile = "release",
    [string]$OutDir = "",
    [switch]$Portable
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
if (-not $OutDir) {
    $OutDir = Join-Path $root $(if ($Portable) { "target\dist" } else { "target\staging" })
}
$built = Join-Path $root "target\$Profile"

function Take([string]$Source, [string]$Name) {
    if (-not (Test-Path -LiteralPath $Source)) {
        throw "missing $Source; build it first"
    }
    Copy-Item -LiteralPath $Source -Destination (Join-Path $OutDir $Name) -Force
    Write-Host "  $Name"
}

New-Item -ItemType Directory -Path $OutDir -Force | Out-Null
Write-Host "staging in $OutDir"

Take (Join-Path $built "lowlatd.exe") "lowlatd.exe"
Take (Join-Path $built "lowlat-vdd.exe") "lowlat-vdd.exe"
Take (Join-Path $root "scripts\kessel-login.py") "lowlat-login.py"
if ($Portable) {
    Take (Join-Path $root "packaging\portable\start-host.ps1") "start-host.ps1"
    Take (Join-Path $root "packaging\portable\README.txt") "README.txt"
    Take (Join-Path $root "scripts\vdd-install.ps1") "install-driver.ps1"
    $psexec = Get-Command PsExec64.exe -CommandType Application -ErrorAction SilentlyContinue |
        Select-Object -First 1
    if (-not $psexec) { throw "PsExec64.exe is not on the PATH" }
    Take $psexec.Source "PsExec64.exe"
} else {
    Take (Join-Path $root "packaging\lowlatd.env.windows") "lowlatd.env.template"
}

$parts = @("lowlat_idd.dll", "lowlat-idd.inf", "lowlat-idd.cat")
$package = Join-Path $root "target\vdd-package"
$missing = $parts | Where-Object { -not (Test-Path -LiteralPath (Join-Path $package $_)) }
if ($missing) {
    if ($Portable) { throw "no complete driver package in $package (scripts/package-vdd.sh builds it)" }
    Write-Warning "no complete driver package in $package (scripts/package-vdd.sh builds it); the folder has no driver"
} else {
    $driver = Join-Path $OutDir "driver"
    New-Item -ItemType Directory -Path $driver -Force | Out-Null
    foreach ($part in $parts) {
        Copy-Item -LiteralPath (Join-Path $package $part) -Destination (Join-Path $driver $part) -Force
        Write-Host "  driver\$part"
    }
}

if ($Portable) {
    $kit = (& python.exe (Join-Path $root "scripts\fetch-wdk.py")).Trim()
    Take (Join-Path $kit "c\tools\10.0.26100.0\x64\devcon.exe") "driver\devcon.exe"
    # The public half of the certificate the catalogue is signed with, which
    # the install script makes the machine trust.
    $signer = (Get-AuthenticodeSignature -LiteralPath (Join-Path $package "lowlat-idd.cat")).SignerCertificate
    if (-not $signer) { throw "the driver's catalogue is not signed" }
    [IO.File]::WriteAllBytes((Join-Path $driver "lowlat-idd.cer"), $signer.Export("Cert"))
    Write-Host "  driver\lowlat-idd.cer ($($signer.Thumbprint))"
}
