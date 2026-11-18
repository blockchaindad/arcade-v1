# Arcade V1.0 installer for Windows 10/11 x64.
#
# One line (PowerShell). It downloads the newest release, checks the sha256, and installs
# it. Nothing to compile unless there is no download yet, or you ask with --build:
#
#   powershell -NoProfile -ExecutionPolicy Bypass -Command "[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12; iex ((New-Object Net.WebClient).DownloadString('https://raw.githubusercontent.com/martinseeger2002/arcade-v1/main/scripts/install.ps1'))"
#
# From a clone of this repo it does the same. --build skips the download and compiles here.
#
#   powershell -ExecutionPolicy Bypass -File scripts\install.ps1
#   powershell -ExecutionPolicy Bypass -File scripts\install.ps1 --build --no-desktop
#
# What it installs, for the current user (no administrator account):
#   arcaded, arcade, and Arcade V1.0 (the window; WebView2) into %LOCALAPPDATA%\Programs\Arcade
#   data into %LOCALAPPDATA%\arcade   (or --home)
#   a Task Scheduler entry "Arcade V1.0" that starts arcaded at login
#   a Start menu shortcut
#
# Options: --no-desktop  --build  --prebuilt DIR  --home DIR  --log PATH
#          --http-port N  --mesh-port N  --stun-port N
# Environment: ARCADE_BUILD=1, ARCADE_HOME, ARCADE_GITHUB, ARCADE_BRANCH, ARCADE_NO_OPEN=1

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
try { [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12 } catch {}

$OwnerRepo = if ($env:ARCADE_GITHUB) { $env:ARCADE_GITHUB } else { 'martinseeger2002/arcade-v1' }
$Branch = if ($env:ARCADE_BRANCH) { $env:ARCADE_BRANCH } else { 'main' }
$TaskName = 'Arcade V1.0'
$NoOpen = ($env:ARCADE_NO_OPEN -eq '1')

$NoDesktop = $false
$Build = ($env:ARCADE_BUILD -eq '1')
$Prebuilt = $null
$DataDir = $null
$HttpPort = $null
$MeshPort = $null
$StunPort = $null
$LogPath = $null

$argv = @($args)
for ($i = 0; $i -lt $argv.Count; $i++) {
    $a = $argv[$i]
    switch ($a) {
        '--no-desktop' { $NoDesktop = $true }
        '--build' { $Build = $true }
        '--prebuilt' {
            if ($i + 1 -ge $argv.Count) { throw 'missing value for --prebuilt' }
            $i++; $Prebuilt = $argv[$i]
        }
        '--home' {
            if ($i + 1 -ge $argv.Count) { throw 'missing value for --home' }
            $i++; $DataDir = $argv[$i]
        }
        '--http-port' {
            if ($i + 1 -ge $argv.Count) { throw 'missing value for --http-port' }
            $i++; $HttpPort = [int]$argv[$i]
        }
        '--mesh-port' {
            if ($i + 1 -ge $argv.Count) { throw 'missing value for --mesh-port' }
            $i++; $MeshPort = [int]$argv[$i]
        }
        '--stun-port' {
            if ($i + 1 -ge $argv.Count) { throw 'missing value for --stun-port' }
            $i++; $StunPort = [int]$argv[$i]
        }
        '--log' {
            if ($i + 1 -ge $argv.Count) { throw 'missing value for --log' }
            $i++; $LogPath = $argv[$i]
        }
        '--help' {
            Get-Content -LiteralPath $PSCommandPath -TotalCount 28
            exit 0
        }
        '-h' {
            Get-Content -LiteralPath $PSCommandPath -TotalCount 28
            exit 0
        }
        default { throw "unknown option: $a" }
    }
}

function Say([string]$Msg) { Write-Host $Msg -ForegroundColor Cyan }

function Step([string]$Msg) {
    $line = '{0:yyyy-MM-dd HH:mm:ss} {1}' -f (Get-Date), $Msg
    Write-Host $line
    if ($script:LogPath -and -not $script:TranscriptOn) {
        Add-Content -LiteralPath $script:LogPath -Value $line -Encoding Unicode -ErrorAction SilentlyContinue
    }
}

function Stop-InstallLog {
    if ($script:TranscriptOn) {
        try { Stop-Transcript | Out-Null } catch {}
        $script:TranscriptOn = $false
    }
}

# A failed install must not leave the previous node off. The task is the one already
# registered; enabling it is a no-op when this run never got as far as disabling it.
function Resume-OldNode {
    Step "Starting the previous '$TaskName' task so this failure does not leave the node off."
    $already = @(Get-Process -Name 'arcaded' -ErrorAction SilentlyContinue)
    try {
        Enable-ScheduledTask -TaskName $TaskName -ErrorAction Stop | Out-Null
        Start-ScheduledTask -TaskName $TaskName -ErrorAction Stop
        Step "Login task '$TaskName' is enabled and was started."
        return
    } catch {
        Step "Task Scheduler did not start '$TaskName' ($($_.Exception.Message))."
    }
    if ($already.Count -gt 0) {
        Step 'arcaded.exe is already running, so it was not started a second time.'
        return
    }
    $vbs = Join-Path (Join-Path (LocalAppData) 'Programs\Arcade') 'arcaded-hidden.vbs'
    $wscript = Join-Path $env:SystemRoot 'System32\wscript.exe'
    if ((Test-Path -LiteralPath $vbs) -and (Test-Path -LiteralPath $wscript)) {
        Step "Starting $vbs directly."
        Start-Process -FilePath $wscript -ArgumentList "//B `"$vbs`"" -WindowStyle Hidden
    } else {
        Step "No previous login task or arcaded-hidden.vbs was found to start."
    }
}

function Copy-WithRetry([string]$Src, [string]$Dst) {
    for ($n = 1; $n -le 10; $n++) {
        try {
            Copy-Item -LiteralPath $Src -Destination $Dst -Force -ErrorAction Stop
            return
        } catch {
            if ($n -ge 10) { throw }
            Step "Copy of $(Split-Path -Leaf $Src) is in use; retry $n of 10."
            Start-Sleep -Seconds 2
        }
    }
}

if (-not $LogPath -and $Prebuilt) {
    $temp = if ($env:TEMP) { $env:TEMP } else { [IO.Path]::GetTempPath() }
    $LogPath = Join-Path $temp 'arcade-v1-install.log'
}
$script:LogPath = $LogPath
$script:TranscriptOn = $false
if ($LogPath) {
    try {
        Start-Transcript -LiteralPath $LogPath -Append -Force -ErrorAction Stop | Out-Null
        $script:TranscriptOn = $true
    } catch {
        $script:TranscriptOn = $false
        Add-Content -LiteralPath $LogPath -Value ("{0:yyyy-MM-dd HH:mm:ss} Start-Transcript failed: {1}" -f (Get-Date), $_.Exception.Message) -Encoding Unicode -ErrorAction SilentlyContinue
    }
    Step 'install.ps1 started.'
}

function LocalAppData {
    if ($env:LOCALAPPDATA) { return $env:LOCALAPPDATA }
    return (Join-Path $env:USERPROFILE 'AppData\Local')
}

function Test-TcpBusy([int]$Port) {
    $client = New-Object System.Net.Sockets.TcpClient
    try {
        $wait = $client.BeginConnect('127.0.0.1', $Port, $null, $null)
        $ok = $wait.AsyncWaitHandle.WaitOne(200, $false)
        if (-not $ok) { return $false }
        $client.EndConnect($wait)
        return $true
    } catch {
        return $false
    } finally {
        $client.Close()
    }
}

function Test-UdpBusy([int]$Port) {
    $udp = New-Object System.Net.Sockets.Socket -ArgumentList ([System.Net.Sockets.AddressFamily]::InterNetwork), ([System.Net.Sockets.SocketType]::Dgram), ([System.Net.Sockets.ProtocolType]::Udp)
    try {
        $udp.Bind((New-Object System.Net.IPEndPoint ([System.Net.IPAddress]::Any, $Port)))
        return $false
    } catch {
        return $true
    } finally {
        $udp.Close()
    }
}

function Get-FreeTcp([int]$Port) {
    $n = 0
    while ((Test-TcpBusy $Port) -and $n -lt 20) { $Port += 10; $n++ }
    return $Port
}

function Assert-Sha256([string]$File, [string]$SumFile) {
    $line = (Get-Content -Raw -LiteralPath $SumFile).Trim()
    $expected = (($line -split '\s+')[0]).ToLower()
    $actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $File).Hash.ToLower()
    if ($actual -ne $expected) { throw "The download is damaged (checksum). Expected $expected, got $actual." }
}

function Add-UserPath([string]$Dir) {
    $cur = [Environment]::GetEnvironmentVariable('Path', 'User')
    if (-not $cur) { $cur = '' }
    $norm = $Dir.TrimEnd('\')
    $parts = @()
    $seen = $false
    foreach ($p in ($cur -split ';')) {
        if (-not $p) { continue }
        if ($p.TrimEnd('\') -eq $norm) { $seen = $true }
        $parts += $p
    }
    if (-not $seen) {
        [Environment]::SetEnvironmentVariable('Path', ((@($Dir) + $parts) -join ';'), 'User')
    }
    if ($env:Path -notlike "*$norm*") { $env:Path = "$Dir;$env:Path" }
}

function Test-WebView2 {
    $names = @(
        'HKLM:\SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}',
        'HKLM:\SOFTWARE\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}',
        'HKCU:\SOFTWARE\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}'
    )
    foreach ($k in $names) {
        if (Test-Path $k) {
            $v = (Get-ItemProperty -LiteralPath $k -ErrorAction SilentlyContinue).pv
            if ($v -and $v -ne '0.0.0.0') { return $true }
        }
    }
    return $false
}

function Stop-Arcade([string]$Dir) {
    # The open 0.1.101 window starts arcaded again through arcaded-hidden.vbs even after the
    # login task is disabled. Close that window first. Nothing in the data folder is deleted.
    Step 'Closing arcade-desktop.exe before the service, so it cannot start arcaded again.'
    $desk = @(Get-Process -Name 'arcade-desktop' -ErrorAction SilentlyContinue)
    if ($desk.Count -gt 0) {
        foreach ($proc in $desk) { [void]$proc.CloseMainWindow() }
        $deadline = (Get-Date).AddSeconds(10)
        while ((Get-Process -Name 'arcade-desktop' -ErrorAction SilentlyContinue) -and (Get-Date) -lt $deadline) {
            Start-Sleep -Seconds 1
        }
        $left = @(Get-Process -Name 'arcade-desktop' -ErrorAction SilentlyContinue)
        if ($left.Count -gt 0) {
            Step 'arcade-desktop.exe was still open after 10 seconds; stopping it.'
            $left | Stop-Process -Force -ErrorAction SilentlyContinue
        } else {
            Step 'arcade-desktop.exe closed.'
        }
    } else {
        Step 'arcade-desktop.exe was not running.'
    }
    Disable-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue | Out-Null
    $task = Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
    if (-not $task) {
        Step "No login task named '$TaskName' to disable."
    } elseif ($task.State -ne 'Disabled') {
        Step "The login task '$TaskName' is still $($task.State) after Disable-ScheduledTask."
    } else {
        Step "Login task '$TaskName' is Disabled."
    }
    $running = @(Get-Process -Name 'arcaded' -ErrorAction SilentlyContinue)
    if ($running.Count -gt 0 -and $Dir) {
        Step 'Stopping Arcade so it can finish writing (up to 5 minutes)...'
        if (-not (Test-Path -LiteralPath $Dir)) { New-Item -ItemType Directory -Force -Path $Dir | Out-Null }
        $stop = Join-Path $Dir 'arcaded.stop'
        Set-Content -LiteralPath $stop -Value 'stop' -Encoding ascii
        $deadline = (Get-Date).AddSeconds(300)
        while ((Get-Process -Name 'arcaded' -ErrorAction SilentlyContinue) -and (Get-Date) -lt $deadline) {
            Start-Sleep -Seconds 1
        }
        Remove-Item -LiteralPath $stop -Force -ErrorAction SilentlyContinue
    }
    Stop-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue
    $still = @(Get-Process -Name 'arcaded' -ErrorAction SilentlyContinue)
    if ($still.Count -gt 0) {
        Step 'arcaded.exe was still running after the clean stop; it was force-killed.'
        $still | Stop-Process -Force -ErrorAction SilentlyContinue
    } else {
        Step 'arcaded.exe had already exited; it was not force-killed.'
    }
    Start-Sleep -Milliseconds 400
}

function Ensure-Toolchain {
    $cargoHome = Join-Path $env:USERPROFILE '.cargo\bin'
    if (Test-Path (Join-Path $cargoHome 'cargo.exe')) {
        if ($env:Path -notlike "*$cargoHome*") { $env:Path = "$cargoHome;$env:Path" }
    }
    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
        Say "Installing Rust for this user (rustup)..."
        $init = Join-Path $env:TEMP 'rustup-init.exe'
        Invoke-WebRequest -Uri 'https://win.rustup.rs/x86_64' -OutFile $init -UseBasicParsing
        & $init -y --profile minimal --default-toolchain stable --default-host x86_64-pc-windows-msvc
        if ($LASTEXITCODE -ne 0) { throw "rustup-init failed ($LASTEXITCODE)" }
        $env:Path = "$cargoHome;$env:Path"
    }
    $hostLine = (& rustc -vV | Select-String '^host:').ToString()
    $script:CargoTarget = @()
    if ($hostLine -notmatch 'windows-msvc') {
        Say "Adding the x86_64-pc-windows-msvc Rust target..."
        & rustup target add x86_64-pc-windows-msvc
        if ($LASTEXITCODE -ne 0) { throw "rustup target add failed" }
        $script:CargoTarget = @('--target', 'x86_64-pc-windows-msvc')
    }
    $llvm = Join-Path $env:ProgramFiles 'LLVM\bin\libclang.dll'
    $haveClang = (Test-Path $llvm) -or (Get-Command clang -ErrorAction SilentlyContinue)
    if (-not $haveClang) {
        $winget = Get-Command winget -ErrorAction SilentlyContinue
        if ($winget) {
            Say "Installing LLVM (libclang), needed to compile the database..."
            & winget install --id LLVM.LLVM -e --accept-source-agreements --accept-package-agreements
        }
    }
    $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
    $vs = $null
    if (Test-Path $vswhere) {
        $vs = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
    }
    if (-not $vs) {
        throw "Visual Studio C++ build tools are not installed. Install `"Desktop development with C++`" from https://visualstudio.microsoft.com/visual-cpp-build-tools/ and run this again."
    }
}

function Sync-Source([string]$Dest) {
    $git = Get-Command git -ErrorAction SilentlyContinue
    if ($git) {
        if (Test-Path (Join-Path $Dest '.git')) {
            & git -C $Dest fetch origin $Branch
            if ($LASTEXITCODE -ne 0) { throw 'git fetch failed' }
            & git -C $Dest checkout $Branch
            if ($LASTEXITCODE -ne 0) { throw 'git checkout failed' }
            & git -C $Dest reset --hard "origin/$Branch"
            if ($LASTEXITCODE -ne 0) { throw 'git reset failed' }
        } else {
            if (Test-Path $Dest) { Remove-Item -LiteralPath $Dest -Recurse -Force }
            & git clone --branch $Branch "https://github.com/$OwnerRepo.git" $Dest
            if ($LASTEXITCODE -ne 0) { throw 'git clone failed' }
        }
        return
    }
    $zipUrl = "https://codeload.github.com/$OwnerRepo/zip/refs/heads/$Branch"
    $tmp = Join-Path $env:TEMP ("arcade-src-" + [guid]::NewGuid().ToString('n'))
    New-Item -ItemType Directory -Path $tmp | Out-Null
    try {
        $zip = Join-Path $tmp 'src.zip'
        Invoke-WebRequest -Uri $zipUrl -OutFile $zip -UseBasicParsing
        Expand-Archive -LiteralPath $zip -DestinationPath $tmp -Force
        $unpacked = Get-ChildItem -LiteralPath $tmp -Directory | Where-Object { $_.Name -like "*-$Branch" } | Select-Object -First 1
        if (-not $unpacked) { throw 'The source download had no folder.' }
        $kept = Join-Path $Dest 'target'
        if (Test-Path $kept) {
            $destTarget = Join-Path $unpacked.FullName 'target'
            if (Test-Path $destTarget) { Remove-Item -LiteralPath $destTarget -Recurse -Force }
            Move-Item -LiteralPath $kept -Destination $destTarget
        }
        if (Test-Path $Dest) { Remove-Item -LiteralPath $Dest -Recurse -Force }
        Move-Item -LiteralPath $unpacked.FullName -Destination $Dest
    } finally {
        if (Test-Path $tmp) { Remove-Item -LiteralPath $tmp -Recurse -Force -ErrorAction SilentlyContinue }
    }
}

function Build-Source([string]$Root) {
    Ensure-Toolchain
    $envScript = Join-Path $Root 'scripts\msvc-env.ps1'
    if (-not (Test-Path $envScript)) { throw "Missing $envScript" }
    . $envScript
    $pkgs = @('build', '--release', '--locked') + $script:CargoTarget + @('-p', 'arcaded', '-p', 'arcade-cli')
    $desktop = -not $NoDesktop
    Push-Location $Root
    try {
        if ($desktop) {
            & cargo @($pkgs + @('-p', 'arcade-desktop'))
            if ($LASTEXITCODE -ne 0) {
                Say "The desktop app did not build. Installing the service and the command line. The window needs the MSVC toolchain and the WebView2 crate to compile; run with --no-desktop to skip this attempt."
                $desktop = $false
                & cargo @pkgs
            }
        } else {
            & cargo @pkgs
        }
        if ($LASTEXITCODE -ne 0) { throw "cargo build failed ($LASTEXITCODE)" }
    } finally {
        Pop-Location
    }
    $script:NoDesktop = -not $desktop
    $out = if ($script:CargoTarget.Count -gt 0) { Join-Path $Root 'target\x86_64-pc-windows-msvc\release' } else { Join-Path $Root 'target\release' }
    # MPIR's DLLs (chiavdf, the ARCADE chain's proof of time, links them): beside the programs.
    $cargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $env:USERPROFILE '.cargo' }
    $mpir = Get-ChildItem (Join-Path $cargoHome 'registry\src') -Recurse -Directory -Filter mpir_gc_x64 -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($mpir) { Copy-Item (Join-Path $mpir.FullName 'mpir*.dll') $out -Force }
    return $out
}

