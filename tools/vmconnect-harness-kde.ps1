# vmconnect-harness-kde.ps1 - self-asserting vmconnect-equivalent regression
# test for wayrdp's KDE backend (Plasma 6.3+ guests: Kali, Parrot).
#
# Sibling of vmconnect-harness.ps1 (the Hyprland gate). The wire half is the
# same: tools/vmconnect-probe.exe drives AF_HYPERV -> raw MCS Connect-Initial
# like real vmconnect. The guest half differs because KDE fails differently:
# the risks are a blank or off-origin desktop, physical outputs left
# disabled after a session, KWin/plasmashell crashes, and a session lease
# that never comes back.
#
#   K0  gate          - kwin_wayland + plasmashell running, wayrdp unit
#                       active and on the kde backend, KWin >= 6.3, a clean
#                       output layout (no stale Virtual-wayrdp, >= 1 enabled
#                       physical output), auto-lock off, /tmp writable.
#   A1  connection    - prober completed the vmconnect handshake
#   A2  single-peer   - exactly one peer_accepted this run
#   A3  activation    - gate reached activated=1
#   K4  output        - "session: live at" once; Virtual-wayrdp enabled and
#                       every physical output disabled DURING the hold
#   K5  first frame   - PipeWire delivered a frame
#   A7  content       - health sampled a rendered (non-uniform) desktop
#   K6  no give-up    - no "no usable stream" / lease refusal this run
#   K2b input         - EIS connected (key delivery itself is not asserted:
#                       there is no compositor-neutral focus control yet)
#   K7  teardown      - "teardown: done"; Virtual-wayrdp gone and the
#                       baseline physical outputs enabled again
#   K8  churn         - 6 rapid 2 s connects; afterwards the same KWin pid,
#                       plasmashell alive, layout restored, no lease refusal
#   K9  e2e liveness  - KWin pid unchanged across the whole run, no new
#                       kwin_wayland/plasmashell core dumps
#
# Logs: wayrdp's stderr is read from /tmp/wayrdp-trace.log when a drop-in
# redirects it there (VMCreate's 98-trace-file.conf), else from the user
# journal. Either way only lines from this run count.
#
# Usage: pwsh -File .\vmconnect-harness-kde.ps1 -VmName "wayrdp-kde-kali*"
#   RUN WITH pwsh (PS7). Exit 0 = all assertions PASS; 1 = something FAILED.

[CmdletBinding()]
param(
    [string]$VmName      = "wayrdp-kde-*",
    [int]$HoldSec        = 15,       # > extras' 4 s reattach + layout settle
    [int]$GateWaitSec    = 8,        # teardown restores outputs (~1-2 s) + slack
    [string]$GuestUser   = "vmcreate",
    [string]$GuestIp     = "",
    [string]$SshKey      = "$env:LOCALAPPDATA\VMCreate\ssh\vmcreate_ed25519",
    [string]$VmId        = "",
    [switch]$SkipChurn
)

$ErrorActionPreference = "Stop"
$tools   = Split-Path -Parent $MyInvocation.MyCommand.Path
$probe   = Join-Path $tools "vmconnect-probe.exe"
$results = New-Object System.Collections.Generic.List[string]
$failed  = $false

function Assert($name, [bool]$ok, $detail) {
    if ($ok) { $script:results.Add("[PASS] $name :: $detail") }
    else     { $script:results.Add("[FAIL] $name :: $detail"); $script:failed = $true }
}

# Session environment for tools that talk to KWin/D-Bus from an ssh shell.
$SessionEnv = 'export XDG_RUNTIME_DIR=/run/user/$(id -u); export DBUS_SESSION_BUS_ADDRESS=unix:path=$XDG_RUNTIME_DIR/bus; export WAYLAND_DISPLAY=$(ls $XDG_RUNTIME_DIR 2>/dev/null | grep -E "^wayland-[0-9]+$" | head -1); export QT_QPA_PLATFORM=wayland LC_ALL=C.UTF-8;'

function Guest([string]$cmd) {
    & ssh -i $SshKey -o StrictHostKeyChecking=no -o ConnectTimeout=8 -o BatchMode=yes "$GuestUser@$GuestIp" "$SessionEnv $cmd" 2>&1 | Out-String
}

function Verdict {
    Write-Host ""
    Write-Host "=== vmconnect-harness-kde assertions ==="
    foreach ($r in $script:results) { Write-Host "  $r" }
    if ($script:failed) { Write-Host "[harness-kde] FAIL"; exit 1 }
    Write-Host "[harness-kde] PASS"; exit 0
}

