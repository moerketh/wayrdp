#!/bin/bash
# compositor-stress.sh — reproduce wayrdp's headless-output retry storm
# directly against the compositor (harness A8).
#
# Background: wayrdp's 5s ticker retries create_headless_output() while the
# negotiated size has not landed (real-client DisplayControl fallback at
# 800x600). Each attempt runs: output create headless -> hl.monitor pin eval
# -> migrate_workspaces_to_output() eval. On Hyprland 0.56.2 that eval
# enumerates workspaces (hl.get_workspaces) while the compositor is mid
# reconfigure after the previous attempt's output removal; the workspace ->
# layout/keyboard chain can be torn (IKeyboard::getActiveLayoutIndex SEGV,
# crash reports 5437/3671 on wayrdp-validate). A green prober run never
# exercises this (it negotiates successfully on the first attempt), which is
# why the suite passed while real vmconnect killed the session.
#
# This script compresses the storm: N cycles of create -> pin -> migrate ->
# remove with short settle delays. It is NOT a fuzzer for Hyprland itself;
# it gates the contract wayrdp must satisfy: this exact call pattern must
# not crash the compositor.
set -u

NAME="wayrdp-stress"
CYCLES="${1:-8}"
SLEEP="0.4"

export HYPRLAND_INSTANCE_SIGNATURE
HYPRLAND_INSTANCE_SIGNATURE="$(ls -t /run/user/1000/hypr/ 2>/dev/null | head -1)"
[ -n "$HYPRLAND_INSTANCE_SIGNATURE" ] || { echo "no running instance"; exit 2; }

# Verbatim copy of migrate_workspaces_to_output()'s lua (crates/backend-
# wlroots/src/lib.rs): enumerate workspaces, move populated non-wayrdp ones
# onto the fresh output, focus it. pcall-guarded like the original.
migrate_lua() {
cat <<'LUA'
local me = "wayrdp-stress"
local last = nil
for _, ws in ipairs(hl.get_workspaces()) do
  if ws.monitor and ws.monitor.name ~= me
     and not ws.monitor.name:find("^wayrdp%-")
     and ws.windows > 0 then
    pcall(hl.dispatch, hl.dsp.workspace.move({ workspace = ws.id, monitor = me }))
    last = ws.id
  end
end
hl.dispatch(hl.dsp.focus({ monitor = me }))
if last then
  pcall(hl.dispatch, hl.dsp.exec_raw("workspace " .. last))
end
LUA
}

# Concurrency is the repro: wayrdp's sync_layout polls `hyprctl monitors -j`
# every 150ms for up to 10 tries AFTER each `output create headless`, while
# the mode-pin eval (hl.monitor) is still reconfiguring. monitors -j reads
# each monitor's active workspace inline; under churn that chain is torn and
# 0.56.2 SEGVs (getActiveLayoutIndex, crash 5437/3671). Replay BOTH: the
# storm above and a tight monitors -j poller overlapping it.
poll_pid=""
start_poller() {
    (
        end=$((SECONDS + CYCLES * 2))
        while [ $SECONDS -lt $end ]; do
            hyprctl monitors -j >/dev/null 2>&1
        done
    ) &
    poll_pid=$!
}
stop_poller() { [ -n "$poll_pid" ] && kill "$poll_pid" 2>/dev/null; wait "$poll_pid" 2>/dev/null; poll_pid=""; }

start_poller
trap stop_poller EXIT

for i in $(seq 1 "$CYCLES"); do
    hyprctl output create headless "$NAME" >/dev/null 2>&1
    hyprctl eval "hl.monitor({ output = \"$NAME\", mode = \"800x600@60\", position = \"0x0\", scale = 1 })" >/dev/null 2>&1
    # sync_layout equivalent: burst of monitors -j right after the pin
    for _ in 1 2 3 4 5; do hyprctl monitors -j >/dev/null 2>&1; done
    hyprctl eval "$(migrate_lua)" >/dev/null 2>&1
    for _ in 1 2 3 4 5; do hyprctl monitors -j >/dev/null 2>&1; done
    sleep "$SLEEP"
    hyprctl output remove "$NAME" >/dev/null 2>&1
    sleep "$SLEEP"
done
stop_poller
trap - EXIT

echo "stress cycles done: $CYCLES"
