#!/bin/sh
# Optional operator profile ONLY. Runs before application/native sidecar startup.
# Requires root and NET_ADMIN; not part of the default Gateway installation.
set -eu
for tool in iptables ip6tables; do
  "$tool" -w 5 -N RSG_APP_EGRESS
  "$tool" -w 5 -A RSG_APP_EGRESS -d "$(if [ "$tool" = iptables ]; then echo 127.0.0.0/8; else echo ::1/128; fi)" -j RETURN
  "$tool" -w 5 -A RSG_APP_EGRESS -j REJECT
  "$tool" -w 5 -I OUTPUT 1 -m owner --uid-owner 10001 -j RSG_APP_EGRESS
done
# If either family fails, this init container fails and the app never starts.
echo 'owner egress rules installed for both families'
