# vmconnect-harness.ps1 - self-asserting vmconnect-equivalent regression test
# for wayrdp.
#
# Faithfully emulates the vmconnect (Hyper-V Enhanced Session) wire path that
# real vmconnect uses, then ASSERTS the regression classes we have actually
# hit, so we never need a manual "is it black?" again.
#
# Wire path (faithful to vmconnect, via tools/vmconnect-probe.exe):
#   AF_HYPERV -> guest RDP service port 3389 -> TPKT + X.224 CR (no RDP_NEG_REQ)
#   -> raw MCS Connect-Initial (BER 0x7f65 + PER GCC CCrq) -> MCS Connect-Response
#   This is the exact point where generic clients (xfreerdp) stall forever
#   (they send X.224 RDP_NEG_REQ and wait for a nego reply the VmConnectMode
#   server never sends). The prober asserts each stage locally.
#
# Server-side regression assertions (over SSH, the source of truth):
#   A0  gate          - compositor healthy BEFORE any RDP test: process alive,
#                       cmdline has no --safe-mode, configerrors empty, IPC up,
#                       running hyprland.log has NO "attempting to generate"
#                       (autogen fallback) line, and a real hyprland.lua/conf
#                       exists. Without this every downstream assertion can
#                       pass against the compositor's error-overlay screen,
#                       which is exactly the regression the user reported as
#                       "suite is green but my vmconnect is black".
#   A1 connection     - prober completed the vmconnect handshake (S1..S3)
#   A2 single-peer    - this run produced exactly ONE peer_accepted .. exit
#                       cycle (guards the multi-peer race that froze the screen)
#   A3 activation     - the peer's gate reached activated=1 before exit
#   A4 shm capture    - journal recorded 'shm opened for <output>' AND the shm
#                       region exists with a valid wayrdp magic (else the pump
#                       silently retries forever => black client)
#   A5 no crash       - no NEW hyprlandCrashReport*.txt appeared this run AND
#                       Hyprland is NOT in --safe-mode now (the setKeyboardFocus
#                       UAF forced by RDP simulated motion; the regression the
#                       user hit twice)
#   A6 frames flowing - 'send_frame_gfx fid=' incremented while the prober held
#                       the session open (black/frozen detection)
#   A7 frames nonblack- shm pixel sampling DURING the hold shows >=5% non-black
#                       content (catches "connects but black" that A6 cannot,
#                       since a pump can advance seq on black frames). Does NOT
#                       catch safe-mode (its overlay is non-black) -> A0e/A0f.
#   A8 compositor     - compositor-stress.sh replays wayrdp's headless-output
#      stress         create/pin/migrate/remove pattern (the real-client
#                       DisplayControl-fallback retry storm; 0.56.2 SEGVs in
#                       getActiveLayoutIndex under it) and the original
#                       compositor pid must survive it. ON by default; -SkipStress
#                       to bypass while iterating. This is the class of failure
#                       that read GREEN (prober negotiates fine on attempt #1)
#                       while real vmconnect 800x600 fallback killed the session.
#   A8b client churn  - 6 rapid 2s prober connects (vs one clean 12s hold):
#                       reproduces the real-client connect/fail/disconnect
#                       cycle where wayrdp's `hyprctl devices -j` readiness
#                       poll raced virtual-keyboard teardown and SEGV'd the
#                       compositor (the exact "suite green, vmconnect black"
#                       incident class). Same-compositor-pid must survive.
#   A8c restart churn - SIGSEGV the compositor, then 6 rapid connects fired
#                       immediately after the wrapper's relaunch, while
#                       autostart.lua's plugin loader runs. This is the exact
#                       14:02 real-client loop: devicesRequest on a FRESH
#                       compositor null-deref'd a just-created vkbd whose
#                       keymap wasn't configured yet (frame: xkb_keymap_num_
#                       layouts inside getActiveLayoutIndex). The relaunched
#                       pid must survive unchanged with no extra crash dumps.
#   A9a-d e2e liveness- after the stress storm AND the prober session AND
#                       teardown, the ORIGINAL compositor is still alive: same
#                       pid + instance signature, no new crash reports, not
#                       safe-mode. A0*/A5 only prove liveness at run START /
#                       mid-session; the observed crash fired during teardown.
#   A0g-i environment - guest runtime can sustain the daemon: /tmp writable
#                       with >=100M free, and no leftover WLOG BinaryAppender
#                       pcap hoard. The failure class here is "suite green,
#                       compositor healthy, but wayrdp restart-loops because
#                       its stderr fd is unwritable" (full /tmp quota ->
#                       eprintln! panics in FFI callbacks). A0a-f can all pass
#                       in that state; without these the run reads as a
#                       mysterious A1 connection failure.
#
# Reachability: the harness refuses to run if SSH to the guest is down. A
# guest reboot (e.g. after a host vmms reset) reallocates the DHCP address, so
# a remembered -GuestIp goes stale; omit -GuestIp to re-resolve from the NIC
# each run instead of reusing a dead address.
#
# Usage: pwsh -File .\vmconnect-harness.ps1 [-VmName "wayrdp-validate_*"] ...
#   RUN WITH pwsh (PS7). Windows PowerShell 5.1 misreads this UTF-8 file.
#
# Exit code 0 = all assertions PASS; 1 = one or more ASSERTIONS FAILED.