function Try-Download {
    $name = 'arcade-v1-windows-x86_64'
    # /releases/latest skips pre-releases. Walk the release list (newest first, drafts
    # excluded, pre-releases included) and take the first one that has the Windows zip.
    $url = $null
    $sumUrl = $null
    $tag = $null
    try {
        $headers = @{ 'User-Agent' = 'arcade-v1-install'; 'Accept' = 'application/vnd.github+json' }
        $rels = @(Invoke-RestMethod -Uri "https://api.github.com/repos/$OwnerRepo/releases?per_page=30" -Headers $headers)
        foreach ($rel in $rels) {
            if (-not $rel -or $rel.draft) { continue }
            $z = $null
            $s = $null
            foreach ($asset in @($rel.assets)) {
                if (-not $asset) { continue }
                if ($asset.name -eq "$name.zip") { $z = [string]$asset.browser_download_url }
                if ($asset.name -eq "$name.zip.sha256") { $s = [string]$asset.browser_download_url }
            }
            if ($z -and $s) {
                $url = $z
                $sumUrl = $s
                $tag = [string]$rel.tag_name
                break
            }
        }
    } catch {
        Say "Could not list releases ($($_.Exception.Message))."
    }
    if (-not $url) {
        $url = "https://github.com/$OwnerRepo/releases/latest/download/$name.zip"
        $sumUrl = "$url.sha256"
    }
    $cache = Join-Path (LocalAppData) 'arcade-v1-cache'
    if (Test-Path $cache) { Remove-Item -LiteralPath $cache -Recurse -Force }
    New-Item -ItemType Directory -Path $cache | Out-Null
    $zip = Join-Path $cache "$name.zip"
    if ($tag) { Say "Downloading Arcade V1.0 for Windows x64 ($tag)..." } else { Say "Downloading Arcade V1.0 for Windows x64..." }
    try {
        Invoke-WebRequest -Uri $url -OutFile $zip -UseBasicParsing
        Invoke-WebRequest -Uri $sumUrl -OutFile "$zip.sha256" -UseBasicParsing
    } catch {
        Say "No prebuilt download for this PC yet."
        return $false
    }
    # A download that fails its checksum installs nothing: it stops here, never falling back to a build from source
    # (a damaged or changed download is a reason to stop, not to try something else).
    try { Assert-Sha256 $zip "$zip.sha256" } catch {
        Remove-Item -LiteralPath $zip, "$zip.sha256" -Force -ErrorAction SilentlyContinue
        throw ("Nothing was installed: " + $_.Exception.Message + " Try again later, or download the release by hand and check it.")
    }
    Expand-Archive -LiteralPath $zip -DestinationPath $cache -Force
    $dir = Join-Path $cache $name
    if (-not (Test-Path (Join-Path $dir 'bin\arcaded.exe'))) {
        Say "The download did not contain arcaded.exe."
        return $false
    }
    $commit = Join-Path $dir 'COMMIT'
    if (Test-Path $commit) {
        $c = (Get-Content -LiteralPath $commit -TotalCount 1)
        if ($c.Length -ge 12) { Write-Host ("  version " + $c.Substring(0, 12)) }
    }
    $inner = Join-Path $dir 'scripts\install.ps1'
    $forward = @('--prebuilt', $dir)
    if ($NoDesktop) { $forward += '--no-desktop' }
    if ($DataDir) { $forward += @('--home', $DataDir) }
    if ($null -ne $HttpPort) { $forward += @('--http-port', "$HttpPort") }
    if ($null -ne $MeshPort) { $forward += @('--mesh-port', "$MeshPort") }
    if ($null -ne $StunPort) { $forward += @('--stun-port', "$StunPort") }
    & powershell.exe -NoProfile -ExecutionPolicy Bypass -File $inner @forward
    exit $LASTEXITCODE
}

