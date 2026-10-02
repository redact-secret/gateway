#!/bin/sh
# Prove a candidate binary does not contain the qualification test build's seam (ADR 0020).
# usage: check-no-qualification-seam.sh <binary>
#
# The markers are built from fragments so this script never contains them whole (the
# repository-wide seam scan in tests/destination_policy.rs would otherwise flag it, and a
# literal here could never be told apart from a real hit). Exit 1 if any marker is present.
set -eu
bin="$1"
test -f "$bin"
a=RSG-QUALIFICATION; b=-BUILD-NOT-FOR-RELEASE
c=redact-secret-gateway; d=-qualification
e=--fake; f=-provider
g=fake; h=_provider
i=for_test; j=_http
k=Public; l=OrLoopback
bad=0
for marker in "$a$b" "$c$d" "$e$f" "$g$h" "$i$j" "${i}_https" "$k$l"; do
  if grep -a -F -q -- "$marker" "$bin"; then
    echo "FOUND a qualification-build marker in $(basename "$bin")" >&2
    bad=1
  fi
done
[ "$bad" -eq 0 ] || exit 1
echo "no qualification-build marker or seam symbol in $(basename "$bin") ($(wc -c <"$bin" | tr -d ' ') bytes scanned)"
