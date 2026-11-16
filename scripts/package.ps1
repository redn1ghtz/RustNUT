param([switch]$Build)

$ErrorActionPreference = 'Stop'
Set-Location (Join-Path $PSScriptRoot '..')
$target = 'i686-pc-windows-msvc'
if ($Build) {
    cargo build --locked --release --target $target
    if ($LASTEXITCODE) { throw 'Rust release build failed.' }
}

$distribution = New-Item -ItemType Directory -Force 'dist/RustNUT-windows-x86'
$source = (Resolve-Path "target/$target/release/rustnut.exe").Path
$stream = [IO.File]::OpenRead($source)
$reader = [IO.BinaryReader]::new($stream)
try {
    $stream.Position = 0x3c
    $stream.Position = $reader.ReadUInt32()
    if ($reader.ReadUInt32() -ne 0x00004550 -or $reader.ReadUInt16() -ne 0x014c) {
        throw 'The release executable must be a Windows x86 PE.'
    }
} finally {
    $reader.Dispose()
}

Copy-Item $source, README.md, LICENSE -Destination $distribution.FullName
$docs = New-Item -ItemType Directory -Force (Join-Path $distribution 'docs')
Copy-Item docs/verification.md -Destination $docs.FullName
$exeHash = (Get-FileHash -Algorithm SHA256 $source).Hash.ToLowerInvariant()
Set-Content -Encoding ascii (Join-Path $distribution 'SHA256SUMS.txt') "$exeHash  rustnut.exe"

$metadata = cargo metadata --locked --format-version 1 --filter-platform $target | ConvertFrom-Json
if ($LASTEXITCODE) { throw 'Could not read Cargo license metadata.' }
$notices = New-Item -ItemType Directory -Force (Join-Path $distribution 'licenses')
$list = @('# RustNUT dependency notices', '', 'Cargo packages resolved for the Windows x86 target', '')
foreach ($package in $metadata.packages | Sort-Object name) {
    if ($package.id -eq $metadata.resolve.root) { continue }
    $name = "$($package.name)-$($package.version)"
    $folder = New-Item -ItemType Directory -Force (Join-Path $notices $name)
    $licenses = Get-ChildItem -LiteralPath (Split-Path $package.manifest_path) -File |
        Where-Object { $_.Name -match '^(LICENSE|COPYING|NOTICE|UNLICENSE)' }
    if (!$licenses) { throw "License files missing for $name." }
    $licenses | Copy-Item -Destination $folder.FullName
    $list += "- $name — $($package.license)"
}
$rust = New-Item -ItemType Directory -Force (Join-Path $notices 'Rust')
$legal = Join-Path (rustc --print sysroot) 'share/doc/rust'
$rustTerms = New-Item -ItemType Directory -Force (Join-Path $rust 'licenses')
Copy-Item (Join-Path $legal 'COPYRIGHT-library.html') -Destination $rust.FullName
Get-ChildItem -LiteralPath (Join-Path $legal 'licenses') -File | Copy-Item -Destination $rustTerms.FullName
$list += '', 'Rust standard library notices are in licenses/Rust.'
Set-Content -Encoding utf8 (Join-Path $distribution 'THIRD-PARTY.md') $list

$zip = Join-Path $distribution.Parent 'RustNUT-windows-x86.zip'
Compress-Archive -Path (Join-Path $distribution '*') -DestinationPath $zip -Force
$zipHash = (Get-FileHash -Algorithm SHA256 $zip).Hash.ToLowerInvariant()
Set-Content -Encoding ascii "$zip.sha256" "$zipHash  RustNUT-windows-x86.zip"
Get-Item $source, $zip | Select-Object Name, Length
Write-Output "SHA256 $exeHash"
