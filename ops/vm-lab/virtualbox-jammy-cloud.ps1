param(
    [ValidateSet("prepare", "start", "stop", "status", "destroy", "install", "test")]
    [string]$Action = "status",
    [string]$VmName = "qubox-jammy-vbox",
    [string]$GuestDisplayName = "JammyVBox",
    [string]$CloudImageUrl = "https://cloud-images.ubuntu.com/jammy/current/jammy-server-cloudimg-amd64.ova",
    [string]$ServerUrl = "ws://13.239.73.205:7000/ws",
    [string]$GuestUser = "ubuntu",
    [string]$GuestPassword = "ubuntu",
    [string]$WslDistro = "Ubuntu-22.04",
    [int]$MemoryMB = 8192,
    [int]$CpuCount = 4,
    [int]$VramMB = 64,
    [int]$SshPort = 2222,
    [int]$QuicPort = 5000,
    [string]$HostOnlyInterfaceName = "VirtualBox Host-Only Ethernet Adapter",
    [string]$HostOnlyHostIp = "192.168.56.1",
    [int]$HostOnlyPrefixLength = 24,
    [string]$GuestHostOnlyIp = "192.168.56.20",
    [int]$DisplayWidth = 1600,
    [int]$DisplayHeight = 900,
    [ValidateSet("gui", "headless")]
    [string]$StartType = "gui"
)

$ErrorActionPreference = "Stop"

$RepoRoot = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$RuntimeRoot = Join-Path $RepoRoot ".local\vm-lab\virtualbox-jammy-cloud"
$DownloadsDir = Join-Path $RuntimeRoot "downloads"
$StateDir = Join-Path $RuntimeRoot "state"
$VmBaseFolder = Join-Path $RuntimeRoot "vbox"
$OvaPath = Join-Path $DownloadsDir "jammy-server-cloudimg-amd64.ova"
$UserDataPath = Join-Path $StateDir "user-data"
$MetaDataPath = Join-Path $StateDir "meta-data"
$SeedIsoPath = Join-Path $StateDir "seed.iso"
$GuestPrivateKeyPath = Join-Path $StateDir "vm-admin-key"
$GuestPublicKeyPath = "$GuestPrivateKeyPath.pub"
$SourceArchivePath = Join-Path $StateDir "qubox-source.tar"
$GuestSourceRevisionPath = "/home/$GuestUser/.qubox-source-rev"

foreach ($path in @($RuntimeRoot, $DownloadsDir, $StateDir, $VmBaseFolder)) {
    if (-not (Test-Path $path)) {
        New-Item -ItemType Directory -Path $path | Out-Null
    }
}

function Get-VBoxManagePath {
    if ($env:QUBOX_VBOXMANAGE -and (Test-Path $env:QUBOX_VBOXMANAGE)) {
        return $env:QUBOX_VBOXMANAGE
    }

    $defaultPath = "C:\Program Files\Oracle\VirtualBox\VBoxManage.exe"
    if (Test-Path $defaultPath) {
        return $defaultPath
    }

    throw "Unable to locate VBoxManage.exe. Set QUBOX_VBOXMANAGE or install VirtualBox."
}

function Invoke-VBoxManage {
    param(
        [Parameter(Mandatory = $true)]
        [string[]]$Arguments,
        [switch]$IgnoreExitCode
    )

    $output = & (Get-VBoxManagePath) @Arguments 2>&1
    $exitCode = $LASTEXITCODE
    if (-not $IgnoreExitCode -and $exitCode -ne 0) {
        throw ((@("VBoxManage failed:") + $output) -join [Environment]::NewLine)
    }

    return $output
}

function Invoke-Wsl {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Command,
        [switch]$IgnoreExitCode
    )

    $output = & wsl.exe -d $WslDistro -- sh -lc $Command 2>&1
    $exitCode = $LASTEXITCODE
    if (-not $IgnoreExitCode -and $exitCode -ne 0) {
        throw ((@("WSL command failed:", $Command) + $output) -join [Environment]::NewLine)
    }

    return $output
}

