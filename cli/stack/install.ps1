<#
Installs the qloudpulse client on Windows (x64 or ARM64): picks the zip for this machine, checks it against the
release's SHA256SUMS, puts qloudpulse-stack.exe in %LOCALAPPDATA%\Programs\qloudpulse (or -Dir) and adds that to the user's
PATH.

  irm https://github.com/ulfmansson/qloudpulse-public/releases/download/qloudpulse-stack-<version>/install.ps1 | iex

The copy attached to a release installs that release; this copy in the repository installs the newest release, or
-Version (or $env:QLOUDPULSE_VERSION). Through irm | iex options cannot be passed, so each has an environment variable
too:

  -Version <version>   $env:QLOUDPULSE_VERSION      the client version (default: the release this script came with;
                                                    for the copy in the repository, the newest release)
  -Dir <dir>           $env:QLOUDPULSE_INSTALL_DIR  where qloudpulse-stack.exe goes
  -Target <target>     the build to install, x86_64-pc-windows-gnu or aarch64-pc-windows-gnullvm (default: this
                       machine's; an ARM64 machine gets the ARM64 build even from an x64 PowerShell)
  -Base <url or dir>   $env:QLOUDPULSE_DOWNLOAD_BASE  where the zips and SHA256SUMS are (default: the release)
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
  [switch]$NoModifyPath
)

# A release's copy has its version here (cli/stack/release.sh writes it); this one has none.
$QloudpulseDefaultVersion = ''

function Install-Qloudpulse {
  param([string]$Version, [string]$Dir, [string]$Target, [string]$Base, [bool]$Attest, [bool]$NoModifyPath)
  $ErrorActionPreference = 'Stop'
  $ProgressPreference = 'SilentlyContinue' # Windows PowerShell's progress bar slows a download many times over
  $Repo = 'ulfmansson/qloudpulse-public'

  if (-not $Version) { $Version = $QloudpulseDefaultVersion }
  if (-not $Version) {
    if ($Base) { throw 'qloudpulse install: no version: pass -Version or set $env:QLOUDPULSE_VERSION with -Base' }
    # The newest qloudpulse-stack-<version> release. Not GitHub's "latest": that is the cluster agent's release, in the
    # same repository.
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
    try {
      $releases = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases?per_page=100" -UseBasicParsing `
        -Headers @{ Accept = 'application/vnd.github+json' }
    } catch { $releases = @() }
    $Version = $releases | ForEach-Object { $_.tag_name } | Where-Object { $_ -match '^qloudpulse-stack-[0-9]+(\.[0-9]+)+$' } |
      ForEach-Object { $_ -replace '^qloudpulse-stack-', '' } | Sort-Object { [version]$_ } -Descending | Select-Object -First 1
    if (-not $Version) { throw "qloudpulse install: could not find the newest release on github.com/$Repo; pass -Version" }
  }
  $Version = $Version -replace '^(v|qloudpulse-stack-)', ''
  if ($Version -notmatch '^[0-9]+(\.[0-9]+)*$') { throw "qloudpulse install: not a version: $Version (for example 0.2.0)" }
  $Tag = "qloudpulse-stack-$Version"
  if (-not $Base) { $Base = "https://github.com/$Repo/releases/download/$Tag" }
  if (-not $Dir) { $Dir = Join-Path $env:LOCALAPPDATA 'Programs\qloudpulse' }

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
  $Zip = "qloudpulse-stack-$Version-$Target.zip"
  Write-Host "qloudpulse install: qloudpulse-stack $Version for $Target"

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
      & gh attestation verify (Join-Path $Tmp $Zip) --repo $Repo --signer-workflow "$Repo/.github/workflows/cli-stack-release.yml" --source-ref "refs/tags/$Tag" | Out-Host
      if ($LASTEXITCODE -ne 0) { throw "qloudpulse install: $Zip has no provenance from $Repo's release workflow at $Tag; nothing was installed" }
    }

    $out = Join-Path $Tmp 'x'
    Expand-Archive -LiteralPath (Join-Path $Tmp $Zip) -DestinationPath $out
    $exe = Join-Path $out 'qloudpulse-stack.exe'
    $said = & $exe --version
    if ($LASTEXITCODE -ne 0) { throw 'qloudpulse install: the downloaded qloudpulse-stack.exe does not run here' }

    New-Item -ItemType Directory -Path $Dir -Force | Out-Null
    # Copied next to the old one and moved over it, so qloudpulse-stack.exe is never half written.
    $new = Join-Path $Dir 'qloudpulse-stack.exe.new'
    Copy-Item -LiteralPath $exe -Destination $new -Force
    Move-Item -LiteralPath $new -Destination (Join-Path $Dir 'qloudpulse-stack.exe') -Force
    Write-Host "qloudpulse install: installed $said as $(Join-Path $Dir 'qloudpulse-stack.exe')"
  } finally {
    Remove-Item -LiteralPath $Tmp -Recurse -Force -ErrorAction SilentlyContinue
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
        Write-Host "qloudpulse install: added $Dir to your PATH; open a new terminal to use qloudpulse-stack"
      }
    } finally {
      $key.Close()
    }
    if (($env:Path -split ';') -notcontains $Dir) { $env:Path = "$env:Path;$Dir" }
  }
}

Install-Qloudpulse -Version $Version -Dir $Dir -Target $Target -Base $Base -Attest:$Attest.IsPresent `
  -NoModifyPath:($NoModifyPath.IsPresent -or [bool]$env:QLOUDPULSE_NO_MODIFY_PATH)
