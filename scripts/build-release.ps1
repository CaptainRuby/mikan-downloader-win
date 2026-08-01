$ErrorActionPreference = "Stop"

$root = Resolve-Path (Join-Path $PSScriptRoot "..")
$release = Join-Path $root "release"
$appName = "Mikan" + (-join (0x4E0B, 0x8F7D, 0x52A9, 0x624B | ForEach-Object { [char]$_ }))
$package = Get-Content -LiteralPath (Join-Path $root "package.json") -Raw | ConvertFrom-Json
$installerName = "$appName-v$($package.version).exe"
$portable = Join-Path $release $appName
$legacyPortable = Join-Path $release "$appName-win32-x64"
$portableExe = Join-Path $portable "$appName.exe"
$portableMarker = Join-Path $portable ".portable"
$preservedData = Join-Path ([System.IO.Path]::GetTempPath()) ("mikan-release-data-" + [guid]::NewGuid().ToString("N"))
$userProfile = [Environment]::GetFolderPath("UserProfile")
$cargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $userProfile ".cargo" }
$rustupHome = if ($env:RUSTUP_HOME) { $env:RUSTUP_HOME } else { Join-Path $userProfile ".rustup" }

$env:CARGO_HOME = $cargoHome
$env:RUSTUP_HOME = $rustupHome
$env:Path = "$(Join-Path $cargoHome 'bin');$env:Path"

Push-Location $root
try {
  $dataSource = @(
    (Join-Path $portable "data"),
    (Join-Path $legacyPortable "data"),
    (Join-Path $release "app\data")
  ) | Where-Object { Test-Path -LiteralPath $_ } | Select-Object -First 1
  if ($dataSource) {
    Copy-Item -LiteralPath $dataSource -Destination $preservedData -Recurse -Force
  }

  Get-CimInstance Win32_Process -ErrorAction SilentlyContinue |
    Where-Object {
      $_.ExecutablePath -and
      $_.ExecutablePath.StartsWith($release, [System.StringComparison]::OrdinalIgnoreCase)
    } |
    ForEach-Object {
      Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue
    }

  Get-ChildItem -LiteralPath $release -File -Force -ErrorAction SilentlyContinue |
    Remove-Item -Force
  $legacyApp = Join-Path $release "app"
  if (Test-Path -LiteralPath $legacyApp) {
    Remove-Item -LiteralPath $legacyApp -Recurse -Force
  }
  if (Test-Path -LiteralPath $legacyPortable) {
    Remove-Item -LiteralPath $legacyPortable -Recurse -Force
  }

  npm exec tauri -- build
  if ($LASTEXITCODE -ne 0) {
    throw "Tauri build failed with exit code $LASTEXITCODE"
  }

  $binary = Join-Path $root "src-tauri\target\release\mikan-rss-downloader.exe"
  if (-not (Test-Path -LiteralPath $binary)) {
    throw "Tauri executable was not found: $binary"
  }

  New-Item -ItemType Directory -Force -Path $portable | Out-Null
  Get-ChildItem -LiteralPath $portable -Force |
    Where-Object { $_.Name -ne "data" } |
    Remove-Item -Recurse -Force
  Copy-Item -LiteralPath $binary -Destination $portableExe -Force
  [System.IO.File]::WriteAllText($portableMarker, "Mikan portable installation`r`n")

  if ((Test-Path -LiteralPath $preservedData) -and -not (Test-Path -LiteralPath (Join-Path $portable "data"))) {
    Copy-Item -LiteralPath $preservedData -Destination (Join-Path $portable "data") -Recurse -Force
  }

  $bundleDir = Join-Path $root "src-tauri\target\release\bundle\nsis"
  $installer = Get-ChildItem -LiteralPath $bundleDir -Filter "*.exe" -File |
    Sort-Object LastWriteTime -Descending |
    Select-Object -First 1
  if (-not $installer) {
    throw "Tauri NSIS installer was not found in $bundleDir"
  }
  Copy-Item -LiteralPath $installer.FullName -Destination (Join-Path $release $installerName) -Force
} finally {
  if (Test-Path -LiteralPath $preservedData) {
    Remove-Item -LiteralPath $preservedData -Recurse -Force
  }
  Pop-Location
}
