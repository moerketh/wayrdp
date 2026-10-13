# Per-syscall latency summarizer for strace -f -tt -T output.
# Input lines look like:
#   12345 10:00:01.123456 sendmsg(7, ...) = 1024 <0.001234>
# The trailing <seconds> is the syscall duration (strace -T); the leading
# fields are thread id and wall-clock timestamp (strace -f -tt).
# Prints one row per syscall: count, total/avg/max duration in microseconds.
/<[0-9.]+>$/ {
  dur = $NF; gsub(/[<>]/, "", dur)
  name = $0
  sub(/^[0-9]+ +[0-9:.]+ +/, "", name)
  sub(/\(.*/, "", name)
  n[name]++; t[name] += dur; us = dur * 1e6
  if (us > mx[name]) mx[name] = us
}
END {
  for (k in n)
    printf "%-12s n=%-7d total=%9.0fus avg=%9.1fus max=%10.0fus\n", k, n[k], t[k]*1e6, t[k]*1e6/n[k], mx[k]
}
