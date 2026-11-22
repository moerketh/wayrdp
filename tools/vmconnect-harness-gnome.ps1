# vmconnect-harness-gnome.ps1 - self-asserting vmconnect-equivalent regression
# test for wayrdp's GNOME backend (Mutter RemoteDesktop/ScreenCast; Kali
# GNOME 50 first).
#
# Sibling of vmconnect-harness-kde.ps1. The wire half is the same probe
# (tools/vmconnect-probe.exe: AF_HYPERV -> raw MCS Connect-Initial). The guest
# half reads Mutter's DisplayConfig over D-Bus: the risks on GNOME are a
# session Mutter refuses (lock screen), a virtual monitor that never
# negotiates, the console's physical monitor left disabled, and a
# gnome-shell crash (gnome-shell IS the compositor: it takes the session
# with it).
#
#   G0  gate          - gnome-shell running, wayrdp unit active and on the
#                       gnome backend, GNOME >= 50, a clean layout (no Meta-*
#                       virtual monitor, >= 1 enabled physical monitor), lock
#                       off and not locked now, /tmp writable
#   A1  connection    - prober completed the vmconnect handshake (--neg-ssl)
#   A2  single-peer   - exactly one peer_accepted this run
#   A3  activation    - gate reached activated=1
#   G4  monitor       - "session: live at" once; a Meta-* monitor enabled and
#                       every physical monitor disabled DURING the hold
#   G5  first frame   - PipeWire delivered a frame
#   A7  content       - health sampled a rendered (non-uniform) desktop
#   G6  no give-up    - no create failure / lease refusal / give-up
#   G2  input         - EIS connected, devices bound, keymap delivered, and
#                       the absolute pointer's region carries Mutter's
#                       mapping id (the region input aims at)
#   G7  teardown      - "teardown: done"; no Meta-* left and the baseline
#                       physical monitors enabled again
#   G8  churn         - 6 rapid 2 s connects; afterwards the same gnome-shell
#                       pid, layout restored, no lease refusal
#   G9  e2e liveness  - gnome-shell pid unchanged, no new gnome-shell core
#                       dumps, wayrdp still active
#
# Logs: /tmp/wayrdp-trace.log when a drop-in redirects stderr there, else the
# user journal; only lines from this run count.
#
# -GuestUser must be the desktop user: the checks need its session bus.
#
# Usage: pwsh -File .\vmconnect-harness-gnome.ps1 -VmName "wayrdp-gnome-kali*"
#   RUN WITH pwsh (PS7). Exit 0 = all assertions PASS; 1 = something FAILED.

[CmdletBinding()]
param(
    [string]$VmName      = "wayrdp-gnome-*",
    [int]$HoldSec        = 15,
    [int]$GateWaitSec    = 6,        # teardown + Mutter's console restore + slack
    [string]$GuestUser   = "kali",
    [string]$GuestIp     = "",
    [string]$SshKey      = "$env:LOCALAPPDATA\VMCreate\ssh\vmcreate_ed25519",
    [string]$VmId        = "",
    [int]$MinGnome       = 50,
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

$SessionEnv = 'export XDG_RUNTIME_DIR=/run/user/$(id -u); export DBUS_SESSION_BUS_ADDRESS=unix:path=$XDG_RUNTIME_DIR/bus; export LC_ALL=C.UTF-8;'

function Guest([string]$cmd) {
    & ssh -i $SshKey -o StrictHostKeyChecking=no -o ConnectTimeout=8 -o BatchMode=yes "$GuestUser@$GuestIp" "$SessionEnv $cmd" 2>&1 | Out-String
}

function Verdict {
    Write-Host ""
    Write-Host "=== vmconnect-harness-gnome assertions ==="
    foreach ($r in $script:results) { Write-Host "  $r" }
    if ($script:failed) { Write-Host "[harness-gnome] FAIL"; exit 1 }
    Write-Host "[harness-gnome] PASS"; exit 0
}

# "connector enabled|disabled" per monitor, from DisplayConfig.GetCurrentState
# (python3-gi ships with GNOME; double quotes only inside the Python).
$LayoutCmd = 'python3 -c ''from gi.repository import Gio
b = Gio.bus_get_sync(Gio.BusType.SESSION)
r = b.call_sync("org.gnome.Mutter.DisplayConfig", "/org/gnome/Mutter/DisplayConfig", "org.gnome.Mutter.DisplayConfig", "GetCurrentState", None, None, 0, 5000, None).unpack()
on = {m[0] for l in r[2] for m in l[5]}
for m in r[1]:
    print(m[0][0], "enabled" if m[0][0] in on else "disabled")'' 2>/dev/null'
function Layout {
    $map = @{}
    foreach ($l in ((Guest $LayoutCmd) -split "`r?`n")) {
        if ($l -match '^(\S+) (enabled|disabled)$') { $map[$Matches[1]] = $Matches[2] }
    }
    return $map
}
function Virtuals($map) { @($map.Keys | Where-Object { $_ -like 'Meta-*' }) }
function EnabledVirtual($map) { @(Virtuals $map | Where-Object { $map[$_] -eq 'enabled' }) }
function EnabledPhysical($map) { @($map.Keys | Where-Object { $_ -notlike 'Meta-*' -and $map[$_] -eq 'enabled' } | Sort-Object) }
function Describe($map) { ($map.GetEnumerator() | Sort-Object Key | ForEach-Object { "$($_.Key)=$($_.Value)" }) -join ', ' }

# --- Resolve VM + service GUID + guest IP ------------------------------------
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
Write-Host "[harness-gnome] VM=$(if ($vmNameResolved) { $vmNameResolved } else { $VmName }) ID=$vmGuidStr svc=$rdpGuid"
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
    Write-Error "Guest $GuestUser@$GuestIp unreachable over SSH (stale IP, or the key is not authorized for the desktop user?)"; exit 1
}
Write-Host "[harness-gnome] guest=$GuestUser@$GuestIp (ssh reachable)"

