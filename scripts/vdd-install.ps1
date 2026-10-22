# Install the packaged display driver (target/vdd-package, scripts/package-vdd.sh)
# and its device, or remove both with -Remove. Run from an elevated shell. The
# device is made once; installing again updates the driver of the one there.
# This is the driver's only install: the service's install installs no driver
# (docs/07-platforms.md section 10).
#
# In a staged folder (scripts/stage-windows.ps1, either form) this is
# install-driver.ps1, and the package, nefcon and the certificate the package
# is signed with are in driver\ beside it. That machine may never have seen the
# certificate, so it is trusted first; -Remove takes the trust back only when
# no lowlat driver package is left, the input driver being signed with the
# same certificate.

param([switch]$Remove)

$ErrorActionPreference = 'Stop'
$package = Join-Path $PSScriptRoot 'driver'
if (Test-Path (Join-Path $package 'nefconc.exe')) {
    $nefcon = Join-Path $package 'nefconc.exe'
    $certificate = Join-Path $package 'lowlat-idd.cer'
} else {
    $root = Split-Path -Parent $PSScriptRoot
    $nefcon = (& python.exe (Join-Path $root 'scripts\fetch-nefcon.py')).Trim()
    $package = Join-Path $root 'target\vdd-package'
    $certificate = $null
}
$hwid = 'Root\lowlat-vdd'
$className = 'Display'
$classGuid = '4d36e968-e325-11ce-bfc1-08002be10318'
$inf = Join-Path $package 'lowlat-idd.inf'
$stores = @('Root', 'TrustedPublisher')

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

# The packages of lowlat's drivers in the store, by the names they were built as.
function Get-Packages([string]$Pattern) {
    $text = (pnputil /enum-drivers) -join "`n"
    foreach ($block in ($text -split "`n\s*`n")) {
        if ($block -match "Original Name:\s+($Pattern)" -and $block -match 'Published Name:\s+(oem\d+\.inf)') {
            $Matches[1]
        }
    }
}

if ($Remove) {
    if (Test-Present) {
        Invoke-Nefcon --remove-device-node --hardware-id $hwid --class-guid $classGuid
    }
    # Every package of this driver in the store, by either name it was built
    # as: `lowlat-vdd.inf` before its files took the name `lowlat-idd`.
    foreach ($published in (Get-Packages 'lowlat-[iv]dd\.inf')) {
        pnputil /delete-driver $published /uninstall /force
    }
    if ($certificate -and -not (Get-Packages 'lowlat-(idd|vdd|hid)\.inf')) {
        $thumbprint = (New-Object System.Security.Cryptography.X509Certificates.X509Certificate2 $certificate).Thumbprint
        foreach ($store in $stores) {
            Remove-Item "Cert:\LocalMachine\$store\$thumbprint" -ErrorAction SilentlyContinue
        }
    }
    return
}

if (-not (Test-Path $inf)) { throw "no package at $inf; run scripts/package-vdd.sh first" }

# A root, so the catalogue's signature chains; a trusted publisher, so the
# install goes on without asking (docs/11-drivers.md section 1).
if ($certificate) {
    foreach ($store in $stores) {
        Import-Certificate -FilePath $certificate -CertStoreLocation "Cert:\LocalMachine\$store" | Out-Null
    }
}

# The folder kept monitors come back from (docs/11-drivers.md section 1.4): the
# system account and administrators in full, users to write -- so an
# unelevated controller keeps a display -- and the driver's own account,
# LocalService, to read. Nothing inherited from the program-data directory.
# Left in place by -Remove, as a person's settings are.
$folder = Join-Path $env:ProgramData 'lowlat\display'
New-Item -ItemType Directory -Path $folder -Force | Out-Null
$acl = New-Object System.Security.AccessControl.DirectorySecurity
$acl.SetAccessRuleProtection($true, $false)
$inherit = [System.Security.AccessControl.InheritanceFlags]'ContainerInherit, ObjectInherit'
foreach ($rule in @(
        @('S-1-5-18', 'FullControl'),
        @('S-1-5-32-544', 'FullControl'),
        @('S-1-5-32-545', 'Modify'),
        @('S-1-5-19', 'ReadAndExecute'))) {
    $sid = New-Object System.Security.Principal.SecurityIdentifier($rule[0])
    $acl.AddAccessRule((New-Object System.Security.AccessControl.FileSystemAccessRule(
                $sid, $rule[1], $inherit, 'None', 'Allow')))
}
Set-Acl -Path $folder -AclObject $acl

if (-not (Test-Present)) {
    Invoke-Nefcon --create-device-node --hardware-id $hwid --class-name $className `
        --class-guid $classGuid --no-duplicates
}
Invoke-Nefcon --install-driver --inf-path $inf
