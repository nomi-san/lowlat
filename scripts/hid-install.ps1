# Install the packaged input driver (target/hid-package, scripts/package-hid.sh)
# and its device, or remove both with -Remove. Run from an elevated shell. The
# device is made once; installing again updates the driver of the one there.
# This is the driver's only install: the service's install installs no driver,
# and the host only opens it (docs/07-platforms.md section 10).
#
# In a staged folder (scripts/stage-windows.ps1) this is install-input-driver.ps1,
# and the package, nefcon and the certificate the package is signed with are in
# input-driver\ beside it. That machine may never have seen the certificate, so
# it is trusted first; -Remove takes the trust back only when no package of
# this driver or of its brand's display driver is left, the display driver
# being signed with the same certificate.
#
# The driver's names -- its files, its hardware ID -- are its brand's, as its
# build wrote them into the package's package.json (docs/11-drivers.md section
# 1.10), so this installs and removes whichever brand the package was built as,
# and no other.
#
# An update or a removal while the lowlat service runs stops the service first
# and starts it again after, so the host's devices go before the driver does.
# A game's handle still open on a pad leaves the change finishing at a reboot:
# the device and its packages go and an install works at once, but the old
# driver host stays until then, and nefcon says so. A first install touches
# nothing else; a running host takes the driver up the next time its input is
# switched onto it.

param([switch]$Remove)

$ErrorActionPreference = 'Stop'
$package = Join-Path $PSScriptRoot 'input-driver'
$staged = Test-Path (Join-Path $package 'nefconc.exe')
if ($staged) {
    $nefcon = Join-Path $package 'nefconc.exe'
} else {
    $root = Split-Path -Parent $PSScriptRoot
    $nefcon = (& python.exe (Join-Path $root 'scripts\fetch-nefcon.py')).Trim()
    $package = Join-Path $root 'target\hid-package'
}
$names = Join-Path $package 'package.json'
if (-not (Test-Path $names)) { throw "no package at $package; run scripts/package-hid.sh first" }
$brand = Get-Content -Raw $names | ConvertFrom-Json
$certificate = if ($staged) { Join-Path $package $brand.certificate } else { $null }
$hwid = $brand.hardware_id
$className = 'System'
$classGuid = '4d36e97d-e325-11ce-bfc1-08002be10318'
$inf = Join-Path $package $brand.inf
$stores = @('Root', 'TrustedPublisher')
# The packages of this driver in the store, and of its brand's display
# driver: lowlat's own was `lowlat-vdd.inf` before its files took the name
# `lowlat-idd`.
$builtAs = [regex]::Escape($brand.inf)
$companion = [regex]::Escape($brand.companion_inf)
if ($brand.id -eq 'lowlat') { $companion = 'lowlat-[iv]dd\.inf' }
$serviceName = 'lowlatd'

function Invoke-Nefcon {
    & $nefcon @args
    if ($LASTEXITCODE -eq 3010) {
        Write-Warning 'done; a reboot finishes the change'
        return
    }
    if ($LASTEXITCODE -ne 0) { throw "nefconc $($args[0]) ended with exit code $LASTEXITCODE" }
}

function Test-Present {
    $null -ne (Get-PnpDevice -PresentOnly -ErrorAction SilentlyContinue |
        Where-Object { $_.HardwareID -contains $hwid })
}

# The packages in the store whose original names the pattern matches.
function Get-Packages([string]$Pattern) {
    $text = (pnputil /enum-drivers) -join "`n"
    foreach ($block in ($text -split "`n\s*`n")) {
        if ($block -match "Original Name:\s+($Pattern)" -and $block -match 'Published Name:\s+(oem\d+\.inf)') {
            $Matches[1]
        }
    }
}

$service = Get-Service -Name $serviceName -ErrorAction SilentlyContinue
$wasRunning = $service -and $service.Status -eq 'Running'
function Stop-Host {
    if ($wasRunning) {
        Write-Host "stopping $serviceName for the change"
        Stop-Service -Name $serviceName
    }
}
function Start-Host {
    if ($wasRunning) {
        Start-Service -Name $serviceName
        Write-Host "$serviceName started again"
    }
}

if ($Remove) {
    Stop-Host
    try {
        if (Test-Present) {
            Invoke-Nefcon --remove-device-node --hardware-id $hwid --class-guid $classGuid
        }
        if (Get-Packages $builtAs) {
            Invoke-Nefcon --remove-driver-store-package --inf-name $brand.inf
        }
    } finally {
        Start-Host
    }
    if ($certificate -and -not (Get-Packages "($builtAs|$companion)")) {
        $thumbprint = (New-Object System.Security.Cryptography.X509Certificates.X509Certificate2 $certificate).Thumbprint
        foreach ($store in $stores) {
            Remove-Item "Cert:\LocalMachine\$store\$thumbprint" -ErrorAction SilentlyContinue
        }
    }
    return
}

if (-not (Test-Path $inf)) { throw "no package at $inf; run scripts/package-hid.sh first" }

# A root, so the catalogue's signature chains; a trusted publisher, so the
# install goes on without asking (docs/11-drivers.md section 1).
if ($certificate) {
    foreach ($store in $stores) {
        Import-Certificate -FilePath $certificate -CertStoreLocation "Cert:\LocalMachine\$store" | Out-Null
    }
}

$update = Test-Present
if ($update) { Stop-Host }
try {
    if (-not $update) {
        Invoke-Nefcon --create-device-node --hardware-id $hwid --class-name $className `
            --class-guid $classGuid --no-duplicates
    }
    Invoke-Nefcon --install-driver --inf-path $inf
} finally {
    if ($update) { Start-Host }
}
