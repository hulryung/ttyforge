#!/usr/bin/env bash
#
# Run every acceptance script. Exit 0 only if each one passed or was skipped
# for missing tooling; any real mismatch fails the run.

cd "$(dirname "$0")"
failed=0 skipped=0 passed=0

for script in bridge-raw.sh bridge-rfc2217.sh mux-fanout.sh; do
  echo "=== $script"
  ./"$script"
  case $? in
    0)  passed=$((passed + 1)) ;;
    77) skipped=$((skipped + 1)) ;;
    *)  failed=$((failed + 1)) ;;
  esac
  echo
done

echo "acceptance: $passed passed, $skipped skipped, $failed failed"
[ "$failed" -eq 0 ]
