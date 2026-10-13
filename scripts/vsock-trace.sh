#!/usr/bin/env bash
# vsock-trace.sh ? collect per-syscall latency for wayrdp's AF_VSOCK RDP channel.
#
# e11: the enhanced session runs over hv_sock (AF_VSOCK port 3389) instead of
# TCP. If frames still feel laggy after the encode-side ladder (e1-e10), the
# remaining suspects are on the send path: many small writes, kernel copy
# cost, or scheduling gaps between encode-done and write(). This script
# times each send/recv/read/write syscall on wayrdp while a session is live
# and prints a summary we can act on.
#
# Usage (on the guest, while an RDP session is connected and busy):
#   ./vsock-trace.sh                # 15 s capture, auto-detect wayrdp pid
#   ./vsock-trace.sh -d 30          # 30 s capture
#   ./vsock-trace.sh -p 1234        # explicit pid
#   ./vsock-trace.sh -c             # strace -c summary only (lowest overhead)
#
# Output: /tmp/vsock-trace-<ts>.log (raw) + an awk summary on stdout.
# strace inflates syscall cost ~2-4x; compare relative, not absolute, numbers.
set -euo pipefail

DURATION=15
PID=""
COUNT_ONLY=0
while getopts "d:p:c" opt; do
  case "$opt" in
    d) DURATION="$OPTARG" ;;
    p) PID="$OPTARG" ;;
    c) COUNT_ONLY=1 ;;
    *) echo "usage: $0 [-d seconds] [-p pid] [-c]" >&2; exit 2 ;;
  esac
done

command -v strace >/dev/null || { echo "strace missing: sudo pacman -S strace (or apt install strace)" >&2; exit 1; }

if [ -z "$PID" ]; then
  PID="$(pgrep -n -f wayrdp || true)"
  [ -n "$PID" ] || { echo "wayrdp not running" >&2; exit 1; }
fi
echo "tracing wayrdp pid $PID for ${DURATION}s"

# Snapshot vsock state first: which guest-side ports are connected, and their
# queue sizes, so the fd numbers in the trace can be tied to the RDP channel.
TS="$(date +%Y%m%d-%H%M%S)"
LOG="/tmp/vsock-trace-${TS}.log"
echo "== /proc/net/vsock before ==" | tee "$LOG"
cat /proc/net/vsock 2>/dev/null | tee -a "$LOG" || echo "(vmw_vsock not loaded?)" | tee -a "$LOG"

# Trace the whole process: FreeRDP does the socket I/O on its own reader and
# channel threads -f follows them. Timestamps (-tt, us) + duration (-T).
TRACE_EXPR="write,writev,send,sendto,sendmsg,read,readv,recv,recvfrom,recvmsg,poll,epoll_wait,ppoll"
if [ "$COUNT_ONLY" -eq 1 ]; then
  timeout -s INT "$DURATION" strace -f -c -e trace="$TRACE_EXPR" -p "$PID" -o "$LOG.stats" || true
  echo "== strace -c syscall summary ==" >> "$LOG"
  cat "$LOG.stats" >> "$LOG"
  cat "$LOG.stats"
else
  timeout -s INT "$DURATION" strace -f -tt -T -s 64 \
    -e trace="$TRACE_EXPR" -p "$PID" -o "$LOG.raw" || true
  cat "$LOG.raw" >> "$LOG"

  echo "== per-syscall latency summary (us: count total avg max) =="
  AWK_SUMM="$(dirname "$(readlink -f "$0")")/vsock-trace-summary.awk"
  awk -f "$AWK_SUMM" "$LOG.raw" | sort -t= -k5 -rn
fi

echo "== /proc/net/vsock after ==" | tee -a "$LOG"
cat /proc/net/vsock 2>/dev/null | tee -a "$LOG" >/dev/null || true
echo "raw log: $LOG"