function Convert-ToWslPath {
    param([Parameter(Mandatory = $true)][string]$WindowsPath)

    return ((Invoke-Wsl -Command "wslpath -a '$WindowsPath'") | Out-String).Trim()
}

function Write-Utf8NoBomLfFile {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Path,
        [Parameter(Mandatory = $true)]
        [string]$Content
    )

    $normalized = ($Content -replace "`r`n", "`n") -replace "`r", "`n"
    $utf8NoBom = [System.Text.UTF8Encoding]::new($false)
    [System.IO.File]::WriteAllText($Path, $normalized, $utf8NoBom)
}

function Invoke-WslScript {
    param(
        [Parameter(Mandatory = $true)]
        [string]$ScriptContent,
        [switch]$IgnoreExitCode
    )

    $scriptPath = Join-Path $StateDir ([IO.Path]::GetRandomFileName() + ".sh")
    try {
        Write-Utf8NoBomLfFile -Path $scriptPath -Content $ScriptContent
        $wslPath = Convert-ToWslPath $scriptPath
        $output = & wsl.exe -d $WslDistro -- sh $wslPath 2>&1
        $exitCode = $LASTEXITCODE
        if (-not $IgnoreExitCode -and $exitCode -ne 0) {
            throw ((@("WSL script failed:") + $output) -join [Environment]::NewLine)
        }

        return $output
    }
    finally {
        if (Test-Path $scriptPath) {
            Remove-Item $scriptPath -Force
        }
    }
}

function Invoke-GuestScript {
    param(
        [Parameter(Mandatory = $true)]
        [string]$ScriptContent,
        [switch]$IgnoreExitCode
    )

    $wrapperScript = @"
#!/bin/sh
set -eu
sshpass -p '$GuestPassword' ssh -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -p $SshPort $GuestUser@127.0.0.1 'bash -se' <<'REMOTE_SCRIPT'
$ScriptContent
REMOTE_SCRIPT
"@

    return Invoke-WslScript -ScriptContent $wrapperScript -IgnoreExitCode:$IgnoreExitCode
}

function Copy-ToGuest {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Source,
        [Parameter(Mandatory = $true)]
        [string]$Destination
    )

    $sourceWslPath = Convert-ToWslPath $Source
    $wrapperScript = @"
#!/bin/sh
set -eu
sshpass -p '$GuestPassword' scp -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -P $SshPort '$sourceWslPath' '$GuestUser@127.0.0.1:$Destination'
"@

    Invoke-WslScript -ScriptContent $wrapperScript | Out-Null
}

function Get-CargoPath {
    $cargoPath = Join-Path $HOME ".cargo\bin\cargo.exe"
    if (Test-Path $cargoPath) {
        return $cargoPath
    }

    $command = Get-Command cargo.exe -ErrorAction SilentlyContinue
    if ($command) {
        return $command.Source
    }

    throw "Unable to locate cargo.exe"
}

function Ensure-SourceArchive {
    if (Test-Path $SourceArchivePath) {
        Remove-Item $SourceArchivePath -Force
    }

    Push-Location $RepoRoot
    try {
        & tar.exe -cf $SourceArchivePath Cargo.toml README.md apps crates docs ops
        if ($LASTEXITCODE -ne 0) {
            throw "tar.exe failed to create the source archive"
        }
    }
    finally {
        Pop-Location
    }
}

function Get-CurrentSourceRevision {
    Ensure-SourceArchive
    return (Get-FileHash -Algorithm SHA256 $SourceArchivePath).Hash.ToLowerInvariant()
}

function Ensure-GuestKeyPair {
    if ((Test-Path $GuestPrivateKeyPath) -and (Test-Path $GuestPublicKeyPath)) {
        return
    }

    & ssh-keygen -q -t ed25519 -N "" -f $GuestPrivateKeyPath | Out-Null
    if ($LASTEXITCODE -ne 0) {
        throw "ssh-keygen failed while creating the guest admin key"
    }
}