# "name enabled|disabled" per output, ANSI stripped.
# Layout differs across Plasma versions: older kscreen-doctor puts
# "enabled" on the Output: line, 6.7 prints a UUID there and each attribute
# on its own tab-indented line. Only the name ($3) is fixed.
$LayoutCmd = 'kscreen-doctor -o 2>/dev/null | sed "s/\x1b\[[0-9;]*m//g" | awk ''function out() { if (name != "") print name, s } /^Output:/ { out(); name = $3; s = "disabled"; for (i = 4; i <= NF; i++) if ($i == "enabled") s = "enabled"; next } NF == 1 && $1 == "enabled" { s = "enabled" } END { out() }'''
function Layout {
    $map = @{}
    foreach ($l in ((Guest $LayoutCmd) -split "`r?`n")) {
        if ($l -match '^(\S+) (enabled|disabled)$') { $map[$Matches[1]] = $Matches[2] }
    }
    return $map
}
function Physical($map) { @($map.Keys | Where-Object { $_ -ne 'Virtual-wayrdp' }) }
function EnabledPhysical($map) { @(Physical $map | Where-Object { $map[$_] -eq 'enabled' } | Sort-Object) }

# --- Resolve VM + service GUID + guest IP (as in vmconnect-harness.ps1) ----
$rdpGuid = Get-ChildItem "HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Virtualization\GuestCommunicationServices" |
    Where-Object { (Get-ItemProperty $_.PsPath).ElementName -match 'RDP 3389' } |
    Select-Object -First 1 -ExpandProperty PSChildName
if (-not $rdpGuid) { Write-Error "No 'RDP 3389' guest service registered on this host"; exit 1 }
$vmNameResolved = ""
if ($VmId) {
    $vmGuidStr = $VmId
} else {
    $vm = Get-VM -Name $VmName -ErrorAction Stop | Sort-Object CreationTime -Descending | Select-Object -First 1
    if ($vm.State -ne 'Running') { Write-Error "VM '$($vm.Name)' is $($vm.State), need Running"; exit 1 }
    $vmGuidStr      = $vm.Id
    $vmNameResolved = $vm.Name
}
Write-Host "[harness-kde] VM=$(if ($vmNameResolved) { $vmNameResolved } else { $VmName }) ID=$vmGuidStr svc=$rdpGuid"
if (-not $GuestIp) {
    for ($try = 0; -not $GuestIp -and $try -lt 12; $try++) {
        $nics = if ($VmId) { Get-VMNetworkAdapter -VMId $vmGuidStr -ErrorAction SilentlyContinue }
                else       { Get-VMNetworkAdapter -VMName $vmNameResolved -ErrorAction SilentlyContinue }
        if ($nics) { $GuestIp = ($nics.IPAddresses | Where-Object { $_ -match '^\d+\.\d+\.\d+\.\d+$' } | Select-Object -First 1) }
        if (-not $GuestIp) { Start-Sleep -Seconds 2 }
    }
}
if (-not $GuestIp) { Write-Error "Could not resolve guest IP (pass -GuestIp)"; exit 1 }
if ((Guest 'echo reachable').Trim() -notmatch 'reachable') {
    Write-Error "Guest $GuestUser@$GuestIp unreachable over SSH (stale IP after a reboot?)"; exit 1
}
Write-Host "[harness-kde] guest=$GuestUser@$GuestIp (ssh reachable)"

if (-not (Test-Path $probe) -or (Get-Item "$tools\vmconnect-probe.cs").LastWriteTime -gt (Get-Item $probe).LastWriteTime) {
    $csc = "$env:WINDIR\Microsoft.NET\Framework64\v4.0.30319\csc.exe"
    & $csc /nologo /out:$probe "$tools\vmconnect-probe.cs" | Out-Null
    if (-not (Test-Path $probe)) { Write-Error "vmconnect-probe build failed"; exit 1 }
}

# --- Log access: only lines written after a mark ---------------------------
function Mark {
    $n = (Guest '[ -f /tmp/wayrdp-trace.log ] && wc -l < /tmp/wayrdp-trace.log || echo -1').Trim() -as [int]
    $t = (Guest 'date +%s').Trim()
    return @{ Lines = $n; Epoch = $t }
}
function TraceSince($mark) {
    if ($mark.Lines -ge 0) { Guest "tail -n +$($mark.Lines + 1) /tmp/wayrdp-trace.log" }
    else { Guest "journalctl --user -u wayrdp -o cat --no-pager --since @$($mark.Epoch)" }
}
function Count($text, $pattern) { ([regex]::Matches($text, $pattern)).Count }
function Pid1($name) { (Guest "pgrep -x $name | head -1").Trim() }