[CmdletBinding()]
param(
    [string]$VmName      = "wayrdp-validate_*",
    [int]$HoldSec        = 12,                # how long the prober holds the session; must
                                              # outlast the post-activation DisplayControl
                                              # caps flood + wayrdp's ~1.9s-throttled drdynvc
                                              # caps retry (the dvc-caps request only reliably
                                              # reaches the prober after that settles)
    [int]$GateWaitSec    = 8,                 # how long to wait for gate/activation
    [string]$GuestUser   = "omarchy",
    [string]$GuestIp     = "",                # auto-resolve if empty
    [string]$SshKey      = "$env:USERPROFILE\.ssh\vmcreate_ed25519",
    [string]$VmId        = "",                # bypass Get-VM (which hangs when the host
                                              # vmms/WMI query wedges) by passing the VM GUID
    [switch]$SkipFrames,                      # skip A6 (frame streaming) when debugging
    [switch]$SkipStress                       # skip A8 (compositor output-create stress)
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

# Guest SSH helper. Defined before first use (the reachability pre-check below)
# because PowerShell does not hoist script-scope functions.
function Guest([string]$cmd) {
    & ssh -i $SshKey -o StrictHostKeyChecking=no -o ConnectTimeout=8 -o BatchMode=yes "$GuestUser@$GuestIp" $cmd 2>&1 | Out-String
}

# --- Resolve VM + service GUID -------------------------------------------
$rdpGuid = Get-ChildItem "HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Virtualization\GuestCommunicationServices" |
    Where-Object { (Get-ItemProperty $_.PsPath).ElementName -match 'RDP 3389' } |
    Select-Object -First 1 -ExpandProperty PSChildName
if (-not $rdpGuid) { Write-Error "No 'RDP 3389' guest service registered on this host"; exit 1 }
# Get-VM queries the Hyper-V WMI provider; on a wedged vmms this blocks
# indefinitely and silently stalls the whole run before the prober even
# starts. -VmId lets the caller supply the VM GUID directly (still the only
# thing Get-VM is used for) so the harness stays independent of WINMGMT.
$vmNameResolved = ""
if ($VmId) {
    $vmGuidStr = $VmId
    # We skipped Get-VM (wedged-vmms safety), so we have no display name.
    # IP auto-resolution below must key off -VmId, never $vm.Name (unset here).
    Write-Host "[harness] VM=$VmName (id override) ID=$vmGuidStr svc=$rdpGuid"
} else {
    $vm = Get-VM -Name $VmName -ErrorAction Stop | Sort-Object CreationTime -Descending | Select-Object -First 1
    if ($vm.State -ne 'Running') { Write-Error "VM '$($vm.Name)' is $($vm.State), need Running"; exit 1 }
    $vmGuidStr      = $vm.Id
    $vmNameResolved = $vm.Name
    Write-Host "[harness] VM=$($vm.Name) ID=$vmGuidStr svc=$rdpGuid"
}

# --- Resolve guest IP if not provided ------------------------------------
# Prefer the CURRENT DHCP lease over a remembered IP: a guest reboot (e.g.
# after a host vmms wedge reset) reallocates the address, and a stale IP
# either hangs SSH or - worse - points at nothing and silently zeroes every
# SSH assertion below. Re-derive from the NIC each run when not overridden.
if (-not $GuestIp) {
    $try = 0
    while (-not $GuestIp -and $try -lt 12) {
        # Get-VMNetworkAdapter by -VMId works even under the -VmId override;
        # key off $vmNameResolved (by name) only when we actually ran Get-VM.
        $nics = $null
        if ($VmId -and $vmGuidStr) { $nics = Get-VMNetworkAdapter -VMId $vmGuidStr -ErrorAction SilentlyContinue }
        elseif ($vmNameResolved)   { $nics = Get-VMNetworkAdapter -VMName $vmNameResolved -ErrorAction SilentlyContinue }
        if ($nics) { $GuestIp = ($nics.IPAddresses | Where-Object { $_ -match '^\d+\.\d+\.\d+\.\d+$' } | Select-Object -First 1) }
        if (-not $GuestIp) { Start-Sleep -Seconds 2; $try++ }
    }
    if ($GuestIp) { Write-Host "[harness] auto-resolved guest IP $GuestIp (current DHCP lease)" }
}
if (-not $GuestIp) { Write-Error "Could not resolve guest IP for SSH assertions (pass -GuestIp)"; exit 1 }

# --- Reachability pre-check (gap #2: stale/moved IP must FAIL loudly) ------
# A passed-cached -GuestIp that no longer answers means every SSH assertion
# would run against a dead/moved host and silently return empty (which reads
# as PASS for "not-empty" checks). Verify SSH reaches the box BEFORE any A*.
$probeAlive = Guest 'echo reachable'
if ($probeAlive.Trim() -notmatch 'reachable') {
    Write-Error "Guest $GuestUser@$GuestIp unreachable over SSH. The guest IP likely changed after a reboot; drop the stale -GuestIp (re-resolve via Get-VMNetworkAdapter) or correct it. probe output: $probeAlive"
    exit 1
}
Write-Host "[harness] guest=$GuestUser@$GuestIp (ssh reachable)"

# --- Build the prober -----------------------------------------------------
if (-not (Test-Path $probe) -or (Get-Item "$tools\vmconnect-probe.cs").LastWriteTime -gt (Get-Item $probe).LastWriteTime) {
    $csc = "$env:WINDIR\Microsoft.NET\Framework64\v4.0.30319\csc.exe"
    & $csc /nologo /out:$probe "$tools\vmconnect-probe.cs" | Out-Null
    if (-not (Test-Path $probe)) { Write-Error "vmconnect-probe build failed"; exit 1 }
    Write-Host "[harness] built vmconnect-probe.exe"
}

# --- Capture a guest-side baseline (before we connect) --------------------
$sigBefore   = (Guest 'ls -t /run/user/1000/hypr 2>/dev/null | head -1').Trim()
$crashBefore = (Guest 'ls /home/omarchy/.cache/hyprland/hyprlandCrashReport*.txt 2>/dev/null | wc -l').Trim() -as [int]
Write-Host "[harness] baseline: sig=$sigBefore crashReports=$crashBefore"

# --- A0: guest compositor health gate -------------------------------------
# A wedged/misconfigured Hyprland silently invalidates every downstream
# assertion (RDP connects "work" against a black safe-mode screen). This is
# the exact regression we hit and had to fix by hand; gate it so we never
# read a run as a wayrdp failure when the compositor was the broken part.
Assert "A0a-hyprland-running" ((Guest 'pgrep -x Hyprland' | Out-String).Trim() -ne '') "Hyprland process present"
$hyprArgs = (Guest 'ps -o args= -C Hyprland 2>/dev/null | head -1').Trim()
Assert "A0b-not-safe-mode" ($hyprArgs -notmatch 'safe-mode') "cmdline: $hyprArgs"
$cfgErr = (Guest 'S=$(ls -t /run/user/1000/hypr 2>/dev/null | head -1); HYPRLAND_INSTANCE_SIGNATURE=$S hyprctl configerrors 2>/dev/null').Trim()
Assert "A0c-no-config-errors" ([string]::IsNullOrWhiteSpace($cfgErr)) "configerrors: '$cfgErr'"
$ipc = (Guest 'S=$(ls -t /run/user/1000/hypr 2>/dev/null | head -1); HYPRLAND_INSTANCE_SIGNATURE=$S hyprctl monitors 2>/dev/null | head -1').Trim()
Assert "A0d-ipc-socket" ($ipc -match 'Monitor') "hyprctl monitors head: '$ipc'"

# A0e: compositor loaded the REAL config, not the autogenerated fallback.
#
# The failure this guards: on a guest boot where hyprland cannot find/parse
# the lua config, ConfigManager logs
#   "WARN ]: No config file found; attempting to generate."
# then auto-generates a default and shows the error overlay. '--safe-mode'
# normally accompanies it (A0b), but A0b alone is NOT sufficient:
#   * the autogen marker persists even if a build drops the flag, and
#   * safe-mode could be entered for another reason while a stale log still
#     shows an earlier boot's good load.
# 'configerrors' (A0c) is empty in this state because the generated config
# parses clean -- which is exactly why the suite read GREEN while the client
# showed the fallback screen. So check the log of the RUNNING instance (the
# newest /run/user/1000/hypr/<sig> dir) for the autogen line directly.
# Pattern is a single space-free word: embedded double quotes in the ssh
# command are stripped by PS 5.1's native-argument stringization, which would
# split the pattern into bogus file args and make grep -c print
# "path:0" (unparseable). Single-word 'attempting' is unique to that line.
$autogen = (Guest 'S=$(ls -t /run/user/1000/hypr 2>/dev/null | head -1); grep -c attempting "/run/user/1000/hypr/$S/hyprland.log" 2>/dev/null').Trim() -as [int]
if ($null -eq $autogen) { $autogen = -1 }
Assert "A0e-config-not-autogen" ($autogen -eq 0) "autogen-line count in running hyprland.log = $autogen (0 = real config loaded)"

# A0f: a real config source actually EXISTS. Paranoia over A0e: if the log is
# unavailable (autogen=-1) we already failed, but this also fails the run when
# neither hyprland.lua (Omarchy) nor hyprland.conf is present at all -- the
# precondition for the fallback A0e watches for.
$cfgPresent = (Guest '[ -f "$HOME/.config/hypr/hyprland.lua" ] || [ -f "$HOME/.config/hypr/hyprland.conf" ]; echo $?').Trim()
Assert "A0f-config-present" ($cfgPresent -eq '0') "hyprland.lua|conf present in ~/.config/hypr (exit=$cfgPresent)"

# A0g: guest runtime environment can sustain the daemon. The failure this
# guards (hit live 2026-10-13): WLOG_LEVEL=TRACE + WinPR BinaryAppender had
# filled the omarchy user's /tmp tmpfs quota with /tmp/wlog/<pid>.pcap dumps
# (1.6 GB). Once the quota was exhausted, EVERY /tmp write failed —
# including systemd's StandardError=append:/tmp/wayrdp-trace.log fd. wayrdp's
# eprintln! then panicked on its first stderr write; while that panic was
# trapped inside ffi_guard the daemon survived, but pre-fix builds aborted
# (panic_cannot_unwind) and restart-looped on EVERY connection while the
# compositor, config and IPC were all perfectly healthy — the suite's A0a-f
# would read GREEN with RDP completely dead. Two checks:
#   1. /tmp is writable by the guest user AND has >=100M free
#      (quota exhaustion surfaces as "Disk quota exceeded", a full tmpfs as
#      ENOSPC — either must fail the run, not the session).
#   2. No oversized wlog pcap dumps: any single /tmp/wlog/*.pcap >=300M, or
#      the dir total >=800M, means TRACE+BinaryAppender was left on.
$tmpOk = (Guest 'echo harness-probe > /tmp/.a0g-probe 2>&1 && rm -f /tmp/.a0g-probe && echo OK').Trim()
Assert "A0g-tmp-writable" ($tmpOk -match 'OK') "write probe: $tmpOk"
$tmpFree = (Guest "df -m --output=avail /tmp | tail -1 | tr -d ' '").Trim() -as [int]
if ($null -eq $tmpFree) { $tmpFree = 0 }
Assert "A0h-tmp-headroom" ($tmpFree -ge 100) "/tmp free = ${tmpFree}M (need >=100M)"
$wlogRaw = (Guest 'ls -l /tmp/wlog/*.pcap 2>/dev/null' | Out-String)
$wlogTotal = 0
$wlogOversize = 0
foreach ($m in [regex]::Matches(($wlogRaw -replace "`r",''), '[ \t]+(\d+)[ \t]+[^\s]+\n')) {
    $sz = [int64]$m.Groups[1].Value
    $wlogTotal += $sz
    if ($sz -ge 300MB) { $wlogOversize++ }
}
Assert "A0i-no-wlog-hoard" (($wlogOversize -eq 0) -and ($wlogTotal -lt 800MB)) "wlog pcap: oversize files=$wlogOversize total=$([math]::Round($wlogTotal/1MB))M"
if ($failed) { Write-Host "[harness] guest compositor/environment unhealthy - fix before RDP test"; foreach($r in $results){Write-Host "  $r"}; exit 1 }

# --- Guest-side trace file: app eprintln!/stderr (StandardError=append) ---
# A2/A3/A4/A6 MUST read this; the journal only carries WLOG lines once the
# 98-trace-file.conf drop-in redirects StandardError here. Read it fresh after
# the run; it accumulates the full session. (Older journal-only reads silently
# saw zero peer_accepted/gate/shm lines once the drop-in engaged.)
$GetTrace = { Guest 'cat /tmp/wayrdp-trace.log 2>/dev/null' }

# A0g/hyprland PID baseline + A8 storm script staging happen here (before A1)
# so the prober session AND the stress storm are both covered by the final
# liveness gate. The PID is captured fresh each run: a wrapper auto-restart
# (start-hyprland) respawns Hyprland with a new pid + new instance signature,
# and comparing against THIS baseline detects exactly that silent respawn.
$hyprPidBefore = (Guest 'pgrep -x Hyprland | head -1').Trim()
Write-Host "[harness] baseline: hyprPid=$hyprPidBefore"

# --- A8: compositor liveness under wayrdp's headless-output retry storm ---
# Gap this closes: the prober negotiates successfully on the first attempt, so
# a green run never exercises wayrdp's fallback path -- the real-client 800x600
# DisplayControl loop where the 5s ticker retries create_headless_output() +
# the workspace-migrate eval until the size lands. On Hyprland 0.56.2 that eval
# enumerates workspaces mid-reconfigure and hit getActiveLayoutIndex on a torn
# layout chain (crash reports 5437/3671, wayrdp-validate, compositor SIGSEGV ->
# real vmconnect black-screen + disconnect while this suite read PASS).
# compositor-stress.sh compresses that exact call pattern; if the compositor
# dies or respawns under it, A8/A9 fail. Skip via -SkipStress when iterating.
if (-not $SkipStress) {
    # CRLF-proof locally: on a Windows checkout git may hand us CRLF (\r
    # breaks bash). Strip Windows-side; a guest-side tr -d "\r" would arrive
    # with its quotes stripped under PS 5.1 and delete every letter 'r'.
    $csLf = Join-Path $env:TEMP 'wayrdp-compositor-stress.lf.sh'
    [IO.File]::WriteAllText($csLf, ((Get-Content -Raw "$tools\compositor-stress.sh") -replace "`r`n","`n"))
    & scp -i $SshKey -o StrictHostKeyChecking=no -o BatchMode=yes $csLf "${GuestUser}@${GuestIp}:/tmp/wayrdp-compositor-stress.sh" 2>&1 | Out-Null
    $stressOut = (Guest 'bash /tmp/wayrdp-compositor-stress.sh 8 2>&1')
    Write-Host $stressOut
    $hyprPidMid = (Guest 'pgrep -x Hyprland | head -1').Trim()
    $crashMid = (Guest 'ls /home/omarchy/.cache/hyprland/hyprlandCrashReport*.txt 2>/dev/null | wc -l').Trim() -as [int]
    Assert "A8-compositor-survives-storm" (($hyprPidMid -eq $hyprPidBefore) -and ($crashMid -le $crashBefore)) "hyprPid $hyprPidBefore->$hyprPidMid crashReports $crashBefore->$crashMid"
    # A8b: rapid client churn. The real failure (black screen + disconnects on
    # wayrdp-validate, crash reports 5437/3671/681 at 00:23) came from repeated
    # vmconnect connect->fail->disconnect attempts inside ~30s while wayrdp's
    # readiness gate polled `hyprctl devices -j` at 250ms: each connect creates
    # a zwp_virtual_keyboard_v1, each disconnect destroys it, and devicesRequest
    # iterating g_pInputManager->m_keyboards mid-teardown SEGV'd 0.56.2
    # (IKeyboard::getActiveLayoutIndex+0x10). The normal A1 prober holds a
    # single clean 12s session -- exactly one vkbd lifecycle, zero overlap, so
    # the suite read green while the compositor was dying. Repro: 6 rapid
    # 2s-hold connects; the ORIGINAL compositor must still be alive after.
    # The real client attempts that killed 00:23 REACHED activation (virtual
    # keyboard + output created, capture pumping) and then dropped abruptly.
    # Killing the prober pre-activation (before MCS channel join completes)
    # never creates the vkbd, so there is nothing to race -- 1.5s kills did
    # NOT reproduce. Kills must land AFTER activation (~4s in) so wayrdp
    # already created the vkbd and is mid-session when the socket tears.
    # Concurrently hammer `hyprctl devices -j` guest-side: wayrdp's readiness
    # gate polls it at 250ms on each connect, and devicesRequest iterating
    # m_keyboards across a vkbd destroy is the SEGV (IKeyboard::
    # getActiveLayoutIndex+0x10, crash 5437/3671).
    $churnJob = Start-Job -ScriptBlock {
        param($k,$ip) & ssh -i $k -o StrictHostKeyChecking=no -o BatchMode=yes "omarchy@$ip" 'S=$(ls -t /run/user/1000/hypr/ | head -1); end=$((SECONDS+40)); while [ $SECONDS -lt $end ]; do HYPRLAND_INSTANCE_SIGNATURE=$S hyprctl devices -j >/dev/null 2>&1; done' 2>&1 | Out-Null
    } -ArgumentList $SshKey,$GuestIp
    $churnFailed = $false
    $churnTraceBase = ((Guest 'wc -l < /tmp/wayrdp-trace.log 2>/dev/null').Trim() -as [int])
    for ($i = 1; $i -le 6 -and -not $churnFailed; $i++) {
        $proc = Start-Process -FilePath $probe -ArgumentList "$vmGuidStr $rdpGuid --seconds 10" -PassThru -WindowStyle Hidden
        Start-Sleep -Milliseconds 4000
        if (-not $proc.HasExited) { Stop-Process -Id $proc.Id -Force }
        Start-Sleep -Milliseconds 700   # gap between attempts, like the real client
        $alive = (Guest 'pgrep -x Hyprland | head -1').Trim()
        if ($alive -eq '') { $churnFailed = $true }
    }
    $null = Receive-Job $churnJob -Wait -AutoRemoveJob
    Start-Sleep -Seconds 3   # let any in-flight teardown race settle
    $hyprPidChurn = (Guest 'pgrep -x Hyprland | head -1').Trim()
    $crashChurn = (Guest 'ls /home/omarchy/.cache/hyprland/hyprlandCrashReport*.txt 2>/dev/null | wc -l').Trim() -as [int]
    Assert "A8b-compositor-survives-churn" (($hyprPidChurn -eq $hyprPidBefore) -and ($crashChurn -le $crashBefore) -and (-not $churnFailed)) "hyprPid $hyprPidBefore->$hyprPidChurn crashReports $crashBefore->$crashChurn midChurnDead=$churnFailed"

    # A8d: mid-activation kill OUTCOME gate. A Stop-Process from this script is
    # a clean process kill that lands wherever the prober's hold loop is
    # (post-activation), so the A8b churn above never exercised the wedge the
    # gdb backtrace proved: a client dying mid-activation leaves the vsock in
    # CLOSE_WAIT, CheckFileDescriptor (FreeRDP 3.32) reports healthy forever,
    # and the peer mainloop spins holding the pump lease. Here the prober
    # TerminateProcess()es itself right after MCS Connect-Response (stage 3,
    # --die-at-stage), so wayrdp sees an open vsock whose client is simply
    # gone, mid-activation.
    # Assert the OUTCOME, not the fix's log lines, so the gate tests the bug
    # on any build. The symptom the wedge produces is the *stuck pump*: on
    # main the killed peer's mainloop spins forever, so its capture pump
    # never unloads and never frees its /tmp socket / lease. On the fix the
    # EOF probe / activation deadline reaps it: teardown runs to "pump lease
    # released". (A raw CLOSE_WAIT socket count can't gate this: the kill is
    # over a vsock, `ss -tan` doesn't list those, and later churn peers leave
    # their own CLOSE_WAIT transiently — so it reads 1 even after a correct
    # teardown. The lease + a clean output list are the reliable signal.)
    $killOut = (& $probe $vmGuidStr $rdpGuid --seconds 10 --die-at-stage 3 2>&1 | Out-String)
    $killDied = ($killOut | Select-String -Pattern "DIE-AT-STAGE 3").Count -gt 0
    # Give the daemon a beat to notice the EOF and unwind the killed pump.
    Start-Sleep -Seconds 4
    $churnTrace = ((& $GetTrace) | Select-Object -Skip $churnTraceBase) -join "`n"
    $leaseBlocked   = ($churnTrace | Select-String -Pattern "couldn't take pump lease|previous capture pump still active").Count
    $leaseReleased  = ($churnTrace | Select-String -Pattern "teardown: pump lease released").Count
    # On a correct teardown, once the killed pumps all unwind (this gate plus
    # the churn above) at most the one live session's wayrdp-N output remains.
    # `ls -t | head -1` is racy here (Hyprland 0.56 SIGs and the stress
    # churns relaunches; /run keeps stale dirs), so find the live instance by
    # its answering .socket.sock.
    $monJson = Guest 'for d in $(ls -t /run/user/1000/hypr/); do o=$(HYPRLAND_INSTANCE_SIGNATURE=$d hyprctl monitors -j 2>/dev/null); if [ -n "$o" ]; then echo "$o"; break; fi; done'
    $wayrdpOuts = (($monJson | ConvertFrom-Json) | Where-Object { $_.name -like "wayrdp-*" }).Count
    Assert "A8d-mid-activation-kill-reaped" ($killDied -and ($leaseBlocked -eq 0) -and ($leaseReleased -gt 0) -and ($wayrdpOuts -le 1)) "killDied=$killDied leaseBlockLines=$leaseBlocked leaseReleased=$leaseReleased wayrdpOutputs=$wayrdpOuts"
    # A8c: restart churn — the exact 14:02 real-client crash loop. SIGSEGV the
    # compositor; the start-hyprland wrapper relaunches it, autostart.lua's
    # plugin loader runs during init, and the client's retries land while the
    # FRESH compositor's input stack is still settling. The pre-fix autostart
    # polled `hyprctl devices -j` at 250ms there, and devicesRequest null-
    # deref'd the just-created vkbd (keymap not yet configured: frame shows
    # xkb_keymap_num_layouts inside getActiveLayoutIndex). Post-fix autostart
    # must never call devices; the relaunched compositor must survive 6 rapid
    # post-activation-kill connects fired immediately after relaunch.
    $oldPid = (Guest 'pgrep -x Hyprland | head -1').Trim()
    Guest "kill -SEGV $oldPid" | Out-Null
    $newPid = ''
    for ($t = 0; $t -lt 25 -and ((-not $newPid) -or ($newPid -eq $oldPid)); $t++) {
        Start-Sleep -Seconds 1
        $newPid = (Guest 'pgrep -x Hyprland | head -1').Trim()
    }
    Assert "A8c0-wrapper-relaunch" (($newPid -ne '') -and ($newPid -ne $oldPid)) "pid $oldPid -> $newPid (wrapper must relaunch the compositor)"
    $crashAfterKill = (Guest 'ls /home/omarchy/.cache/hyprland/hyprlandCrashReport*.txt 2>/dev/null | wc -l').Trim() -as [int]
    $rnFailed = $false
    for ($i = 1; $i -le 6 -and -not $rnFailed; $i++) {
        $proc = Start-Process -FilePath $probe -ArgumentList "$vmGuidStr $rdpGuid --seconds 10" -PassThru -WindowStyle Hidden
        Start-Sleep -Milliseconds 4000
        if (-not $proc.HasExited) { Stop-Process -Id $proc.Id -Force }
        Start-Sleep -Milliseconds 700
        $alive = (Guest 'pgrep -x Hyprland | head -1').Trim()
        if (($alive -eq '') -or ($alive -ne $newPid)) { $rnFailed = $true }
    }
    Start-Sleep -Seconds 3
    $pidAfterChurn = (Guest 'pgrep -x Hyprland | head -1').Trim()
    $crashAfterChurn = (Guest 'ls /home/omarchy/.cache/hyprland/hyprlandCrashReport*.txt 2>/dev/null | wc -l').Trim() -as [int]
    Assert "A8c-restart-churn" ((-not $rnFailed) -and ($pidAfterChurn -eq $newPid) -and ($crashAfterChurn -le $crashAfterKill)) "relaunchPid $newPid->$pidAfterChurn crashReports $crashAfterKill->$crashAfterChurn (kill adds exactly one; any more = restart-race crash) midChurnDead=$rnFailed"
    # A8c intentionally killed the run-start compositor. Re-baseline so the
    # later A5/A9 identity checks compare against the relaunched instance
    # instead of failing on the pid the harness itself murdered.
    $hyprPidBefore = $pidAfterChurn
    $sigBefore     = (Guest 'ls -t /run/user/1000/hypr 2>/dev/null | head -1').Trim()
    $crashBefore   = $crashAfterChurn
} else {
    $results.Add("[SKIP] A8-compositor-survives-storm :: -SkipStress set (stress is ON by default; this closes the real-client retry-storm gap)")
}

# Baseline trace length AFTER the A8/A8b storm+churn, BEFORE A1's connect. The
# trace file is StandardError=append and survives across wayrdp restarts, so it
# accumulates prior runs' lines; A8b's six churned connects each write
# peer_accepted/exit too. A2 requires A1 to produce EXACTLY ONE pair, so count
# only what A1 appends from here.
$traceBaseLines = ((Guest 'wc -l < /tmp/wayrdp-trace.log 2>/dev/null').Trim() -as [int])
if ($null -eq $traceBaseLines) { $traceBaseLines = 0 }

# --- A1: run the prober (the vmconnect wire emulation) --------------------
# It holds the session open ~HoldSec so the peer activates and streams frames.
#
# A7 pixel-content sampling MUST overlap this hold: the shm region exists only
# while a peer is connected, so sampling after the prober exits finds nothing.
# Run the prober as a background job, sample the shm on a cadence while it
# holds, then join. (Start-Job so this stays single-script; no guest service.)
$samplerPy = Join-Path $tools "shm_pixsample.py"
$haveSampler = Test-Path $samplerPy
if ($haveSampler) {
    & scp -i $SshKey -o StrictHostKeyChecking=no -o BatchMode=yes "$samplerPy" "${GuestUser}@${GuestIp}:/tmp/wayrdp-pxsample.py" 2>&1 | Out-Null
}
$pxSamples = New-Object System.Collections.Generic.List[double]

# --- input sink (for A2b input-liveness assertion) --------------------------
# Spawn a focused 'foot' window running 'cat > /tmp/wayrdp-inp.log' so an
# injected keystroke lands there. Then the prober (with --send-input) types a
# known char and A2b greps the log for it: the FIRST live input assertion,
# which must pass before the persistent-input-thread refactor is judged
# non-regressing. Best-effort: if foot isn't present or the window can't be
# focused, A2b reports [NOTE] instead of failing (older hosts).
$inpMarker = 'z'   # char the prober injects via --send-input
Guest 'rm -f /tmp/wayrdp-inp.log 2>/dev/null' | Out-Null
# Guest-side waiter, launched detached: polls for the session's headless
# output (which only exists once the prober connects), then raises a focused
# foot window on it. The tty gets raw mode (canonical holds a lone key until
# Enter) and focus is applied + verified via the same hl.dispatch(...) call
# wayrdp itself uses, with a status file so A2b can tell "no key arrived"
# apart from "focus never landed".
$sinkWaiter = @'
S=$(for d in /run/user/1000/hypr/*/; do [ -S "${d}.socket.sock" ] && timeout 1 socat - "UNIX-CONNECT:${d}.socket.sock" </dev/null >/dev/null 2>&1 && { basename "$d"; break; }; done)
WD=$(ls -1 /run/user/1000/wayland-* 2>/dev/null | grep -v lock | head -1 | xargs -n1 basename)
export HYPRLAND_INSTANCE_SIGNATURE="$S" WAYLAND_DISPLAY="$WD" XDG_RUNTIME_DIR=/run/user/1000
rm -f /tmp/wayrdp-sink.status
for i in $(seq 1 100); do hyprctl monitors 2>/dev/null | grep -q "^Monitor wayrdp-" && break; sleep 0.25; done
# Raw-mode sink: canonical tty holds a lone key until Enter, which is exactly
# the false-negative that made this assertion look dead.
nohup foot --title wayrdp-inp-test sh -c 'stty -icanon -echo min 1; stdbuf -o0 cat > /tmp/wayrdp-inp.log' >/dev/null 2>&1 &
sleep 1.5
# 0.56.2 lua API: hl.dispatch executes a dispatcher expression result.
# wayrdp's own migrate_workspaces_to_output relies on the same call pattern
# (hl.dispatch(hl.dsp.focus(...))) and logs migration success on live sessions,
# so this is the verified path; a bare hl.dsp.focus(...) never dispatches.
hyprctl eval 'hl.dispatch(hl.dsp.focus({ window = "title:wayrdp-inp-test" }))' >/dev/null 2>&1
sleep 0.5
# Focus verify: A2b must know if focus failed instead of reading as input dead.
TITLE=$(hyprctl activewindow -j 2>/dev/null | python3 -c 'import json,sys; print(json.load(sys.stdin).get("title",""))' 2>/dev/null)
if [ "$TITLE" = "wayrdp-inp-test" ]; then echo sink-ok > /tmp/wayrdp-sink.status; else echo "focus-failed:$TITLE" > /tmp/wayrdp-sink.status; fi
# Stay alive until the keystroke lands (inotify on the log), not a fixed sleep:
# the harness drains the sink only after the gate-wait + journal greps, long
# enough that a fixed 20s window would expire and foot would die first.
timeout 60 inotifywait -q -e modify -e close_write /tmp/wayrdp-inp.log >/dev/null 2>&1 &
IPID=$!
# Backstop in case no key ever arrives: wait for the inotify watcher, else
# give up after its timeout and let the harness report the empty log.
wait $IPID 2>/dev/null
'@
$sinkWaiter = $sinkWaiter -replace "`r", ""   # strip CR -> LF for bash over ssh
$waiterTmp = Join-Path $env:TEMP "wayrdp-sinkwait.$PID.sh"
[IO.File]::WriteAllText($waiterTmp, $sinkWaiter)
& scp -i $SshKey -o StrictHostKeyChecking=no -o BatchMode=yes "$waiterTmp" "${GuestUser}@${GuestIp}:/tmp/wayrdp-sinkwait.sh" 2>&1 | Out-Null
Remove-Item $waiterTmp -Force -ErrorAction SilentlyContinue
# NB: pkill -f pattern uses a bracket so it can't match the invoking shell's
# own command line (which literally contains "wayrdp-inp-test") and kill the
# ssh session mid-command.
$sinkStatus = (Guest 'command -v foot >/dev/null 2>&1 || { echo no-foot; exit 0; }; pkill -f "wayrdp-inp-tes[t]" 2>/dev/null; rm -f /tmp/wayrdp-sink.status; nohup bash /tmp/wayrdp-sinkwait.sh >/tmp/wayrdp-sinkwait.out 2>&1 & echo sink-launched')
$sinkReady  = ($sinkStatus -match 'sink-launched')

# Build prober argv; PowerShell surfaces each ArgumentList item to $args inside
# the job, so the job splats $args[1..] (everything after the probe path).
$probeArgv = @($probe, $vmGuidStr, $rdpGuid, '--seconds', "$HoldSec")
if ($sinkReady) { $probeArgv += '--send-input' }
$probeJob = Start-Job -ScriptBlock {
    $p = $args[0]; $rest = $args[1..($args.Count - 1)]
    & $p @rest 2>&1 | Out-String
} -ArgumentList $probeArgv

if ($haveSampler) {
    # Poll until the prober job drains or HoldSec+grace elapses. The handshake
    # (~5-7s) plus the --seconds hold plus the dvc-caps 2.5s tail can run past
    # HoldSec+10 (e.g. HoldSec=12 -> ~19s+ prober run), which used to cut the
    # join before ALL_STAGES_PASS flushed. Give it ample headroom.
    $deadline = (Get-Date).AddSeconds($HoldSec + 25)
    while ((Get-Date) -lt $deadline -and $probeJob.State -eq 'Running') {
        $line = (Guest 'python3 /tmp/wayrdp-pxsample.py 2>/dev/null').Trim()
        if ($line -match '^OK ([\d.]+) (\d+) (\d+)') {
            $pxSamples.Add([double]$Matches[1])
        }
        Start-Sleep -Milliseconds 900
    }
}
$probeOut = (Receive-Job $probeJob -Wait -AutoRemoveJob)
$probeExit = 0; if ($probeOut -notmatch 'ALL_STAGES_PASS') { $probeExit = 1 }
Write-Host $probeOut
Assert "A1-connection" ($probeOut -match 'ALL_STAGES_PASS') "prober handshake output tail: $(($probeOut -split "`n")[-1])"

# Grab the A2b sink NOW, right after the prober --send-input burst, while the
# focused foot/cat window is still alive. Waiting until after the gate sleep +
# journal greps lets the waiter's window die first and the raw bytes are lost.
# A short settle first so the compositor delivers the key and cat's raw-mode
# read reaches the log before we snapshot it.
Start-Sleep -Milliseconds 800
$sinkStateEarly = ((Guest 'cat /tmp/wayrdp-sink.status 2>/dev/null') | Out-String).Trim()
$sinkLogEarly   = ((Guest 'cat /tmp/wayrdp-inp.log 2>/dev/null') | Out-String)

# Wait for the gate to tick at least once more post-hold.
Start-Sleep -Seconds $GateWaitSec

# --- Guest-side assertions over SSH ---------------------------------------
# wayrdp journal since just before the connect.
# peer_accepted / gate / shm / send_frame_gfx are app eprintln! -> StandardError
# -> /tmp/wayrdp-trace.log (98-trace-file.conf). Never grep the journal for these:
# it only has WLOG once the drop-in redirects app stderr, so journal reads here
# silently returned 0 lines and masked the real state.
$traceFull = (& $GetTrace)
# Keep only the lines appended during THIS run (skip the pre-connect baseline).
$traceLines = $traceFull -split "`r?`n"
$trace = if ($traceLines.Count -gt $traceBaseLines) {
    ($traceLines[$traceBaseLines..($traceLines.Count - 1)] -join "`n")
} else { "" }

# A2: single-peer. Count peer_accepted vs peer_mainloop-exit lines in window.
$accepts = ([regex]::Matches($trace, 'peer_accepted\] vsock peer')).Count
$exits   = ([regex]::Matches($trace, 'peer_mainloop\] exit')).Count
Assert "A2-single-peer" ($accepts -eq 1) "peer_accepted=$accepts exits=$exits (this run only; exactly one prober connect expected)"

# A3: activation. The gate line carries 'activated=1' once the peer connects.
$gate = [regex]::Matches($trace, 'peer_mainloop\] gate:.*') | ForEach-Object { $_.Value }
$gateLines = @($gate)
$gateCount = $gateLines.Count
$activated = @($gateLines | Where-Object { $_ -match 'activated=1' }) | Select-Object -Last 1
Assert "A3-activation" ($gateCount -ge 1 -and $null -ne $activated) "$gateCount gate lines; last activated: $activated"

# A4: shm capture region live.
$shmOpened = ([regex]::Matches($trace, 'shm opened for')).Count -gt 0
Assert "A4-shm-capture" $shmOpened "trace 'shm opened for' present=$shmOpened"

# A2b: input liveness — the prober (--send-input) injected a known scancode
# into the focused sink window; assert it landed. This is the gate that proves
# the input path is non-regressed by the persistent-input-thread refactor.
# The waiter's status file distinguishes "sink ran but no key arrived"
# (real failure) from "focus never landed / sink never launched" (harness
# artifact, reported as its own message instead of blaming input).
if ($sinkReady) {
    # Status/log were stashed right after the prober (above) while the window
    # was still alive; use those, not a late post-gate read.
    $sinkState = $sinkStateEarly
    if ($sinkState -match '^focus-failed') {
        Assert "A2b-input-liveness" $false "sink focus never landed ($sinkState); not a wayrdp failure, check focus eval"
    } elseif ($sinkState -ne 'sink-ok') {
        Assert "A2b-input-liveness" $false "sink waiter status='$sinkState' (expected sink-ok); window probably never spawned"
    } else {
        $sinkLog = $sinkLogEarly
        $hit = ($sinkLog -match [regex]::Escape($inpMarker))
        Assert "A2b-input-liveness" $hit "focused sink /tmp/wayrdp-inp.log contains '$inpMarker'? $hit; log='${sinkLog}'"
    }
    # cleanup: kill the sink window + log (bracket pattern: don't match this
    # command's own shell and die before the rm runs)
    Guest 'pkill -f "wayrdp-inp-tes[t]" 2>/dev/null; rm -f /tmp/wayrdp-inp.log /tmp/wayrdp-sink.status' | Out-Null
} else {
    Assert "A2b-input-liveness" $false "sink not ready on guest (status='$($sinkStatus.Trim())'); input assertion cannot pass without a sink"
}

# A5: no new crash + not in safe-mode.
$sigNow   = (Guest 'ls -t /run/user/1000/hypr 2>/dev/null | head -1').Trim()
$crashNow = (Guest 'ls /home/omarchy/.cache/hyprland/hyprlandCrashReport*.txt 2>/dev/null | wc -l').Trim() -as [int]
$safeMode = (Guest 'ps -o args= -C Hyprland 2>/dev/null | head -1').Trim() -match 'safe-mode'
Assert "A5-no-crash" (($crashNow -le $crashBefore) -and (-not $safeMode)) "crashReports $crashBefore->$crashNow safeMode=$safeMode"

# A6: frames flowing. The prober emulates VMConnect *basic* mode, which does
# not negotiate the Graphics Pipeline, so wayrdp takes the slow-path bitmap
# encoder (no rdpgfx, no send_frame_gfx fid= lines). Its breadcrumb is the
# periodic "[gfx-lat] pub->enc_ms ... over N" line, whose N is a cumulative
# frame counter. A live capture pump therefore shows N rising across lines.
# (EGFX fid-lines remain the check on a graphics-pipeline client; that path is
# not exercised by this basic-mode prober.)
if (-not $SkipFrames) {
    $counts = [regex]::Matches($trace, '\[gfx-lat\][^\n]*?over (\d+)') | ForEach-Object { [int]$_.Groups[1].Value }
    $framesOk = ($counts.Count -ge 2) -and (($counts | Measure-Object -Maximum).Maximum -gt ($counts | Measure-Object -Minimum).Minimum)
    if (-not $framesOk -and $counts.Count -eq 1) {
        # A single gfx-lat line still proves at least one frame was encoded and
        # published this run; treat >=1 slow-path frame as flowing.
        $framesOk = $counts[0] -ge 1
    }
    # Basic-mode prober never opens rdpgfx, so wayrdp stays on the slow path and
    # (post-flush fix) may emit no [gfx-lat] lines even while frames flow. In
    # that case defer to A7's pixel sampling: a non-black shm read during the
    # hold is definitive proof frames reached the pump. Don't double-count a
    # basic-mode breadcrumb absence as a stall.
    if (-not $framesOk -and $counts.Count -eq 0 -and $pxSamples.Count -gt 0 -and (($pxSamples | Measure-Object -Maximum).Maximum -gt 0.05)) {
        $framesOk = $true
        $results.Add("[NOTE] A6: no [gfx-lat] slow-path lines (expected in basic mode); frames proven by A7 pixel sampling")
    }
    $fidDetail = if ($counts.Count -gt 0) { "gfx-lat n=$($counts.Count) min-over=$(($counts|Measure-Object -Minimum).Minimum) max-over=$(($counts|Measure-Object -Maximum).Maximum)" } else { "no [gfx-lat] slow-path lines in window (deferred to A7)" }
    Assert "A6-frames-flowing" $framesOk $fidDetail

    # A7: frames have CONTENT (not all-black). A6 only proves the encoder ran;
    # a pump can publish black frames with advancing seq (ext-image-copy copy
    # failure, wrong bank, cleared surface). Sample the shm region DURING the
    # hold and require a measurable non-black pixel ratio. Threshold 5%: enough
    # to reject an all-black buffer, low enough to pass a dark desktop.
    # NOTE: this does NOT catch safe-mode (its fallback screen is non-black) --
    # that is A0e/A0f's job. A7 gates the "connects but black" class only.
    if ($pxSamples.Count -gt 0) {
        $best = ($pxSamples | Measure-Object -Maximum).Maximum
        Assert "A7-frames-nonblack" ($best -gt 0.05) "best non-black ratio=$([math]::Round($best,4)) over $($pxSamples.Count) samples"
    } else {
        # No samples: either the shm never appeared (caught by A4) or the
        # sampler file is absent. Report, don't fail, so a missing sampler
        # cannot mask an otherwise-good run.
        $results.Add("[SKIP] A7-frames-nonblack :: no pixel samples captured (shm absent or sampler missing)")
    }
} else {
    $results.Add("[SKIP] A6-frames-flowing :: -SkipFrames set")
    $results.Add("[SKIP] A7-frames-nonblack :: -SkipFrames set")
}

# --- A9: compositor liveness AT END OF RUN (post-session) -------------------
# The other half of the black-screen gap: A0* gates liveness at run START only.
# Crash reports written mid-session (during the prober hold, or seconds after
# teardown while wayrdp cleans up) were invisible to A5: its crash-count and
# safe-mode checks ran before the compositor died, and its pgrep in A0a only
# proves SOME Hyprland existed then. Re-verify now that everything this run
# did -- stress storm + full RDP session + teardown -- left the ORIGINAL
# compositor alive: same pid (no wrapper respawn), same instance signature,
# no new crash reports, not safe-mode.
$hyprPidAfter = (Guest 'pgrep -x Hyprland | head -1').Trim()
$sigAfter     = (Guest 'ls -t /run/user/1000/hypr 2>/dev/null | head -1').Trim()
$crashAfter   = (Guest 'ls /home/omarchy/.cache/hyprland/hyprlandCrashReport*.txt 2>/dev/null | wc -l').Trim() -as [int]
$safeModeNow  = (Guest 'ps -o args= -C Hyprland 2>/dev/null | head -1').Trim() -match 'safe-mode'
Assert "A9a-compositor-alive-e2e" ($hyprPidAfter -ne '') "hyprPid=$hyprPidAfter"
Assert "A9b-same-compositor-instance" (($hyprPidAfter -eq $hyprPidBefore) -and ($sigAfter -eq $sigBefore)) "pid $hyprPidBefore->$hyprPidAfter sig $sigBefore->$sigAfter"
Assert "A9c-no-crash-e2e" ($crashAfter -le $crashBefore) "crashReports $crashBefore->$crashAfter"
Assert "A9d-not-safe-mode-e2e" (-not $safeModeNow) "safeMode=$safeModeNow"

# --- A8d: persistent virtual input devices ---------------------------------
# Root-cause gate for the two Hyprland 0.56.x keyboard-churn SEGV classes
# (devicesRequest iterator / libinput key event on unguarded IKeyboard state):
# backend-wlroots now creates ONE virtual keyboard+pointer pair per compositor
# lifetime and stashes the wayland connection between RDP sessions instead of
# per-session create/destroy churn. Assert the invariant three ways:
#   a) the seat holds exactly one hl-virtual-keyboard-wayrdp while IDLE here
#      (no client connected) -- the stashed device survives between sessions;
#   b) this run's trace shows the stash/reuse cycle actually happening;
#   c) zero "virtual keyboard destroyed" lines this run (classic churn gone).
# Note: A8c's forced compositor relaunch legitimately wipes the stash once
# (connection dies with the compositor; next session reconnects fresh), so a
# reuse count is asserted, not an exact stash:session ratio.
# Rollback escape hatch: WAYRDP_DISABLE_PERSIST_INPUT=1 in wayrdp's systemd
# unit restores the per-session device model (and reintroduces the churn).
# The persistent-vkbd / stash-reuse hard gates assert a feature that was
# PARKED with feat/e1b-cursor-shape (single long-lived input thread). Current
# main uses honest per-session virtual devices, so they read false here and
# must not block the deploy gate. Report them as INFO so the signal stays
# visible; re-enable as hard gates once the persistent-input work lands.
$devicesJson = (Guest 'S=$(ls -t /run/user/1000/hypr/ | head -1); HYPRLAND_INSTANCE_SIGNATURE=$S hyprctl devices 2>/dev/null')
$vkbdCount   = ([regex]::Matches($devicesJson, 'hl-virtual-keyboard-wayrdp')).Count
$traceEnd   = (& $GetTrace) -split "`r?`n"
$traceDelta = if ($traceEnd.Count -gt $traceBaseLines) { ($traceEnd[$traceBaseLines..($traceEnd.Count - 1)] -join "`n") } else { "" }
$stashCnt   = ([regex]::Matches($traceDelta, 'input devices stashed')).Count
$reuseCnt   = ([regex]::Matches($traceDelta, 'reusing stashed wayland connection')).Count
$destroyCnt = ([regex]::Matches($traceDelta, 'virtual keyboard destroyed')).Count
$results.Add("[INFO] A8d-single-persistent-vkbd :: hl-virtual-keyboard-wayrdp count=$vkbdCount (idle, post-run; persistent-input feature parked, not gated on main)")
$results.Add("[INFO] A8d-stash-reuse-cycle :: stashed=$stashCnt reused=$reuseCnt this run (informational)")
# Real signal kept: any mid-run vkbd churn is worth seeing, but on main churn
# is expected (per-session devices), so this is also report-only until the
# persistent thread lands.
$results.Add("[INFO] A8d-vkbd-churn :: 'virtual keyboard destroyed' lines this run=$destroyCnt (informational)")

# --- Verdict ---------------------------------------------------------------
Write-Host ""
Write-Host "=== vmconnect-harness assertions ==="
foreach ($r in $results) { Write-Host "  $r" }
if ($failed) { Write-Host "[harness] FAIL"; exit 1 }
Write-Host "[harness] PASS"; exit 0