function Ensure-CloudLocalds {
    Invoke-Wsl -Command "if ! command -v cloud-localds >/dev/null 2>&1; then sudo apt-get update && sudo DEBIAN_FRONTEND=noninteractive apt-get install -y cloud-image-utils; fi" | Out-Null
}

function Write-SeedFiles {
    Ensure-GuestKeyPair
    $publicKey = (Get-Content $GuestPublicKeyPath -Raw).Trim()
    $instanceId = $VmName.ToLowerInvariant()

    $userData = @"
#cloud-config
hostname: $VmName
manage_etc_hosts: true
ssh_pwauth: true
chpasswd:
  list: |
        ${GuestUser}:${GuestPassword}
  expire: false
ssh_authorized_keys:
  - $publicKey
package_update: true
packages:
  - openssh-server
    - build-essential
    - pkg-config
    - libasound2-dev
    - ffmpeg
  - xfce4
  - lightdm
  - xterm
  - openbox
  - x11-apps
  - xauth
  - virtualbox-guest-utils
write_files:
  - path: /etc/lightdm/lightdm.conf.d/99-qubox-autologin.conf
    permissions: '0644'
    content: |
      [Seat:*]
      autologin-user=$GuestUser
      autologin-user-timeout=0
      user-session=xfce
    - path: /etc/xdg/autostart/qubox-visible.desktop
        permissions: '0644'
        content: |
            [Desktop Entry]
            Type=Application
            Name=Qubox Visible Terminal
            Exec=xterm -fa Monospace -fs 14 -geometry 100x30+60+60 -title qubox-vbox
            X-GNOME-Autostart-enabled=true
            OnlyShowIn=XFCE;
runcmd:
  - systemctl set-default graphical.target
    - sh -lc 'echo /usr/sbin/lightdm > /etc/X11/default-display-manager'
    - ln -sf /lib/systemd/system/lightdm.service /etc/systemd/system/display-manager.service
    - systemctl disable gdm3 || true
  - systemctl restart ssh
    - systemctl daemon-reload
    - systemctl restart lightdm
"@

    $metaData = @"
instance-id: $instanceId
local-hostname: $VmName
"@

    Set-Content -Path $UserDataPath -Value $userData -NoNewline
    Set-Content -Path $MetaDataPath -Value $metaData -NoNewline
}

function Ensure-SeedIso {
    Write-SeedFiles
    Ensure-CloudLocalds

    $userDataWslPath = Convert-ToWslPath $UserDataPath
    $metaDataWslPath = Convert-ToWslPath $MetaDataPath
    $seedIsoWslPath = Convert-ToWslPath $SeedIsoPath

    Invoke-Wsl -Command "cloud-localds '$seedIsoWslPath' '$userDataWslPath' '$metaDataWslPath'" | Out-Null
}

function Ensure-ApplianceDownload {
    if (Test-Path $OvaPath) {
        return
    }

    Invoke-WebRequest -Uri $CloudImageUrl -OutFile $OvaPath
}

function Test-VmExists {
    $listing = (Invoke-VBoxManage -Arguments @("list", "vms") -IgnoreExitCode | Out-String)
    return $listing -match ('"' + [Regex]::Escape($VmName) + '"')
}

function Test-VmRunning {
    $listing = (Invoke-VBoxManage -Arguments @("list", "runningvms") -IgnoreExitCode | Out-String)
    return $listing -match ('"' + [Regex]::Escape($VmName) + '"')
}

function Ensure-HostOnlyInterface {
    $listing = (Invoke-VBoxManage -Arguments @("list", "hostonlyifs") -IgnoreExitCode | Out-String)
    if ($listing -match ("(?m)^Name:\s+" + [Regex]::Escape($HostOnlyInterfaceName) + "\s*$")) {
        return $HostOnlyInterfaceName
    }

    $createOutput = (Invoke-VBoxManage -Arguments @("hostonlyif", "create") | Out-String)
    if ($createOutput -match "Interface '([^']+)' was successfully created") {
        $createdName = $Matches[1]
        Invoke-VBoxManage -Arguments @(
            "hostonlyif", "ipconfig", $createdName,
            "--ip", $HostOnlyHostIp,
            "--netmask", "255.255.255.0"
        ) | Out-Null
        return $createdName
    }

    throw "VBoxManage hostonlyif create did not report the created adapter name"
}

