# Authenticode signing for `tauri build` (inactive until a certificate exists; see docs/RELEASE.md).
#
# Enabled only by passing the overlay config:
#   npm run tauri build -- --config src-tauri/windows/signing.conf.json
# Tauri then calls this script once per file to sign (the app exe, the PDF worker, the installer),
# with the file path as the only argument.
#
# Certificate, from environment variables (never commit them):
#   ATHRIX_SIGN_THUMBPRINT      SHA-1 thumbprint of a certificate in the user's store (Cert:\CurrentUser\My)
#   -- or --
#   ATHRIX_SIGN_PFX             path to a .pfx file
#   ATHRIX_SIGN_PFX_PASSWORD    its password
# Optional:
#   ATHRIX_SIGNTOOL             full path to signtool.exe (default: the newest one in the Windows SDK)
#   ATHRIX_TIMESTAMP_URL        RFC 3161 timestamp server (default: http://timestamp.digicert.com)

param([Parameter(Mandatory = $true)][string]$File)

$ErrorActionPreference = 'Stop'

$signtool = $env:ATHRIX_SIGNTOOL
if (-not $signtool) {
    $kits = Join-Path ${env:ProgramFiles(x86)} 'Windows Kits\10\bin'
    $found = Get-ChildItem -Path $kits -Filter signtool.exe -Recurse -ErrorAction SilentlyContinue |
        Where-Object { $_.FullName -match '\\x64\\' } |
        Sort-Object FullName -Descending | Select-Object -First 1
    if (-not $found) { throw 'signtool.exe not found: install the Windows SDK or set ATHRIX_SIGNTOOL' }
    $signtool = $found.FullName
}

$timestamp = if ($env:ATHRIX_TIMESTAMP_URL) { $env:ATHRIX_TIMESTAMP_URL } else { 'http://timestamp.digicert.com' }
$signArgs = @('sign', '/fd', 'SHA256', '/tr', $timestamp, '/td', 'SHA256', '/d', 'Athrix')

if ($env:ATHRIX_SIGN_THUMBPRINT) {
    $signArgs += @('/sha1', $env:ATHRIX_SIGN_THUMBPRINT)
} elseif ($env:ATHRIX_SIGN_PFX) {
    $signArgs += @('/f', $env:ATHRIX_SIGN_PFX)
    if ($env:ATHRIX_SIGN_PFX_PASSWORD) { $signArgs += @('/p', $env:ATHRIX_SIGN_PFX_PASSWORD) }
} else {
    throw 'no signing certificate: set ATHRIX_SIGN_THUMBPRINT or ATHRIX_SIGN_PFX (see docs/RELEASE.md)'
}

& $signtool @signArgs $File
if ($LASTEXITCODE -ne 0) { throw "signtool failed ($LASTEXITCODE) on $File" }
