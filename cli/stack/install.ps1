<#
Installs the qloudpulse client on Windows (x64 or ARM64): picks the zip for this machine, checks it against the
release's SHA256SUMS, puts qloudpulse-stack.exe in %LOCALAPPDATA%\Programs\qloudpulse (or -Dir) and adds that to the user's
PATH.

  irm https://github.com/ulfmansson/qloudpulse-public/releases/download/qloudpulse-stack-<version>/install.ps1 | iex

The copy attached to a release installs that release; this copy in the repository installs the newest release, or
-Version (or $env:QLOUDPULSE_VERSION). When a client is already installed (qloudpulse-stack.exe, or qloudpulse.exe up
to 0.3.1) it says which version that is and which this would install, and asks before replacing it; one already at
that version is left alone. -Version, -Yes, or no console to ask in (a pipeline) goes ahead without asking. Through
irm | iex options cannot be passed, so each has an environment variable too:

  -Version <version>   $env:QLOUDPULSE_VERSION      the client version (default: the release this script came with;
                                                    for the copy in the repository, the newest release)
  -Dir <dir>           $env:QLOUDPULSE_INSTALL_DIR  where qloudpulse-stack.exe goes
  -Target <target>     the build to install, x86_64-pc-windows-gnu or aarch64-pc-windows-gnullvm (default: this
                       machine's; an ARM64 machine gets the ARM64 build even from an x64 PowerShell)
  -Base <url or dir>   $env:QLOUDPULSE_DOWNLOAD_BASE  where the zips and SHA256SUMS are (default: the release)
  -Yes                 $env:QLOUDPULSE_ASSUME_YES=1  replace an installed client with another version without asking
  -Attest              also check the zip's signed build provenance with gh (gh attestation verify)
  -NoModifyPath        $env:QLOUDPULSE_NO_MODIFY_PATH  leave the user's PATH alone

On a GitHub Actions runner the directory goes to $env:GITHUB_PATH instead of the user's PATH. Plain ASCII, for
Windows PowerShell 5.1 and PowerShell 7.
#>
param(
  [string]$Version = $env:QLOUDPULSE_VERSION,
  [string]$Dir = $env:QLOUDPULSE_INSTALL_DIR,
  [string]$Target = '',
  [string]$Base = $env:QLOUDPULSE_DOWNLOAD_BASE,
  [switch]$Attest,
  [switch]$NoModifyPath,
  [switch]$Yes
)

# A release's copy has its version here (cli/stack/release.sh writes it); this one has none.
$QloudpulseDefaultVersion = ''

function Install-Qloudpulse {
  param([string]$Version, [string]$Dir, [string]$Target, [string]$Base, [bool]$Attest, [bool]$NoModifyPath,
    [bool]$Yes)
  $ErrorActionPreference = 'Stop'
  $ProgressPreference = 'SilentlyContinue' # Windows PowerShell's progress bar slows a download many times over
  $Repo = 'ulfmansson/qloudpulse-public'

  # -1, 0 or 1 as version a is older than, the same as or newer than b.
  function Compare-QloudpulseVersion([string]$a, [string]$b) {
    $x = @($a.Split('.') | ForEach-Object { [long]$_ })
    $y = @($b.Split('.') | ForEach-Object { [long]$_ })
    for ($i = 0; $i -lt [Math]::Max($x.Count, $y.Count); $i++) {
      $p = if ($i -lt $x.Count) { $x[$i] } else { 0 }
      $q = if ($i -lt $y.Count) { $y[$i] } else { 0 }
      if ($p -lt $q) { return -1 }
      if ($p -gt $q) { return 1 }
    }
    return 0
  }

  # Where the version comes from: asked for (-Version or $env:QLOUDPULSE_VERSION), this installer's release, or the
  # newest.
  if ($Version) { $From = 'asked' }
  elseif ($QloudpulseDefaultVersion) { $From = 'release'; $Version = $QloudpulseDefaultVersion }
  else { $From = 'newest' }
  if ($From -eq 'newest') {
    if ($Base) { throw 'qloudpulse install: no version: pass -Version or set $env:QLOUDPULSE_VERSION with -Base' }
    # The newest client release, under either name. Not GitHub's "latest": that may be another tool's release, in the
    # same repository.
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
    try {
      $releases = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases?per_page=100" -UseBasicParsing `
        -Headers @{ Accept = 'application/vnd.github+json' }
    } catch {
      throw ("qloudpulse install: could not list the releases on github.com/$Repo ($($_.Exception.Message)); " +
        'pass -Version')
    }
    $Version = $releases | ForEach-Object { $_.tag_name } |
      Where-Object { $_ -match '^(qloudpulse-stack|qloudpulse-cli)-[0-9]+(\.[0-9]+)+$' } |
      ForEach-Object { $_ -replace '^(qloudpulse-stack|qloudpulse-cli)-', '' } | Sort-Object { [version]$_ } -Descending |
      Select-Object -First 1
    if (-not $Version) { throw "qloudpulse install: github.com/$Repo has no qloudpulse-stack release yet; pass -Version" }
  }
  $Version = $Version -replace '^(v|qloudpulse-stack-|qloudpulse-cli-)', ''
  if ($Version -notmatch '^[0-9]+(\.[0-9]+)*$') { throw "qloudpulse install: not a version: $Version (for example 0.4.0)" }
  if ((Compare-QloudpulseVersion $Version '0.4.0') -lt 0) {
    # Up to 0.3.1 the client was called qloudpulse: those releases are tagged qloudpulse-cli-<version>, their zips are
    # qloudpulse-<version>-<target>.zip and the binary in them is qloudpulse.exe.
    $Bin = 'qloudpulse'; $Tag = "qloudpulse-cli-$Version"; $Workflow = 'cli-release.yml'
  } else {
    $Bin = 'qloudpulse-stack'; $Tag = "qloudpulse-stack-$Version"; $Workflow = 'cli-stack-release.yml'
  }
  if (-not $Base) { $Base = "https://github.com/$Repo/releases/download/$Tag" }
  $DirGiven = [bool]$Dir
  if (-not $Dir) { $Dir = Join-Path $env:LOCALAPPDATA 'Programs\qloudpulse' }

  # A client already installed: in -Dir first, then on PATH, under either name.
  $Old = $null; $OldVersion = $null
  # On PATH too, unless -Dir (or $env:QLOUDPULSE_INSTALL_DIR) names where it goes: one elsewhere is then another install.
  $candidates = @((Join-Path $Dir 'qloudpulse-stack.exe'), (Join-Path $Dir 'qloudpulse.exe'))
  if (-not $DirGiven) {
    $candidates += @(Get-Command qloudpulse-stack, qloudpulse -CommandType Application -ErrorAction SilentlyContinue |
      ForEach-Object { $_.Path })
  }
  foreach ($f in $candidates) {
    if (-not $f -or -not (Test-Path -LiteralPath $f -PathType Leaf)) { continue }
    # "qloudpulse-stack 0.4.0 (1ab39062b 2026-11-21)", or "qloudpulse 0.3.1" before the rename.
    try { $said = & $f --version 2>$null | Select-Object -First 1 } catch { continue }
    if ("$said" -match '^\S+ ([0-9]+(\.[0-9]+)*)') { $Old = $f; $OldVersion = $Matches[1]; break }
  }
  if ($Old) {
    $offer = switch ($From) {
      'newest' { "the newest release is $Version" }
      'release' { "this installer is for $Version" }
      default { "you asked for $Version" }
    }
    $cmp = Compare-QloudpulseVersion $OldVersion $Version
    if ($cmp -eq 0) {
      Write-Host "qloudpulse install: $Old is $OldVersion and ${offer}: already up to date, nothing to do"
      return
    }
    Write-Host "qloudpulse install: $Old is $OldVersion; $offer"
    if ($From -ne 'asked' -and -not $Yes) {
      if ($cmp -lt 0) { $question = "Upgrade to $Version? [Y/n]"; $default = 'y' }
      else { $question = "Replace it with the older $Version? [y/N]"; $default = 'n' }
      # Asked only in a console someone can answer; a pipeline, with its input redirected or none, goes ahead.
      if ([Environment]::UserInteractive -and -not [Console]::IsInputRedirected -and -not $env:CI) {
        try { $answer = Read-Host "qloudpulse install: $question" } catch { $answer = 'y' } # -NonInteractive
        if (-not $answer) { $answer = $default }
        if ($answer -notmatch '^(y|yes)$') {
          Write-Host "qloudpulse install: left $Old at $OldVersion"
          return
        }
      } else {
        Write-Host 'qloudpulse install: no console to ask in; going ahead'
      }
    }
  }

  if (-not $Target) {
    # The machine's architecture, not this process's: an x64 PowerShell runs emulated on Windows on ARM and its
    # $env:PROCESSOR_ARCHITECTURE says AMD64. The system's own environment in the registry names the hardware.
    $arch = $null
    try {
      $arch = (Get-ItemProperty 'HKLM:\SYSTEM\CurrentControlSet\Control\Session Manager\Environment').PROCESSOR_ARCHITECTURE
    } catch { }
    if (-not $arch) {
      try { $arch = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString() } catch { }
    }
    if (-not $arch) { $arch = $env:PROCESSOR_ARCHITECTURE }
    switch -regex ($arch) {
      '^(AMD64|X64)$' { $Target = 'x86_64-pc-windows-gnu' }
      '^ARM64$' { $Target = 'aarch64-pc-windows-gnullvm' }
      default { throw "qloudpulse install: no Windows build for $arch; build from source (cli/stack/README.md)" }
    }
  }
  $Zip = "$Bin-$Version-$Target.zip"
  Write-Host "qloudpulse install: $Bin $Version for $Target"

  if ($Base -match '^https?://') {
    # Windows PowerShell 5.1 may not offer TLS 1.2 unless asked; GitHub takes nothing older.
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
  }
  $Tmp = Join-Path ([IO.Path]::GetTempPath()) ("qloudpulse-" + [Guid]::NewGuid().ToString('N'))
  New-Item -ItemType Directory -Path $Tmp | Out-Null
  try {
    foreach ($name in @($Zip, 'SHA256SUMS')) {
      $dest = Join-Path $Tmp $name
      if ($Base -match '^https?://') {
        try { Invoke-WebRequest -Uri "$Base/$name" -OutFile $dest -UseBasicParsing }
        catch { throw "qloudpulse install: could not download $Base/$name ($($_.Exception.Message))" }
      } else {
        $src = Join-Path $Base $name
        if (-not (Test-Path -LiteralPath $src)) { throw "qloudpulse install: no $name in $Base" }
        Copy-Item -LiteralPath $src -Destination $dest
      }
    }

    # The line for this zip, compared in full.
    $want = $null
    foreach ($line in Get-Content -LiteralPath (Join-Path $Tmp 'SHA256SUMS')) {
      $parts = $line.Trim() -split '\s+', 2
      if ($parts.Count -eq 2 -and ($parts[1] -eq $Zip -or $parts[1] -eq "*$Zip")) { $want = $parts[0] }
    }
    if (-not $want) { throw "qloudpulse install: SHA256SUMS names no $Zip" }
    $got = (Get-FileHash -LiteralPath (Join-Path $Tmp $Zip) -Algorithm SHA256).Hash
    if ($got -ne $want) { throw "qloudpulse install: $Zip does not match SHA256SUMS (got $got, want $want); nothing was installed" }

    if ($Attest) {
      if (-not (Get-Command gh -ErrorAction SilentlyContinue)) { throw 'qloudpulse install: -Attest needs gh (https://cli.github.com)' }
      & gh attestation verify (Join-Path $Tmp $Zip) --repo $Repo --signer-workflow "$Repo/.github/workflows/$Workflow" --source-ref "refs/tags/$Tag" | Out-Host
      if ($LASTEXITCODE -ne 0) { throw "qloudpulse install: $Zip has no provenance from $Repo's release workflow at $Tag; nothing was installed" }
    }

    $out = Join-Path $Tmp 'x'
    Expand-Archive -LiteralPath (Join-Path $Tmp $Zip) -DestinationPath $out
    $exe = Join-Path $out "$Bin.exe"
    if (-not (Test-Path -LiteralPath $exe)) { throw "qloudpulse install: $Zip has no $Bin.exe in it" }
    $said = & $exe --version
    if ($LASTEXITCODE -ne 0) { throw "qloudpulse install: the downloaded $Bin.exe does not run here" }

    New-Item -ItemType Directory -Path $Dir -Force | Out-Null
    # Copied next to the old one and moved over it, so the client is never half written.
    $installed = Join-Path $Dir "$Bin.exe"
    $new = "$installed.new"
    Copy-Item -LiteralPath $exe -Destination $new -Force
    Move-Item -LiteralPath $new -Destination $installed -Force
    Write-Host "qloudpulse install: installed $said as $installed"
  } finally {
    Remove-Item -LiteralPath $Tmp -Recurse -Force -ErrorAction SilentlyContinue
  }
  # One that was somewhere else, or under the old name, is still there.
  if ($Old -and $Old -ne $installed) {
    Write-Host "qloudpulse install: $Old ($OldVersion) is still there; remove it if you no longer need it: Remove-Item '$Old'"
  }

  if ($env:GITHUB_PATH) {
    # On a GitHub Actions runner, the later steps of the job find it.
    Add-Content -LiteralPath $env:GITHUB_PATH -Value $Dir
  } elseif (-not $NoModifyPath) {
    # Through the registry, so the user's PATH keeps its type (REG_EXPAND_SZ) and its %VARIABLES% unexpanded, which
    # [Environment]::SetEnvironmentVariable would lose.
    $key = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment', $true)
    try {
      $path = [string]$key.GetValue('Path', '', [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
      $entries = @($path -split ';' | Where-Object { $_ })
      if ($entries -notcontains $Dir) {
        $kind = if ($path) { $key.GetValueKind('Path') } else { [Microsoft.Win32.RegistryValueKind]::ExpandString }
        $key.SetValue('Path', (($entries + $Dir) -join ';'), $kind)
        # Setting and clearing a variable this way tells Windows the environment changed, so new terminals see it.
        [Environment]::SetEnvironmentVariable('QLOUDPULSE_INSTALL_REFRESH', '1', 'User')
        [Environment]::SetEnvironmentVariable('QLOUDPULSE_INSTALL_REFRESH', $null, 'User')
        Write-Host "qloudpulse install: added $Dir to your PATH; open a new terminal to use $Bin"
      }
    } finally {
      $key.Close()
    }
    if (($env:Path -split ';') -notcontains $Dir) { $env:Path = "$env:Path;$Dir" }
  }
}

Install-Qloudpulse -Version $Version -Dir $Dir -Target $Target -Base $Base -Attest:$Attest.IsPresent `
  -NoModifyPath:($NoModifyPath.IsPresent -or [bool]$env:QLOUDPULSE_NO_MODIFY_PATH) `
  -Yes:($Yes.IsPresent -or ($env:QLOUDPULSE_ASSUME_YES -and $env:QLOUDPULSE_ASSUME_YES -ne '0'))