function Ensure-VmImported {
    if (Test-VmExists) {
        return
    }

    Ensure-ApplianceDownload
    Invoke-VBoxManage -Arguments @("import", $OvaPath, "--vsys", "0", "--vmname", $VmName, "--basefolder", $VmBaseFolder) | Out-Null
}

function Remove-NatPortForwardRule {
    param([string]$RuleName)

    Invoke-VBoxManage -Arguments @("modifyvm", $VmName, "--natpf1", "delete", $RuleName) -IgnoreExitCode | Out-Null
}

function Ensure-SeedController {
    $info = (Invoke-VBoxManage -Arguments @("showvminfo", $VmName, "--machinereadable") | Out-String)
    if ($info -match 'storagecontrollername[0-9]+="IDE"') {
        return "IDE"
    }

    if ($info -notmatch 'storagecontrollername[0-9]+="Seed IDE"') {
        Invoke-VBoxManage -Arguments @("storagectl", $VmName, "--name", "Seed IDE", "--add", "ide") | Out-Null
    }

    return "Seed IDE"
}

function Configure-Vm {
    Ensure-VmImported
    Ensure-SeedIso
    $seedController = Ensure-SeedController
    $hostOnlyInterface = Ensure-HostOnlyInterface

    Invoke-VBoxManage -Arguments @(
        "modifyvm", $VmName,
        "--memory", "$MemoryMB",
        "--cpus", "$CpuCount",
        "--vram", "$VramMB",
        "--graphicscontroller", "vmsvga",
        "--audio-enabled", "off",
        "--clipboard-mode", "bidirectional",
        "--draganddrop", "disabled",
        "--nic1", "nat",
        "--nic2", "hostonly",
        "--hostonlyadapter2", $hostOnlyInterface,
        "--cableconnected2", "on",
        "--boot1", "disk",
        "--boot2", "dvd",
        "--boot3", "none",
        "--boot4", "none"
    ) | Out-Null

    Invoke-VBoxManage -Arguments @(
        "setextradata", $VmName, "CustomVideoMode1", "${DisplayWidth}x${DisplayHeight}x32"
    ) | Out-Null

    Remove-NatPortForwardRule -RuleName "guestssh"
    Remove-NatPortForwardRule -RuleName "guestquictcp"
    Remove-NatPortForwardRule -RuleName "guestquicudp"

    Invoke-VBoxManage -Arguments @("modifyvm", $VmName, "--natpf1", "guestssh,tcp,127.0.0.1,$SshPort,,22") | Out-Null

    Invoke-VBoxManage -Arguments @("storageattach", $VmName, "--storagectl", $seedController, "--port", "1", "--device", "0", "--type", "dvddrive", "--medium", $SeedIsoPath) | Out-Null
}

function Wait-ForGuestSsh {
    param([int]$TimeoutSeconds = 600)

    $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
    while ((Get-Date) -lt $deadline) {
        $client = [System.Net.Sockets.TcpClient]::new()
        try {
            $task = $client.ConnectAsync("127.0.0.1", $SshPort)
            if ($task.Wait(1000) -and $client.Connected) {
                return $true
            }
        }
        catch {
        }
        finally {
            $client.Dispose()
        }

        Start-Sleep -Seconds 5
    }

    return $false
}

