[CmdletBinding()]
param(
    [string]$Destination = ''
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
if ([string]::IsNullOrWhiteSpace($Destination)) {
    $Destination = Join-Path (Split-Path $PSScriptRoot -Parent) 'runtime'
}

# Non-V8/non-XFA upstream build: PDF previews never execute document JavaScript.
# Immutable GitHub release asset 678551894, PDFium 158.0.8125.0 (2026-10-26).
# SHA-256 is pinned from the upstream release API asset digest, not downloaded checksums.
$Version = '8125'
$Url = 'https://github.com/bblanchon/pdfium-binaries/releases/download/chromium/8125/pdfium-win-x64.tgz'
$ExpectedSha256 = '9a964261760cff52570aabf038ab346d3a5034dd8b30d0d5fe8861235d356b8b'
$TempDirectory = Join-Path ([System.IO.Path]::GetTempPath()) ('print-atlas-pdfium-' + [Guid]::NewGuid().ToString('N'))
$Archive = Join-Path $TempDirectory 'pdfium-win-x64.tgz'
$Extracted = Join-Path $TempDirectory 'extracted'

try {
    New-Item -ItemType Directory -Path $Extracted -Force | Out-Null
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
    Invoke-WebRequest -Uri $Url -OutFile $Archive -UseBasicParsing
    $ActualSha256 = (Get-FileHash -LiteralPath $Archive -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($ActualSha256 -cne $ExpectedSha256) {
        throw "PDFium checksum mismatch. Expected $ExpectedSha256; got $ActualSha256. Nothing was installed."
    }
    # Windows 10+ supplies bsdtar. Extract only the DLL, provenance, and complete notices.
    & tar.exe -xzf $Archive -C $Extracted 'bin/pdfium.dll' 'LICENSE' 'licenses' 'VERSION' 'args.gn'
    if ($LASTEXITCODE -ne 0) { throw "PDFium archive extraction failed ($LASTEXITCODE)." }
    $Dll = Join-Path $Extracted 'bin/pdfium.dll'
    $Notices = Join-Path $Extracted 'licenses'
    if (!(Test-Path -LiteralPath $Dll -PathType Leaf) -or
        !(Test-Path -LiteralPath (Join-Path $Notices 'pdfium.txt') -PathType Leaf)) {
        throw 'The pinned PDFium archive is missing its DLL or license notices.'
    }

    $LicenseDirectory = Join-Path $Destination 'licenses/pdfium'
    New-Item -ItemType Directory -Path $LicenseDirectory -Force | Out-Null
    Copy-Item -LiteralPath $Dll -Destination (Join-Path $Destination 'pdfium.dll') -Force
    Copy-Item -LiteralPath (Join-Path $Extracted 'LICENSE') -Destination (Join-Path $LicenseDirectory 'LICENSE') -Force
    Copy-Item -Path (Join-Path $Notices '*') -Destination $LicenseDirectory -Recurse -Force
    Copy-Item -LiteralPath (Join-Path $Extracted 'VERSION') -Destination (Join-Path $LicenseDirectory 'VERSION') -Force
    Copy-Item -LiteralPath (Join-Path $Extracted 'args.gn') -Destination (Join-Path $LicenseDirectory 'args.gn') -Force
    @"
PDFium upstream binary: https://github.com/bblanchon/pdfium-binaries
Release: chromium/$Version (non-V8 Windows x64)
Source archive: $Url
Archive SHA-256: $ExpectedSha256
PDFium source and third-party notices are reproduced in this directory.
"@ | Set-Content -LiteralPath (Join-Path $LicenseDirectory 'ORIGIN.txt') -Encoding UTF8
    Write-Host "Provisioned PDFium chromium/$Version and all upstream license notices in $Destination"
}
finally {
    if (Test-Path -LiteralPath $TempDirectory) { Remove-Item -LiteralPath $TempDirectory -Recurse -Force }
}