# --- K0: guest health gate --------------------------------------------------
$kwinPid  = Pid1 'kwin_wayland'
$shellPid = Pid1 'plasmashell'
Assert "K0a-kwin-running" ($kwinPid -ne '') "kwin_wayland pid=$kwinPid"
Assert "K0b-plasmashell-running" ($shellPid -ne '') "plasmashell pid=$shellPid"
$unit = (Guest 'systemctl --user is-active wayrdp 2>&1').Trim()
Assert "K0c-wayrdp-active" ($unit -eq 'active') "systemctl --user is-active wayrdp = $unit"
$svcLog = Guest 'S=$(systemctl --user show wayrdp -p ActiveEnterTimestamp --value); { [ -f /tmp/wayrdp-trace.log ] && tail -n 400 /tmp/wayrdp-trace.log; journalctl --user -u wayrdp -o cat --no-pager --since "$S"; } 2>/dev/null'
Assert "K0d-kde-backend" ((Count $svcLog 'preflight: KWin') -ge 1) "daemon log has '[backend-kde] preflight: KWin' (unit runs the kde backend)"
$kwinVer = (Guest 'kwin_wayland --version 2>/dev/null').Trim()
$verOk = $kwinVer -match 'kwin\S* (\d+)\.(\d+)' -and (([int]$Matches[1] -gt 6) -or ([int]$Matches[1] -eq 6 -and [int]$Matches[2] -ge 3))
Assert "K0e-kwin-version" $verOk "kwin_wayland --version: $kwinVer (need >= 6.3)"
$layout0 = Layout
$phys0 = EnabledPhysical $layout0
Assert "K0f-layout-clean" ((-not $layout0.ContainsKey('Virtual-wayrdp')) -and $phys0.Count -ge 1) "outputs: $(($layout0.GetEnumerator() | ForEach-Object { "$($_.Key)=$($_.Value)" }) -join ', ')"
$autolock = (Guest 'kreadconfig6 --file kscreenlockerrc --group Daemon --key Autolock 2>/dev/null || kreadconfig5 --file kscreenlockerrc --group Daemon --key Autolock 2>/dev/null').Trim()
Assert "K0g-autolock-off" ($autolock -eq 'false') "kscreenlockerrc Daemon/Autolock = '$autolock' (the lock screen is not streamed yet)"
$tmpOk = (Guest 'echo probe > /tmp/.k0-probe && rm -f /tmp/.k0-probe && df -m --output=avail /tmp | tail -1 | tr -d " "').Trim() -as [int]
Assert "K0h-tmp-headroom" ($tmpOk -ge 100) "/tmp writable, free=${tmpOk}M (need >= 100M)"
if ($failed) { Write-Host "[harness-kde] guest unhealthy - fix before the RDP test"; Verdict }
$coreEpoch = (Guest 'date +%s').Trim()