function Ensure-GuestInstall {
    Start-Vm
    if (-not (Wait-ForGuestSsh -TimeoutSeconds 900)) {
        throw "Timed out waiting for guest SSH on port $SshPort"
    }

    $sourceRevision = Get-CurrentSourceRevision
    $installedRevision = (Invoke-GuestScript -ScriptContent "cat '$GuestSourceRevisionPath' 2>/dev/null || true" -IgnoreExitCode | Out-String).Trim()
    if ($installedRevision -eq $sourceRevision) {
        Ensure-GuestHostOnlyNetworking
        Set-GuestDisplayResolution
        return
    }

    Copy-ToGuest -Source $SourceArchivePath -Destination "/home/$GuestUser/qubox-source.tar"

    $remoteScript = @"
set -euxo pipefail
if [ ! -x "/home/$GuestUser/.cargo/bin/cargo" ]; then
  curl https://sh.rustup.rs -sSf | sh -s -- -y
fi
source "/home/$GuestUser/.cargo/env"
export DEBIAN_FRONTEND=noninteractive
sudo apt-get update
sudo apt-get install -y build-essential pkg-config libasound2-dev ffmpeg xterm openbox x11-apps xauth xfce4 lightdm
sudo sh -lc 'echo /usr/sbin/lightdm > /etc/X11/default-display-manager'
sudo ln -sf /lib/systemd/system/lightdm.service /etc/systemd/system/display-manager.service
sudo systemctl disable gdm3 || true
sudo systemctl daemon-reload
sudo systemctl restart lightdm || true
mkdir -p "/home/$GuestUser/.config/autostart"
cat > "/home/$GuestUser/.config/autostart/qubox-visible.desktop" <<'EOF'
[Desktop Entry]
Type=Application
Name=Qubox Visible Terminal
Exec=xterm -fa Monospace -fs 14 -geometry 100x30+60+60 -title qubox-vbox
X-GNOME-Autostart-enabled=true
OnlyShowIn=XFCE;
EOF
mkdir -p "/home/$GuestUser/src/qubox"
rm -rf "/home/$GuestUser/src/qubox/Cargo.toml" "/home/$GuestUser/src/qubox/Cargo.lock" "/home/$GuestUser/src/qubox/README.md" "/home/$GuestUser/src/qubox/apps" "/home/$GuestUser/src/qubox/crates" "/home/$GuestUser/src/qubox/docs" "/home/$GuestUser/src/qubox/ops"
tar -xf "/home/$GuestUser/qubox-source.tar" -C "/home/$GuestUser/src/qubox"
cd "/home/$GuestUser/src/qubox"
cargo build -p host-agent
printf '%s\n' '$sourceRevision' > '$GuestSourceRevisionPath'
"@

    Invoke-GuestScript -ScriptContent $remoteScript | Out-Null
        Ensure-GuestHostOnlyNetworking
        Set-GuestDisplayResolution
}

function Ensure-GuestHostOnlyNetworking {
        $remoteScript = @"
set -euxo pipefail
default_iface=`$(ip route show default | awk '/default/ {print `$5; exit}')
hostonly_iface=`$(ip -o link show | awk -F': ' '{print `$2}' | grep -v '^lo`$' | grep -vx "`$default_iface" | head -n1)

if [ -z "`$hostonly_iface" ]; then
    echo "Unable to determine the VirtualBox host-only interface inside the guest" >&2
    exit 1
fi

sudo ip link set "`$hostonly_iface" up
cat > /tmp/60-qubox-hostonly.yaml <<'EOF'
network:
    version: 2
    ethernets:
        __HOSTONLY_IFACE__:
            dhcp4: false
            addresses:
                - $GuestHostOnlyIp/$HostOnlyPrefixLength
EOF
sed -i "s/__HOSTONLY_IFACE__/`$hostonly_iface/" /tmp/60-qubox-hostonly.yaml
sudo mv /tmp/60-qubox-hostonly.yaml /etc/netplan/60-qubox-hostonly.yaml
sudo netplan apply

for _ in 1 2 3 4 5; do
    assigned_ip=`$(ip -o -4 addr show dev "`$hostonly_iface" | awk '{print `$4}' | cut -d/ -f1 | head -n1)
    if [ "`$assigned_ip" = "$GuestHostOnlyIp" ]; then
        echo "`$assigned_ip"
        exit 0
    fi
    sleep 1
done