if (-not (Test-Path $probe) -or (Get-Item "$tools\vmconnect-probe.cs").LastWriteTime -gt (Get-Item $probe).LastWriteTime) {
    $csc = "$env:WINDIR\Microsoft.NET\Framework64\v4.0.30319\csc.exe"
    & $csc /nologo /out:$probe "$tools\vmconnect-probe.cs" | Out-Null
    if (-not (Test-Path $probe)) { Write-Error "vmconnect-probe build failed"; exit 1 }
}

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
function ShellPid { (Guest 'pgrep -u $(id -u) -x gnome-shell | head -1').Trim() }

# --- G0: guest health gate ----------------------------------------------------
$shellPid = ShellPid
Assert "G0a-gnome-shell-running" ($shellPid -ne '') "gnome-shell pid=$shellPid"
$unit = (Guest 'systemctl --user is-active wayrdp 2>&1').Trim()
Assert "G0b-wayrdp-active" ($unit -eq 'active') "systemctl --user is-active wayrdp = $unit"
$svcLog = Guest 'S=$(systemctl --user show wayrdp -p ActiveEnterTimestamp --value); { [ -f /tmp/wayrdp-trace.log ] && tail -n 400 /tmp/wayrdp-trace.log; journalctl --user -u wayrdp -o cat --no-pager --since "$S"; } 2>/dev/null'
Assert "G0c-gnome-backend" ((Count $svcLog 'backend-gnome\] preflight: GNOME') -ge 1) "daemon log has '[backend-gnome] preflight: GNOME'"
$ver = (Guest 'gnome-shell --version 2>/dev/null').Trim()
$verOk = $ver -match 'GNOME Shell (\d+)' -and ([int]$Matches[1] -ge $MinGnome)
Assert "G0d-gnome-version" $verOk "gnome-shell --version: $ver (need >= $MinGnome)"
$layout0 = Layout
$phys0 = EnabledPhysical $layout0
Assert "G0e-layout-clean" ((Virtuals $layout0).Count -eq 0 -and $phys0.Count -ge 1) "monitors: $(Describe $layout0)"
$lockEnabled = (Guest 'gsettings get org.gnome.desktop.screensaver lock-enabled').Trim()
$locked = (Guest 'gdbus call --session -d org.gnome.ScreenSaver -o /org/gnome/ScreenSaver -m org.gnome.ScreenSaver.GetActive').Trim()
Assert "G0f-lock-off" ($lockEnabled -eq 'false' -and $locked -eq '(false,)') "lock-enabled=$lockEnabled, screensaver active=$locked (Mutter refuses remote desktop on the lock screen)"
$tmpOk = (Guest 'echo probe > /tmp/.g0-probe && rm -f /tmp/.g0-probe && df -m --output=avail /tmp | tail -1 | tr -d " "').Trim() -as [int]
Assert "G0g-tmp-headroom" ($tmpOk -ge 100) "/tmp writable, free=${tmpOk}M (need >= 100M)"
if ($failed) { Write-Host "[harness-gnome] guest unhealthy - fix before the RDP test"; Verdict }
$coreEpoch = (Guest 'date +%s').Trim()

