# Checks that every third-party crate in the build is under a license that allows use in this proprietary
# engine without publishing its source: permissive licenses only, no copyleft (GPL, LGPL, AGPL, MPL...).
# Run from the repository root: pwsh tools/check-licenses.ps1. Exits 1 if anything isn't allowed.

$ErrorActionPreference = 'Stop'

# SPDX ids we accept. For an "A OR B" expression one accepted choice is enough; for "A AND B" every part
# must be accepted.
$allowed = @(
    'MIT', 'MIT-0', 'Apache-2.0', 'Apache-2.0 WITH LLVM-exception', 'BSD-2-Clause', 'BSD-3-Clause',
    'ISC', 'Zlib', '0BSD', 'Unlicense', 'CC0-1.0', 'BSL-1.0', 'Unicode-3.0', 'Unicode-DFS-2016'
)

function Test-Expression([string]$expr) {
    # Old-style "MIT/Apache-2.0" means OR.
    $expr = ($expr -replace '\s*/\s*', ' OR ').Trim()
    foreach ($choice in ($expr -split '\s+OR\s+')) {
        $parts = $choice.Trim('(', ')', ' ') -split '\s+AND\s+'
        if (-not ($parts | Where-Object { $allowed -notcontains $_.Trim('(', ')', ' ') })) { return $true }
    }
    return $false
}

$meta = cargo metadata --format-version 1 --locked | ConvertFrom-Json
$workspace = $meta.workspace_members
$bad = @()
$count = 0
foreach ($pkg in $meta.packages) {
    if ($workspace -contains $pkg.id) { continue }
    $count++
    if (-not $pkg.license) {
        $bad += "{0} {1}: no license field (license file: {2})" -f $pkg.name, $pkg.version, $pkg.license_file
    } elseif (-not (Test-Expression $pkg.license)) {
        $bad += "{0} {1}: {2}" -f $pkg.name, $pkg.version, $pkg.license
    }
}

if ($bad) {
    Write-Output "Not allowed ($($bad.Count) of $count third-party crates):"
    $bad | ForEach-Object { Write-Output "  $_" }
    exit 1
}
Write-Output "All $count third-party crates are under allowed licenses."