echo "Guest host-only interface `$hostonly_iface did not receive $GuestHostOnlyIp" >&2
exit 1
"@

        Invoke-GuestScript -ScriptContent $remoteScript | Out-Null
}

function Set-GuestDisplayResolution {
        Invoke-VBoxManage -Arguments @(
                "controlvm", $VmName, "setvideomodehint", "$DisplayWidth", "$DisplayHeight", "32"
        ) -IgnoreExitCode | Out-Null

        $remoteScript = @"
set -euxo pipefail
output=`$(DISPLAY=:0 XAUTHORITY="/home/$GuestUser/.Xauthority" xrandr --query 2>/dev/null | awk '/ connected/ {print `$1; exit}')
if [ -n "`$output" ]; then
    preferred_mode="${DisplayWidth}x${DisplayHeight}"
    selected_mode=""
    for candidate in "`$preferred_mode" "1920x1080" "1680x1050" "1440x900"; do
        if DISPLAY=:0 XAUTHORITY="/home/$GuestUser/.Xauthority" xrandr --query | grep -q "^[[:space:]]*`$candidate[[:space:]]"; then
            selected_mode="`$candidate"
            break
        fi
    done

    if [ -n "`$selected_mode" ]; then
        DISPLAY=:0 XAUTHORITY="/home/$GuestUser/.Xauthority" xrandr --output "`$output" --mode "`$selected_mode" || true
    fi
fi
"@

        Invoke-GuestScript -ScriptContent $remoteScript -IgnoreExitCode | Out-Null
}

function Start-GuestVisibleWindow {
    Set-GuestDisplayResolution
    Invoke-GuestScript -ScriptContent @"
DISPLAY=:0 XAUTHORITY=/home/$GuestUser/.Xauthority nohup xterm -fa Monospace -fs 14 -geometry 100x30+60+60 -title qubox-vbox </dev/null >/tmp/qubox-xterm.log 2>&1 &
"@ -IgnoreExitCode | Out-Null
}

function Start-GuestHostAgent {
    Ensure-GuestHostOnlyNetworking
    Set-GuestDisplayResolution
    $remoteScript = @"
set -euxo pipefail
source "/home/$GuestUser/.cargo/env"
cd "/home/$GuestUser/src/qubox"
pkill -f 'target/debug/host-agent' || true
DISPLAY=:0 XAUTHORITY="/home/$GuestUser/.Xauthority" nohup xterm -fa Monospace -fs 14 -geometry 100x30+60+60 -title qubox-vbox </dev/null >/tmp/qubox-xterm.log 2>&1 &
sleep 2
DISPLAY=:0 XAUTHORITY="/home/$GuestUser/.Xauthority" RUST_LOG="info,host_agent=trace,qubox_transport=trace" nohup ./target/debug/host-agent --server $ServerUrl --name $GuestDisplayName --linux-capture auto --x11-display :0.0 --h264-encoder libx264 --disable-audio --auto-approve-pairing --native-quic-bind 0.0.0.0:$QuicPort --native-quic-advertise-ip $GuestHostOnlyIp --media-width $DisplayWidth --media-height $DisplayHeight --media-fps 15 --media-bitrate-kbps 1200 > "/home/$GuestUser/qubox-host-agent.log" 2>&1 &
sleep 3
tail -n 40 "/home/$GuestUser/qubox-host-agent.log" || true
"@

    Invoke-GuestScript -ScriptContent $remoteScript | Out-Null
}

function Ensure-LocalClientCliBuild {
    & (Get-CargoPath) build -p client-cli
    if ($LASTEXITCODE -ne 0) {
        throw "cargo build -p client-cli failed"
    }
}

