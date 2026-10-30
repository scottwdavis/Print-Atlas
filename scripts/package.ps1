[CmdletBinding()]
param(
    [switch]$ProvisionToolsOnly,
    [switch]$SkipBuild
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot

function Get-InnoCompiler {
    # Official release; SHA-256 is also recorded in microsoft/winget-pkgs:
    # manifests/j/JRSoftware/InnoSetup/6.6.1/JRSoftware.InnoSetup.installer.yaml
    $toolVersion = '6.6.1'
    $expectedHash = 'D243CE440C02705530699554FB9612B9B2BD7A2A90629CDB7F41E66F5FAEB91F'
    $tools = Join-Path ([Environment]::GetFolderPath('LocalApplicationData')) 'Print Atlas Build Tools'
    $installDir = Join-Path $tools "Inno Setup/$toolVersion"
    $compiler = Join-Path $installDir 'ISCC.exe'
    # Inno's compiler PE version resources report 0.0.0.0; use the installer's
    # registered product version and location instead.
    $registration = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\Inno Setup 6_is1'
    $installedVersion = Get-ItemPropertyValue -LiteralPath $registration -Name DisplayVersion -ErrorAction SilentlyContinue
    $registeredLocation = [string](Get-ItemPropertyValue -LiteralPath $registration -Name InstallLocation -ErrorAction SilentlyContinue)
    if ((Test-Path -LiteralPath $compiler) -and $installedVersion -eq $toolVersion -and
        $registeredLocation.TrimEnd('\') -eq $installDir.Replace('/', '\').TrimEnd('\')) {
        return $compiler
    }

    New-Item -ItemType Directory -Path $tools -Force | Out-Null
    $download = Join-Path $tools "innosetup-$toolVersion.exe"
    if (-not (Test-Path -LiteralPath $download)) {
        $oldProtocol = [Net.ServicePointManager]::SecurityProtocol
        try {
            [Net.ServicePointManager]::SecurityProtocol = $oldProtocol -bor [Net.SecurityProtocolType]::Tls12
            Invoke-WebRequest -UseBasicParsing -Uri "https://github.com/jrsoftware/issrc/releases/download/is-6_6_1/innosetup-$toolVersion.exe" -OutFile $download
        } finally {
            [Net.ServicePointManager]::SecurityProtocol = $oldProtocol
        }
    }
    $actualHash = (Get-FileHash -LiteralPath $download -Algorithm SHA256).Hash
    if ($actualHash -ne $expectedHash) {
        Remove-Item -LiteralPath $download -Force
        throw 'Inno Setup installer SHA-256 mismatch. The untrusted download was deleted; no installer was run.'
    }
    # /CURRENTUSER and the explicit user-writable directory avoid elevation.
    $arguments = @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', '/SP-', '/CURRENTUSER', '/NOICONS', "/DIR=`"$installDir`"")
    $process = Start-Process -FilePath $download -ArgumentList $arguments -Wait -PassThru
    if ($process.ExitCode -notin @(0, 3010)) {
        throw "Inno Setup provisioning failed with exit code $($process.ExitCode)."
    }
    if (-not (Test-Path -LiteralPath $compiler)) {
        throw "Inno Setup did not install its compiler at $compiler."
    }
    $installedVersion = Get-ItemPropertyValue -LiteralPath $registration -Name DisplayVersion -ErrorAction Stop
    if ($installedVersion -ne $toolVersion) {
        throw "The installed Inno Setup compiler has unexpected version $installedVersion."
    }
    return $compiler
}

function Get-NasmDirectory {
    # Official win64 archive downloaded over HTTPS and pinned by SHA-256.
    $version = '2.16.03'
    $expectedHash = '3EE4782247BCB874378D02F7EAB4E294A84D3D15F3F6EE2DE2F47A46AA7226E6'
    $tools = Join-Path ([Environment]::GetFolderPath('LocalApplicationData')) 'Print Atlas Build Tools'
    New-Item -ItemType Directory -Path $tools -Force | Out-Null
    $archive = Join-Path $tools "nasm-$version-win64.zip"
    if (-not (Test-Path -LiteralPath $archive)) {
        $oldProtocol = [Net.ServicePointManager]::SecurityProtocol
        try {
            [Net.ServicePointManager]::SecurityProtocol = $oldProtocol -bor [Net.SecurityProtocolType]::Tls12
            Invoke-WebRequest -UseBasicParsing -Uri "https://www.nasm.us/pub/nasm/releasebuilds/$version/win64/nasm-$version-win64.zip" -OutFile $archive
        } finally {
            [Net.ServicePointManager]::SecurityProtocol = $oldProtocol
        }
    }
    if ((Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash -ne $expectedHash) {
        Remove-Item -LiteralPath $archive -Force
        throw 'NASM archive SHA-256 mismatch. The untrusted download was deleted; no binary was run.'
    }
    # Re-extract only from the verified archive so stale cached executables are replaced.
    $destination = Join-Path $tools 'NASM'
    Expand-Archive -LiteralPath $archive -DestinationPath $destination -Force
    $bin = Join-Path $destination "nasm-$version"
    if (-not (Test-Path -LiteralPath (Join-Path $bin 'nasm.exe'))) {
        throw 'The verified NASM archive did not contain nasm.exe.'
    }
    return $bin
}

if ($env:OS -ne 'Windows_NT') {
    throw 'Print Atlas Windows packaging requires Windows and the MSVC Rust toolchain.'
}
$compiler = Get-InnoCompiler
$nasmDirectory = Get-NasmDirectory
$env:PATH = "$nasmDirectory;$env:PATH"
if ($env:GITHUB_PATH) {
    [IO.File]::AppendAllText($env:GITHUB_PATH, "$nasmDirectory`n", [Text.UTF8Encoding]::new($false))
}
if ($ProvisionToolsOnly) {
    Write-Output "Inno Setup compiler ready: $compiler"
    Write-Output "NASM assembler ready: $(Join-Path $nasmDirectory 'nasm.exe')"
    return
}

$manifest = Get-Content -LiteralPath (Join-Path $root 'Cargo.toml') -Raw
$packageSection = [regex]::Match($manifest, '(?ms)^\[package\]\s*\r?\n(?<body>.*?)(?=^\[|\z)')
$versionMatch = [regex]::Match($packageSection.Groups['body'].Value, '(?m)^version\s*=\s*"(?<version>[^"]+)"')
if (-not $versionMatch.Success) {
    throw 'Cargo.toml must declare a literal package.version for release packaging.'
}
$version = $versionMatch.Groups['version'].Value
if ($version -notmatch '^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$') {
    throw "Invalid package version: $version"
}
$fileVersion = $version -replace '[-+].*$', ''
if ($env:GITHUB_REF_TYPE -eq 'tag' -and $env:GITHUB_REF_NAME -ne "v$version") {
    throw "Release tag '$($env:GITHUB_REF_NAME)' must match Cargo.toml version 'v$version'."
}

& (Join-Path $PSScriptRoot 'fetch-pdfium.ps1')
if (-not (Test-Path -LiteralPath (Join-Path $root 'runtime/pdfium.dll'))) {
    throw 'The PDF runtime fetch completed without runtime/pdfium.dll.'
}
if (-not (Test-Path -LiteralPath (Join-Path $root 'runtime/licenses/pdfium/LICENSE'))) {
    throw 'The PDF runtime license is missing; refusing to redistribute it without notices.'
}

Push-Location $root
try {
    if (-not $SkipBuild) {
        if (-not (Test-Path -LiteralPath (Join-Path $root 'Cargo.lock'))) {
            throw 'Cargo.lock is required. Generate and commit it before packaging.'
        }
        $oldRustFlags = $env:RUSTFLAGS
        try {
            # A portable application must not require a separately installed MSVC runtime.
            $env:RUSTFLAGS = "$oldRustFlags -C target-feature=+crt-static".Trim()
            & cargo.exe build --locked --release --target x86_64-pc-windows-msvc
            if ($LASTEXITCODE -ne 0) { throw "Cargo release build failed with exit code $LASTEXITCODE." }
        } finally {
            $env:RUSTFLAGS = $oldRustFlags
        }
    }

    $binary = Join-Path $root 'target/x86_64-pc-windows-msvc/release/print-atlas.exe'
    if (-not (Test-Path -LiteralPath $binary)) {
        throw "The Windows x64 release executable is missing: $binary"
    }
    $output = Join-Path $root 'release'
    $staging = Join-Path $output '.package-staging'
    $portable = Join-Path $staging 'Print Atlas'
    if (Test-Path -LiteralPath $staging) { Remove-Item -LiteralPath $staging -Recurse -Force }
    New-Item -ItemType Directory -Path $portable -Force | Out-Null
    try {
        Copy-Item -LiteralPath $binary -Destination (Join-Path $portable 'Print Atlas.exe')
        Copy-Item -LiteralPath (Join-Path $root 'runtime') -Destination (Join-Path $portable 'runtime') -Recurse
        Copy-Item -LiteralPath (Join-Path $root 'README.md') -Destination (Join-Path $portable 'README.md')
        Copy-Item -LiteralPath (Join-Path $root 'LICENSE') -Destination (Join-Path $portable 'LICENSE')
        & (Join-Path $PSScriptRoot 'generate-notices.ps1') -Destination (Join-Path $portable 'third-party-licenses')

        # Crate licenses can predate 1980; ZIP timestamps must be within 1980..2107.
        foreach ($entry in Get-ChildItem -LiteralPath $portable -Recurse -Force) {
            if ($entry.LastWriteTime.Year -lt 1980 -or $entry.LastWriteTime.Year -gt 2107) {
                $entry.LastWriteTime = [datetime]'1980-01-02T00:00:00'
            }
        }

        $releaseRuntime = Join-Path $output 'runtime'
        if (Test-Path -LiteralPath $releaseRuntime) { Remove-Item -LiteralPath $releaseRuntime -Recurse -Force }
        Copy-Item -LiteralPath (Join-Path $portable 'runtime') -Destination $releaseRuntime -Recurse
        $releaseNotices = Join-Path $output 'third-party-licenses'
        if (Test-Path -LiteralPath $releaseNotices) { Remove-Item -LiteralPath $releaseNotices -Recurse -Force }
        Copy-Item -LiteralPath (Join-Path $portable 'third-party-licenses') -Destination $releaseNotices -Recurse

        # Versioned local launchers let the next release coexist with an open older session.
        Copy-Item -LiteralPath (Join-Path $portable 'Print Atlas.exe') -Destination (Join-Path $output "Print Atlas $version.exe") -Force

        $archive = Join-Path $output "Print-Atlas-$version-windows-x64.zip"
        Compress-Archive -LiteralPath $portable -DestinationPath $archive -CompressionLevel Optimal -Force
        & $compiler "/DAppVersion=$version" "/DAppFileVersion=$fileVersion" "/DSourceDir=$portable" "/DOutputDir=$output" (Join-Path $root 'installer/PrintAtlas.iss')
        if ($LASTEXITCODE -ne 0) { throw "Inno Setup compilation failed with exit code $LASTEXITCODE." }
        $setup = Join-Path $output 'Print-Atlas-Setup.exe'
        if (-not (Test-Path -LiteralPath $setup)) { throw 'Inno Setup did not produce Print-Atlas-Setup.exe.' }
        $checksums = foreach ($artifact in @($setup, $archive)) {
            $hash = (Get-FileHash -LiteralPath $artifact -Algorithm SHA256).Hash.ToLowerInvariant()
            '{0}  {1}' -f $hash, (Split-Path -Leaf $artifact)
        }
        [IO.File]::WriteAllLines((Join-Path $output 'SHA256SUMS.txt'), $checksums, [Text.UTF8Encoding]::new($false))
        Write-Output "Packaged Print Atlas $version in $output"
    } finally {
        if (Test-Path -LiteralPath $staging) { Remove-Item -LiteralPath $staging -Recurse -Force }
    }
} finally {
    Pop-Location
}