function Install-Programs([string]$FromDir, [string]$IconPng, [string]$IconIco) {
  try {
    $local = LocalAppData
    if (-not $DataDir) {
        if ($env:ARCADE_HOME) { $script:DataDir = $env:ARCADE_HOME } else { $script:DataDir = (Join-Path $local 'arcade') }
    }
    if ($DataDir -match '"') { throw 'The data folder path cannot contain quotes.' }
    $bin = Join-Path $local 'Programs\Arcade'
    $srcExe = Join-Path $FromDir 'arcaded.exe'
    if (-not (Test-Path -LiteralPath $srcExe)) {
        throw "arcaded.exe is not in the installer payload ($srcExe). Windows Defender may have quarantined it."
    }
    Step "Chain data in $DataDir is not moved or deleted."
    $first = -not (Test-Path (Join-Path $bin 'arcade-desktop.exe'))
    Stop-Arcade $DataDir
    New-Item -ItemType Directory -Force -Path $bin | Out-Null
    if (-not (Test-Path -LiteralPath $DataDir)) { New-Item -ItemType Directory -Force -Path $DataDir | Out-Null }
    Copy-WithRetry $srcExe (Join-Path $bin 'arcaded.exe')
    Copy-WithRetry (Join-Path $FromDir 'arcade.exe') (Join-Path $bin 'arcade.exe')
    # MPIR's DLLs (chiavdf links them) and its license go beside the programs.
    Get-ChildItem -LiteralPath $FromDir -Filter 'mpir*.dll' -ErrorAction SilentlyContinue | ForEach-Object {
        Copy-WithRetry $_.FullName (Join-Path $bin $_.Name)
    }
    $mpirLicense = Join-Path $FromDir 'MPIR-LICENSE.txt'
    if (Test-Path -LiteralPath $mpirLicense) { Copy-WithRetry $mpirLicense (Join-Path $bin 'MPIR-LICENSE.txt') }
    $haveDesktop = $false
    $deskSrc = Join-Path $FromDir 'arcade-desktop.exe'
    if (-not $NoDesktop -and (Test-Path -LiteralPath $deskSrc)) {
        Copy-WithRetry $deskSrc (Join-Path $bin 'arcade-desktop.exe')
        $haveDesktop = $true
        if ($IconIco -and (Test-Path -LiteralPath $IconIco)) {
            Copy-WithRetry $IconIco (Join-Path $bin 'arcade-v1.ico')
        }
        if ($IconPng -and (Test-Path -LiteralPath $IconPng)) {
            Copy-WithRetry $IconPng (Join-Path $bin 'arcade-v1.png')
        }
    } elseif (-not $NoDesktop) {
        Say "The desktop app is not in this build; the service and the command line still install."
    }
    Set-Content -LiteralPath (Join-Path $bin 'home.txt') -Value $DataDir -Encoding ascii
    Add-UserPath $bin

    if ($null -eq $HttpPort) { $script:HttpPort = Get-FreeTcp 8420 }
    if ($null -eq $MeshPort) { $script:MeshPort = Get-FreeTcp 8421 }
    if ($MeshPort -eq $HttpPort) { $script:MeshPort = Get-FreeTcp ($HttpPort + 1) }
    if ($null -eq $StunPort) {
        if (Test-UdpBusy 3478) { $script:StunPort = 0 } else { $script:StunPort = 3478 }
    }

    $exe = Join-Path $bin 'arcaded.exe'
    $run = "`"$exe`" --home `"$DataDir`" --http-port $HttpPort --mesh-port $MeshPort --stun-port $StunPort"
    $runVbs = $run.Replace('"', '""')
    $vbs = Join-Path $bin 'arcaded-hidden.vbs'
    # Login (and a Startup-folder fallback) clear the owner's stop marker, then start. The
    # window's refresh does not: while arcaded.stopped exists it will not run this task.
    @"
Set fso = CreateObject("Scripting.FileSystemObject")
If fso.FileExists("$DataDir\arcaded.stopped") Then fso.DeleteFile "$DataDir\arcaded.stopped", True
Set sh = CreateObject("WScript.Shell")
sh.Environment("Process")("ARCADE_HOME") = "$DataDir"
sh.CurrentDirectory = "$DataDir"
code = sh.Run("$runVbs", 0, True)
WScript.Quit code
"@ | Set-Content -LiteralPath $vbs -Encoding ASCII

    $wscript = Join-Path $env:SystemRoot 'System32\wscript.exe'
    $method = 'task'
    try {
        $action = New-ScheduledTaskAction -Execute $wscript -Argument "//B `"$vbs`"" -WorkingDirectory $DataDir
        $user = "$env:USERDOMAIN\$env:USERNAME"
        try {
            $trigger = New-ScheduledTaskTrigger -AtLogOn -User $user
        } catch {
            $trigger = New-ScheduledTaskTrigger -AtLogOn
        }
        $settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -StartWhenAvailable -MultipleInstances IgnoreNew -RestartCount 3 -RestartInterval (New-TimeSpan -Minutes 1) -ExecutionTimeLimit ([TimeSpan]::Zero)
        Register-ScheduledTask -TaskName $TaskName -Action $action -Trigger $trigger -Settings $settings -Force | Out-Null
        Start-ScheduledTask -TaskName $TaskName
    } catch {
        Say "Task Scheduler refused the login task ($($_.Exception.Message)). Using the Startup folder instead."
        $method = 'startup'
        $startup = [Environment]::GetFolderPath('Startup')
        $shell = New-Object -ComObject WScript.Shell
        $sc = $shell.CreateShortcut((Join-Path $startup 'Arcade V1.0.lnk'))
        $sc.TargetPath = $wscript
        $sc.Arguments = "//B `"$vbs`""
        $sc.WorkingDirectory = $DataDir
        $sc.WindowStyle = 7
        $sc.Description = 'Arcade V1.0'
        $ico = Join-Path $bin 'arcade-v1.ico'
        if (Test-Path $ico) { $sc.IconLocation = "$ico,0" }
        $sc.Save()
        Start-Process -FilePath $wscript -ArgumentList "//B `"$vbs`""
    }
    Set-Content -LiteralPath (Join-Path $bin 'start-method.txt') -Value $method -Encoding ascii

    if ($haveDesktop) {
        $desk = Join-Path $bin 'arcade-desktop.exe'
        $deskVbs = Join-Path $bin 'arcade-desktop.vbs'
        $deskRun = "`"$desk`"".Replace('"', '""')
        @"
Set sh = CreateObject("WScript.Shell")
sh.Environment("Process")("ARCADE_HOME") = "$DataDir"
sh.CurrentDirectory = "$DataDir"
sh.Run "$deskRun", 1, False
"@ | Set-Content -LiteralPath $deskVbs -Encoding ASCII
        $menu = Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs'
        New-Item -ItemType Directory -Force -Path $menu | Out-Null
        $shell = New-Object -ComObject WScript.Shell
        $lnk = $shell.CreateShortcut((Join-Path $menu 'Arcade V1.0.lnk'))
        $lnk.TargetPath = $wscript
        $lnk.Arguments = "//B `"$deskVbs`""
        $lnk.WorkingDirectory = $DataDir
        $lnk.WindowStyle = 7
        $lnk.Description = 'Arcade V1.0'
        $ico = Join-Path $bin 'arcade-v1.ico'
        if (Test-Path $ico) { $lnk.IconLocation = "$ico,0" }
        $lnk.Save()
        if (-not (Test-WebView2)) {
            Say "The app window needs the Microsoft Edge WebView2 runtime. Installing it..."
            $boot = Join-Path $env:TEMP 'MicrosoftEdgeWebview2Setup.exe'
            try {
                Invoke-WebRequest -Uri 'https://go.microsoft.com/fwlink/p/?LinkId=2124703' -OutFile $boot -UseBasicParsing
                Start-Process -FilePath $boot -ArgumentList '/silent','/install' -Wait
            } catch {
                Say "WebView2 could not be installed automatically. Download it from https://developer.microsoft.com/microsoft-edge/webview2/ and then open Arcade V1.0."
            }
        }
        if ($first -and -not $NoOpen) {
            Say "Opening Arcade V1.0..."
            Start-Process -FilePath $wscript -ArgumentList "//B `"$deskVbs`""
        }
    }

    $stunText = if ($StunPort -eq 0) { 'off' } else { "$StunPort" }
    Say "Arcade V1.0 is installed and running."
    Write-Host "  programs: $bin"
    Write-Host "  data:     $DataDir"
    Write-Host "  ports:    http $HttpPort, mesh $MeshPort, STUN $stunText"
    if ($method -eq 'task') {
        Write-Host "  service:  Task Scheduler, `"Arcade V1.0`" (starts at login)"
    } else {
        Write-Host "  service:  Startup folder shortcut (starts at login)"
    }
    if ($haveDesktop) { Write-Host "  app:      Arcade V1.0 in the Start menu" }
    Write-Host "  setup:    the app's Home tab walks you through it (or: arcade setup)"
    Write-Host "  logs:     run arcaded.exe in a terminal to see them. A hidden login start does not keep a console."
    Write-Host ""
    Write-Host "Open a new terminal if `"arcade`" is not found yet (PATH was updated for this user)."
    Step 'Install finished.'
    Stop-InstallLog
  } catch {
    Step "Install failed: $($_.Exception.Message)"
    Resume-OldNode
    Stop-InstallLog
    exit 1
  }
}

