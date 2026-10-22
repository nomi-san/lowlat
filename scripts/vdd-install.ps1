# Install the packaged display driver (target/vdd-package, scripts/package-vdd.sh)
# and its device, or remove both with -Remove. Run from an elevated shell. The
# device is made once; installing again updates the driver of the one there.
#
# In the portable folder (scripts/stage-windows.ps1 -Portable) this is
# install-driver.ps1, and the package, devcon and the certificate the package
# is signed with are in driver\ beside it. That machine has never seen the
# certificate, so it is trusted first, and -Remove takes the trust back.

param([switch]$Remove)

$ErrorActionPreference = 'Stop'
$package = Join-Path $PSScriptRoot 'driver'
if (Test-Path (Join-Path $package 'devcon.exe')) {
    $devcon = Join-Path $package 'devcon.exe'
    $certificate = Join-Path $package 'lowlat-idd.cer'
} else {
    $root = Split-Path -Parent $PSScriptRoot
    $kit = (& python.exe (Join-Path $root 'scripts\fetch-wdk.py')).Trim()
    $devcon = Join-Path $kit 'c\tools\10.0.26100.0\x64\devcon.exe'
    $package = Join-Path $root 'target\vdd-package'
    $certificate = $null
}
$hwid = 'Root\lowlat-vdd'
$inf = Join-Path $package 'lowlat-idd.inf'
$stores = @('Root', 'TrustedPublisher')

function Present {
    $found = & $devcon find $hwid
    return -not ($found -match 'No matching devices found')
}

if ($Remove) {
    if (Present) { & $devcon remove $hwid }
    # Every package of this driver in the store, by either name it was built
    # as: `lowlat-vdd.inf` before its files took the name `lowlat-idd`.
    $text = (pnputil /enum-drivers) -join "`n"
    $blocks = $text -split "`n\s*`n"
    foreach ($block in $blocks) {
        if ($block -match 'Original Name:\s+lowlat-[iv]dd\.inf' -and $block -match 'Published Name:\s+(oem\d+\.inf)') {
            pnputil /delete-driver $Matches[1] /uninstall /force
        }
    }
    if ($certificate) {
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

if (Present) {
    & $devcon update $inf $hwid
} else {
    & $devcon install $inf $hwid
}
