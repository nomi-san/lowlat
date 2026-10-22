#!/usr/bin/env pwsh
# Build the folder W2.5's install copies from: the host, the display
# controller, the login tool and the configuration template, and each driver's
# package with its install script when one has been built.
#
#   scripts/stage-windows.ps1 [-Profile release] [-OutDir <path>] [-Portable]
#
# The display driver comes from target/vdd-package (scripts/package-vdd.sh),
# into driver\ beside install-driver.ps1; the input driver from
# target/hid-package (scripts/package-hid.sh), into input-driver\ beside
# install-input-driver.ps1. Each folder carries nefcon, its licence and the
# certificate the package is signed with. A folder without a package is still
# staged, with a warning, for a machine that already has that driver or does
# without it. The service's install installs no driver: the machine's
# administrator runs the scripts (docs/07-platforms.md section 10). The result
# is what the Windows zip will carry (docs/impl-plan-windows.md W2.5).
#
# -Portable builds the folder that runs the host with nothing installed
# instead, target/dist by default: no template, but packaging/portable's
# launcher and README, and PsExec64 from the PATH. The display driver is
# required there.

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
    $psexec = Get-Command PsExec64.exe -CommandType Application -ErrorAction SilentlyContinue |
        Select-Object -First 1
    if (-not $psexec) { throw "PsExec64.exe is not on the PATH" }
    Take $psexec.Source "PsExec64.exe"
} else {
    Take (Join-Path $root "packaging\lowlatd.env.windows") "lowlatd.env.template"
}

$nefcon = (& python.exe (Join-Path $root "scripts\fetch-nefcon.py")).Trim()
$nefconLicense = Join-Path (Split-Path -Parent (Split-Path -Parent $nefcon)) "LICENSE"

# One driver's package into a folder of its own, its library, setup
# information and catalogue in that order, with nefcon, its licence and the
# certificate the catalogue is signed with; the script that installs them
# beside the folder.
function Copy-DriverPackage([string]$Package, [string[]]$Parts, [string]$Folder,
        [string]$Script, [string]$Installer, [string]$Certificate, [bool]$Required, [string]$Builder) {
    $missing = $Parts | Where-Object { -not (Test-Path -LiteralPath (Join-Path $Package $_)) }
    if ($missing) {
        if ($Required) { throw "no complete driver package in $Package ($Builder builds it)" }
        Write-Warning "no complete driver package in $Package ($Builder builds it); the folder has no $Folder"
        return
    }
    $dest = Join-Path $OutDir $Folder
    New-Item -ItemType Directory -Path $dest -Force | Out-Null
    foreach ($part in $Parts) {
        Copy-Item -LiteralPath (Join-Path $Package $part) -Destination (Join-Path $dest $part) -Force
        Write-Host "  $Folder\$part"
    }
    Take (Join-Path $root $Script) $Installer
    Take $nefcon "$Folder\nefconc.exe"
    Take $nefconLicense "$Folder\nefcon-LICENSE.txt"
    # The public half of the certificate the catalogue is signed with, which
    # the install script makes the machine trust.
    $signer = (Get-AuthenticodeSignature -LiteralPath (Join-Path $Package $Parts[2])).SignerCertificate
    if (-not $signer) { throw "the catalogue in $Package is not signed" }
    [IO.File]::WriteAllBytes((Join-Path $dest $Certificate), $signer.Export("Cert"))
    Write-Host "  $Folder\$Certificate ($($signer.Thumbprint))"
}

Copy-DriverPackage (Join-Path $root "target\vdd-package") @("lowlat_idd.dll", "lowlat-idd.inf", "lowlat-idd.cat") `
    "driver" "scripts\vdd-install.ps1" "install-driver.ps1" "lowlat-idd.cer" $Portable "scripts/package-vdd.sh"
Copy-DriverPackage (Join-Path $root "target\hid-package") @("lowlat_hid.dll", "lowlat-hid.inf", "lowlat-hid.cat") `
    "input-driver" "scripts\hid-install.ps1" "install-input-driver.ps1" "lowlat-hid.cer" $false "scripts/package-hid.sh"