$ScriptDir = $PSScriptRoot
if (-not $ScriptDir -and $PSCommandPath) { $ScriptDir = Split-Path -Parent $PSCommandPath }

if ($Prebuilt) {
    $root = (Resolve-Path -LiteralPath $Prebuilt).Path
    Install-Programs (Join-Path $root 'bin') (Join-Path $root 'icon.png') (Join-Path $root 'icon.ico')
    exit 0
}

$beside = $null
if ($ScriptDir) { $beside = Join-Path $ScriptDir '..\bin\arcaded.exe' }
if ($beside -and (Test-Path -LiteralPath $beside) -and -not $Build) {
    $root = (Resolve-Path (Join-Path $ScriptDir '..')).Path
    Install-Programs (Join-Path $root 'bin') (Join-Path $root 'icon.png') (Join-Path $root 'icon.ico')
    exit 0
}

if (-not $Build) {
    try {
        if (Try-Download) { exit 0 }
    } catch {
        Say "The prebuilt download could not be installed ($($_.Exception.Message))."
    }
    Say "Falling back to building from source."
}

$repoRoot = $null
if ($ScriptDir -and (Test-Path (Join-Path $ScriptDir '..\Cargo.toml'))) {
    $repoRoot = (Resolve-Path (Join-Path $ScriptDir '..')).Path
} else {
    Say "Building Arcade V1.0 from source (the first build takes a while)..."
    $repoRoot = Join-Path (LocalAppData) 'arcade-v1-src'
    Sync-Source $repoRoot
}
$built = Build-Source $repoRoot
$iconPng = Join-Path $repoRoot 'crates\arcade-desktop\icons\icon.png'
$iconIco = Join-Path $repoRoot 'crates\arcade-desktop\icons\icon.ico'
Install-Programs $built $iconPng $iconIco
exit 0
