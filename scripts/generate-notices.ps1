[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$Destination
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$Destination = [IO.Path]::GetFullPath($Destination)

function Get-CargoAbout {
    $version = '0.9.3'
    # Official EmbarkStudios/cargo-about release asset SHA-256 (asset 677833098).
    $expectedHash = '9E2942533EF1C592C4A70BDB384CAE1550A3F5FC9AE40379B6662E3D2F16FE02'
    $tools = Join-Path ([Environment]::GetFolderPath('LocalApplicationData')) 'Print Atlas Build Tools'
    New-Item -ItemType Directory -Path $tools -Force | Out-Null
    $archive = Join-Path $tools "cargo-about-$version-x86_64-pc-windows-msvc.tar.gz"
    if (-not (Test-Path -LiteralPath $archive)) {
        $oldProtocol = [Net.ServicePointManager]::SecurityProtocol
        try {
            [Net.ServicePointManager]::SecurityProtocol = $oldProtocol -bor [Net.SecurityProtocolType]::Tls12
            Invoke-WebRequest -UseBasicParsing -Uri "https://github.com/EmbarkStudios/cargo-about/releases/download/$version/cargo-about-$version-x86_64-pc-windows-msvc.tar.gz" -OutFile $archive
        } finally {
            [Net.ServicePointManager]::SecurityProtocol = $oldProtocol
        }
    }
    if ((Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash -ne $expectedHash) {
        Remove-Item -LiteralPath $archive -Force
        throw 'cargo-about archive SHA-256 mismatch; the untrusted archive was deleted.'
    }
    $install = Join-Path $tools "cargo-about/$version"
    New-Item -ItemType Directory -Path $install -Force | Out-Null
    & tar.exe -xzf $archive -C $install
    if ($LASTEXITCODE -ne 0) { throw 'Could not extract verified cargo-about tool archive.' }
    $executable = Join-Path $install "cargo-about-$version-x86_64-pc-windows-msvc/cargo-about.exe"
    if (-not (Test-Path -LiteralPath $executable)) {
        throw "The cargo-about release archive did not contain $executable."
    }
    return $executable
}

if ($env:OS -ne 'Windows_NT') { throw 'Windows dependency-notice generation requires Windows.' }
$about = Get-CargoAbout
New-Item -ItemType Directory -Path $Destination -Force | Out-Null
Push-Location $root
try {
    # about.toml excludes the non-published local package via private.ignore.
    # Do not infer a Print Atlas source license or ignore unresolved dependencies.
    & $about generate --locked --fail --config (Join-Path $root 'about.toml') --output-file (Join-Path $Destination 'THIRD-PARTY-LICENSES.txt') (Join-Path $root 'installer/third-party.hbs')
    if ($LASTEXITCODE -ne 0) { throw 'Dependency license generation failed; refusing to package without complete notices.' }

    $metadataLines = & cargo.exe metadata --locked --format-version 1 --filter-platform x86_64-pc-windows-msvc
    if ($LASTEXITCODE -ne 0) { throw 'Could not resolve the locked dependency graph for original notices.' }
    $metadata = ($metadataLines -join "`n") | ConvertFrom-Json
    $nodes = @{}
    foreach ($node in $metadata.resolve.nodes) { $nodes[$node.id] = $node }
    $reachable = [Collections.Generic.HashSet[string]]::new()
    $pending = [Collections.Generic.Stack[string]]::new()
    $pending.Push($metadata.resolve.root)
    while ($pending.Count -gt 0) {
        $id = $pending.Pop()
        if (-not $reachable.Add($id)) { continue }
        foreach ($dependency in $nodes[$id].dependencies) { $pending.Push($dependency) }
    }

    $sources = [Collections.Generic.List[string]]::new()
    $sources.Add('Original source archives for unmodified Rust dependencies; no Print Atlas source license is granted here.')
    $sources.Add('Some additional build/dev dependency notices are conservatively retained with the binary dependency notices.')
    $sources.Add('')
    foreach ($package in ($metadata.packages | Sort-Object name, version)) {
        if (-not $package.source -or -not $reachable.Contains($package.id)) { continue }
        $packageRoot = Split-Path -Parent $package.manifest_path
        $packageDestination = Join-Path $Destination "crates/$($package.name)-$($package.version)"
        $sources.Add("$($package.name) $($package.version)")
        $sources.Add("Declared license: $($package.license)")
        $sources.Add("Repository: $($package.repository)")
        $declaredLicensePath = $null
        if ($package.license_file) {
            $declaredLicensePath = if ([IO.Path]::IsPathRooted($package.license_file)) {
                [IO.Path]::GetFullPath($package.license_file)
            } else {
                [IO.Path]::GetFullPath((Join-Path $packageRoot $package.license_file))
            }
        }
        $sources.Add("Source archive: https://crates.io/api/v1/crates/$($package.name)/$($package.version)/download")
        $sources.Add('')
        foreach ($file in (Get-ChildItem -LiteralPath $packageRoot -File -Recurse)) {
            $isNotice = $file.Name -match '(?i)(license|licence|copying|copyright|notice|^ofl\.|^ufl\.)'
            # egui fonts also include Hack-Regular.txt, which is its actual
            # copyright/license notice rather than a generically named LICENSE.
            $isFontNotice = $package.name -eq 'epaint_default_fonts' -and $file.Extension -eq '.txt' -and $file.Directory.Name -eq 'fonts'
            $isDeclaredLicense = $declaredLicensePath -and $file.FullName -eq $declaredLicensePath
            if (-not ($isNotice -or $isFontNotice -or $isDeclaredLicense)) { continue }
            $relative = $file.FullName.Substring($packageRoot.Length).TrimStart('\', '/')
            $target = Join-Path $packageDestination $relative
            New-Item -ItemType Directory -Path (Split-Path -Parent $target) -Force | Out-Null
            Copy-Item -LiteralPath $file.FullName -Destination $target -Force
        }
    }
    [IO.File]::WriteAllLines((Join-Path $Destination 'SOURCES.txt'), $sources, [Text.UTF8Encoding]::new($false))
    Copy-Item -LiteralPath (Join-Path $root 'Cargo.lock') -Destination (Join-Path $Destination 'Cargo.lock') -Force
} finally {
    Pop-Location
}