function Invoke-ClientCli {
    param(
        [Parameter(Mandatory = $true)]
        [string[]]$Arguments,
        [switch]$IgnoreExitCode
    )

    $clientPath = Join-Path $RepoRoot "target\debug\client-cli.exe"
    if (-not (Test-Path $clientPath)) {
        Ensure-LocalClientCliBuild
    }

    $previousRustLog = $env:RUST_LOG
    try {
        if (-not $previousRustLog) {
            $env:RUST_LOG = "info,client_cli=trace,qubox_transport=trace"
        }

        & $clientPath --server $ServerUrl @Arguments
        $exitCode = $LASTEXITCODE
    }
    finally {
        if ($null -ne $previousRustLog) {
            $env:RUST_LOG = $previousRustLog
        }
        else {
            Remove-Item Env:RUST_LOG -ErrorAction SilentlyContinue
        }
    }

    if (-not $IgnoreExitCode -and $exitCode -ne 0) {
        throw "client-cli failed: $($Arguments -join ' ')"
    }
}

function Invoke-TestHandoff {
    Ensure-GuestInstall
    Start-GuestVisibleWindow
    Start-GuestHostAgent
    Ensure-LocalClientCliBuild

    Start-Sleep -Seconds 5
    Invoke-ClientCli -Arguments @("list-hosts") -IgnoreExitCode
    Invoke-ClientCli -Arguments @("pair", "--host", $GuestDisplayName) -IgnoreExitCode

    Invoke-ClientCli -Arguments @(
        "start-session",
        "--host", $GuestDisplayName,
        "--transport", "native-quic",
        "--codec", "h264",
        "--mute-playback",
        "--max-stream-frames", "120"
    )
}

function Show-Status {
    $info = if (Test-VmExists) {
        (Invoke-VBoxManage -Arguments @("showvminfo", $VmName, "--machinereadable") | Out-String)
    } else {
        $null
    }

    [pscustomobject]@{
        VBoxManage = Get-VBoxManagePath
        OvaPresent = Test-Path $OvaPath
        SeedIsoPresent = Test-Path $SeedIsoPath
        VmExists = [bool](Test-VmExists)
        VmRunning = [bool](Test-VmRunning)
        VmBaseFolder = $VmBaseFolder
        HostOnlyInterface = $HostOnlyInterfaceName
        GuestAdvertiseIp = $GuestHostOnlyIp
        DisplayResolution = "${DisplayWidth}x${DisplayHeight}"
        SshPort = $SshPort
        SshReady = Wait-ForGuestSsh -TimeoutSeconds 1
        MachineState = if ($info -and $info -match 'VMState="([^"]+)"') { $Matches[1] } else { "missing" }
    } | Format-List
}

function Start-Vm {
    if (Test-VmRunning) {
        return
    }

    Configure-Vm
    Invoke-VBoxManage -Arguments @("startvm", $VmName, "--type", $StartType) | Out-Null
    Invoke-VBoxManage -Arguments @(
        "controlvm", $VmName, "setvideomodehint", "$DisplayWidth", "$DisplayHeight", "32"
    ) -IgnoreExitCode | Out-Null
}

function Stop-Vm {
    if (Test-VmRunning) {
        Invoke-VBoxManage -Arguments @("controlvm", $VmName, "acpipowerbutton") | Out-Null
    }
}

function Destroy-Vm {
    if (Test-VmRunning) {
        Invoke-VBoxManage -Arguments @("controlvm", $VmName, "poweroff") -IgnoreExitCode | Out-Null
        Start-Sleep -Seconds 2
    }

    if (Test-VmExists) {
        Invoke-VBoxManage -Arguments @("unregistervm", $VmName, "--delete") -IgnoreExitCode | Out-Null
    }

    foreach ($path in @($RuntimeRoot)) {
        if (Test-Path $path) {
            Remove-Item $path -Recurse -Force
        }
    }
}

switch ($Action) {
    "prepare" {
        Configure-Vm
        Show-Status
    }
    "start" {
        Start-Vm
        Show-Status
    }
    "stop" {
        Stop-Vm
        Show-Status
    }
    "destroy" {
        Destroy-Vm
        Show-Status
    }
    "install" {
        Ensure-GuestInstall
        Start-GuestVisibleWindow
        Show-Status
    }
    "status" {
        Show-Status
    }
    "test" {
        Invoke-TestHandoff
    }
}