# --- A1: one held session, layout sampled during the hold ---------------------
$mark = Mark
$probeJob = Start-Job -ScriptBlock {
    $p = $args[0]; $rest = $args[1..($args.Count - 1)]
    & $p @rest 2>&1 | Out-String
} -ArgumentList @($probe, $vmGuidStr, $rdpGuid, '--seconds', "$HoldSec", '--send-input', '--neg-ssl')
$sawVirtual = $false; $sawGuarded = $false; $holdSamples = 0
$deadline = (Get-Date).AddSeconds($HoldSec + 25)
while ((Get-Date) -lt $deadline -and $probeJob.State -eq 'Running') {
    $l = Layout
    if ($l.Count -gt 0) { $holdSamples++ }
    if ((EnabledVirtual $l).Count -ge 1) {
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
Assert "G4a-virtual-monitor-created" ($live -eq 1) "'session: live at' x$live"
Assert "G4b-virtual-monitor-enabled" $sawVirtual "a Meta-* monitor enabled in a hold sample ($holdSamples layout samples)"
Assert "G4c-layout-guard-engaged" $sawGuarded "every physical monitor disabled while the virtual one streamed"
Assert "G5-first-frame" ((Count $trace 'capture: first frame') -ge 1) "$(([regex]::Match($trace, 'capture: first frame[^\r\n]*')).Value)"
$content = Count $trace 'health: desktop content Content'
$blank = ([regex]::Matches($trace, 'health: desktop content Uniform\([A-Za-z]+\)') | ForEach-Object { $_.Value }) -join '; '
Assert "A7-desktop-rendered" ($content -ge 1) "rendered samples seen: $content; blank transitions: $(if ($blank) { $blank } else { 'none' })"
$giveUp = Count $trace 'no usable stream|did not release in|session: virtual monitor .* failed|refuses remote desktop'
Assert "G6-no-give-up" ($giveUp -eq 0) "give-up / lease-refusal / create-failure lines: $giveUp"
$heals = Count $trace 'health: layout heal|health: recreating'
if ($heals -gt 0) { $results.Add("[INFO] G6-heals :: $heals heal/recreate(s) this session") }
Assert "G2a-eis-connected" ((Count $trace 'input: EIS connected') -ge 1) "'input: EIS connected' present"
$devLines = [regex]::Matches($trace, 'input: EIS device [^\r\n]*') | ForEach-Object { $_.Value }
Assert "G2b-eis-devices" (@($devLines).Count -ge 1) "devices: $(if ($devLines) { ($devLines | Select-Object -Unique) -join '; ' } else { 'none (seat never bound)' })"
Assert "G2c-eis-keymap" ((Count $trace 'input: guest keymap layout') -ge 1) "keyboard device delivered a keymap"
Assert "G2d-abs-region-mapped" ((Count $trace 'input: EIS device [^\r\n]*mapping_id: Some') -ge 1) "an absolute pointer region carries a mapping id"
$teardown = Count $trace 'teardown: done'
$layout1 = Layout
$phys1 = EnabledPhysical $layout1
Assert "G7a-teardown-done" ($teardown -ge 1) "'teardown: done' x$teardown"
Assert "G7b-virtual-monitor-gone" ((Virtuals $layout1).Count -eq 0) "monitors after: $(Describe $layout1)"
Assert "G7c-physical-restored" (($phys1 -join ',') -eq ($phys0 -join ',')) "enabled physical before=[$($phys0 -join ',')] after=[$($phys1 -join ',')]"

# --- G8: reconnect churn --------------------------------------------------------
if (-not $SkipChurn) {
    $mark = Mark
    for ($i = 0; $i -lt 6; $i++) {
        & $probe $vmGuidStr $rdpGuid --seconds 2 2>&1 | Out-Null
    }
    Start-Sleep -Seconds ($GateWaitSec + 4)
    $churn = TraceSince $mark
    $layout2 = Layout
    $phys2 = EnabledPhysical $layout2
    $shellPid2 = ShellPid
    $lives = Count $churn 'session: live at'
    $downs = Count $churn 'teardown: done'
    Assert "G8a-shell-survives-churn" ($shellPid2 -eq $shellPid) "gnome-shell pid $shellPid -> $shellPid2"
    Assert "G8b-no-lease-refusal" ((Count $churn 'did not release in') -eq 0) "sessions live=$lives teardowns=$downs"
    Assert "G8c-every-session-torn-down" ($downs -ge $lives) "live=$lives teardown done=$downs"
    Assert "G8d-sessions-created" ($lives -ge 1) "live=$lives of 6 connects"
    Assert "G8e-layout-restored" ((Virtuals $layout2).Count -eq 0 -and (($phys2 -join ',') -eq ($phys0 -join ','))) "monitors: $(Describe $layout2)"
} else {
    $results.Add("[SKIP] G8-churn :: -SkipChurn set")
}

# --- G9: end-to-end compositor liveness -----------------------------------------
$shellPidEnd = ShellPid
Assert "G9a-same-gnome-shell" ($shellPidEnd -eq $shellPid) "gnome-shell pid $shellPid -> $shellPidEnd"
$cores = (Guest "coredumpctl --no-pager --no-legend list --since @$coreEpoch 2>/dev/null | grep -c gnome-shell").Trim() -as [int]
Assert "G9b-no-core-dumps" (($null -eq $cores) -or ($cores -eq 0)) "gnome-shell core dumps this run: $cores"
$unitEnd = (Guest 'systemctl --user is-active wayrdp 2>&1').Trim()
Assert "G9c-wayrdp-still-active" ($unitEnd -eq 'active') "wayrdp unit = $unitEnd"

Verdict
