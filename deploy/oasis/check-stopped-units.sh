#!/usr/bin/env bash
set -eu
for unit in "$@"; do
  rc=0
  properties=$(systemctl show -p LoadState -p ActiveState "$unit") || rc=$?
  load=$(sed -n 's/^LoadState=//p' <<< "$properties")
  active=$(sed -n 's/^ActiveState=//p' <<< "$properties")
  case "$rc/$load/$active" in
    0/loaded/inactive|0/loaded/failed|0/not-found/inactive|4/not-found/inactive) ;;
    *) printf 'Cannot verify stopped unit %s (rc=%s load=%s active=%s).\n' "$unit" "$rc" "$load" "$active" >&2; exit 1 ;;
  esac
done