# --- A1: one held session, layout sampled during the hold -------------------
$mark = Mark
$probeJob = Start-Job -ScriptBlock {
    $p = $args[0]; $rest = $args[1..($args.Count - 1)]
    & $p @rest 2>&1 | Out-String
} -ArgumentList @($probe, $vmGuidStr, $rdpGuid, '--seconds', "$HoldSec", '--send-input')
$sawVirtual = $false; $sawGuarded = $false; $holdSamples = 0
$deadline = (Get-Date).AddSeconds($HoldSec + 25)
while ((Get-Date) -lt $deadline -and $probeJob.State -eq 'Running') {
    $l = Layout
    if ($l.Count -gt 0) { $holdSamples++ }
    if ($l['Virtual-wayrdp'] -eq 'enabled') {
        $sawVirtual = $true
        if ((EnabledPhysical $l).Count -eq 0) { $sawGuarded = $true }
    }
    Start-Sleep -Milliseconds 1000
}
$probeOut = Receive-Job $probeJob -Wait -AutoRemoveJob
Write-Host $probeOut
Assert "A1-connection" ($probeOut -match 'ALL_STAGES_PASS') "prober tail: $(($probeOut.Trim() -split "`n")[-1])"
Start-Sleep -Seconds $GateWaitSec
$trace = TraceSince $mark

$accepts = Count $trace 'peer_accepted\] vsock peer'
Assert "A2-single-peer" ($accepts -eq 1) "peer_accepted=$accepts (exactly one prober connect expected)"
$activated = Count $trace 'peer_mainloop\] gate:.*activated=1'
Assert "A3-activation" ($activated -ge 1) "gate lines with activated=1: $activated"
$live = Count $trace 'session: live at'
Assert "K4a-virtual-output-created" ($live -eq 1) "'session: live at' x$live"
Assert "K4b-virtual-output-enabled" $sawVirtual "Virtual-wayrdp enabled in a hold sample ($holdSamples layout samples)"
Assert "K4c-layout-guard-engaged" $sawGuarded "all physical outputs disabled while Virtual-wayrdp streamed"
Assert "K5-first-frame" ((Count $trace 'capture: first frame') -ge 1) "$(([regex]::Match($trace, 'capture: first frame[^\r\n]*')).Value)"
$content = Count $trace 'health: desktop content Content'
$blank = ([regex]::Matches($trace, 'health: desktop content Uniform\([A-Za-z]+\)') | ForEach-Object { $_.Value }) -join '; '
Assert "A7-desktop-rendered" ($content -ge 1) "rendered samples seen: $content; blank transitions: $(if ($blank) { $blank } else { 'none' })"
$giveUp = Count $trace 'no usable stream|did not release in|session: virtual output .* failed'
Assert "K6-no-give-up" ($giveUp -eq 0) "give-up / lease-refusal / create-failure lines: $giveUp"
$heals = Count $trace 'health: layout heal'
if ($heals -gt 0) { $results.Add("[INFO] K6-heals :: $heals layout heal(s) this session") }
Assert "K2b-eis-connected" ((Count $trace 'input: EIS connected') -ge 1) "'input: EIS connected' present (key delivery not asserted)"
$teardown = Count $trace 'teardown: done'
$layout1 = Layout
$phys1 = EnabledPhysical $layout1
Assert "K7a-teardown-done" ($teardown -ge 1) "'teardown: done' x$teardown"
Assert "K7b-virtual-output-gone" (-not $layout1.ContainsKey('Virtual-wayrdp')) "outputs after: $(($layout1.GetEnumerator() | ForEach-Object { "$($_.Key)=$($_.Value)" }) -join ', ')"
Assert "K7c-physical-restored" ((($phys1 -join ',') -eq ($phys0 -join ','))) "enabled physical before=[$($phys0 -join ',')] after=[$($phys1 -join ',')]"

# --- K8: reconnect churn ----------------------------------------------------
if (-not $SkipChurn) {
    $mark = Mark
    for ($i = 0; $i -lt 6; $i++) {
        & $probe $vmGuidStr $rdpGuid --seconds 2 2>&1 | Out-Null
    }
    Start-Sleep -Seconds ($GateWaitSec + 4)
    $churn = TraceSince $mark
    $layout2 = Layout
    $phys2 = EnabledPhysical $layout2
    $kwinPid2 = Pid1 'kwin_wayland'
    $shellPid2 = Pid1 'plasmashell'
    $lives = Count $churn 'session: live at'
    $downs = Count $churn 'teardown: done'
    Assert "K8a-kwin-survives-churn" ($kwinPid2 -eq $kwinPid) "kwin_wayland pid $kwinPid -> $kwinPid2"
    Assert "K8b-plasmashell-alive" ($shellPid2 -ne '') "plasmashell pid $shellPid -> $shellPid2"
    Assert "K8c-no-lease-refusal" ((Count $churn 'did not release in') -eq 0) "sessions live=$lives teardowns=$downs"
    Assert "K8d-every-session-torn-down" ($downs -ge $lives) "live=$lives teardown done=$downs"
    Assert "K8e-layout-restored" ((-not $layout2.ContainsKey('Virtual-wayrdp')) -and (($phys2 -join ',') -eq ($phys0 -join ','))) "enabled physical=[$($phys2 -join ',')] virtual present=$($layout2.ContainsKey('Virtual-wayrdp'))"
} else {
    $results.Add("[SKIP] K8-churn :: -SkipChurn set")
}

# --- K9: end-to-end compositor liveness ------------------------------------
$kwinPidEnd = Pid1 'kwin_wayland'
Assert "K9a-same-kwin" ($kwinPidEnd -eq $kwinPid) "kwin_wayland pid $kwinPid -> $kwinPidEnd"
$cores = (Guest "coredumpctl --no-pager --no-legend list --since @$coreEpoch 2>/dev/null | grep -cE 'kwin_wayland|plasmashell'").Trim() -as [int]
Assert "K9b-no-core-dumps" (($null -eq $cores) -or ($cores -eq 0)) "kwin_wayland/plasmashell core dumps this run: $cores"
$unitEnd = (Guest 'systemctl --user is-active wayrdp 2>&1').Trim()
Assert "K9c-wayrdp-still-active" ($unitEnd -eq 'active') "wayrdp unit = $unitEnd"

Verdict
