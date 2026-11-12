#!/usr/bin/env bash
# Deterministic device-side helper stand-in. It speaks the same JSON-lines
# protocol as the Java helper so the driver can be tested without a device.
set -euo pipefail

state="${FAKE_ADB_STATE:?FAKE_ADB_STATE is required}"
width="${1:?width}"
height="${2:?height}"
dpi="${3:?dpi}"
display_id="${FAKE_HELPER_DISPLAY_ID:-7}"

printf '%s' "$display_id" > "$state/display-id"
touch "$state/display-active"
cleanup() {
  rm -f "$state/display-active"
}
trap cleanup EXIT

if [[ "${FAKE_HELPER_FAIL_READY:-0}" == "1" ]]; then
  printf '{"id":0,"ok":false,"result":{"error":"fake helper failed to start"}}\n'
  exit 1
fi

own_focus=true
if [[ "${FAKE_HELPER_NO_FOCUS:-0}" == "1" ]]; then
  own_focus=false
fi

ready() {
  printf '{"id":0,"ok":true,"result":{"display_id":%s,"display_flags":64,"own_focus":%s,"width":%s,"height":%s,"dpi":%s,"pid":%s,"uid":2000}}\n' \
    "$display_id" "$own_focus" "$width" "$height" "$dpi" "$$"
}
info() {
  printf '{"id":%s,"ok":true,"result":{"display_id":%s,"display_flags":64,"own_focus":true,"width":%s,"height":%s,"dpi":%s,"pid":%s,"uid":2000}}\n' \
    "$1" "$display_id" "$width" "$height" "$dpi" "$$"
}
reply_error() {
  printf '{"id":%s,"ok":false,"result":{"error":"%s"}}\n' "$1" "$2"
}

ready
if [[ "${FAKE_HELPER_EXIT_AFTER_READY:-0}" == "1" ]]; then
  exit 1
fi

while IFS= read -r line; do
  printf '%s\n' "$line" >> "$state/helper-log"
  id=""
  if [[ "$line" =~ \"id\":[[:space:]]*([0-9]+) ]]; then
    id="${BASH_REMATCH[1]}"
  fi
  command=""
  if [[ "$line" =~ \"command\":[[:space:]]*\"([a-z_]+)\" ]]; then
    command="${BASH_REMATCH[1]}"
  fi
  if [[ -z "$id" || -z "$command" ]]; then
    printf '{"id":0,"ok":false,"result":{"error":"malformed request"}}\n'
    continue
  fi
  case "$command" in
    info)
      info "$id"
      ;;
    snapshot)
      touch "$state/snapshot-seen"
      if [[ "${FAKE_HELPER_SNAPSHOT_HANG:-0}" == "1" ]]; then
        continue
      fi
      if [[ "${FAKE_HELPER_SNAPSHOT_ERROR:-0}" == "1" ]]; then
        reply_error "$id" "No virtual display frame yet"
        continue
      fi
      encoded="$(base64 < "$state/snapshot.png" | tr -d '\n')"
      printf '{"id":%s,"ok":true,"result":{"width":%s,"height":%s,"timestamp":123456789,"png":"%s"}}\n' \
        "$id" "$width" "$height" "$encoded"
      ;;
    activity_info)
      user=""
      if [[ "$line" =~ \"user\":[[:space:]]*([0-9]+) ]]; then
        user="${BASH_REMATCH[1]}"
      fi
      component=""
      if [[ "$line" =~ \"component\":[[:space:]]*\"([^\"]+)\" ]]; then
        component="${BASH_REMATCH[1]}"
      fi
      if [[ "${FAKE_HELPER_ACTIVITY_ERROR:-0}" == "1" ]]; then
        reply_error "$id" "fake activity lookup failed"
        continue
      fi
      printf '{"id":%s,"ok":true,"result":{"user":%s,"component":"%s","launch_mode":%s,"task_affinity":"com.example.app","exported":true}}\n' \
        "$id" "${user:-0}" "$component" "${FAKE_HELPER_LAUNCH_MODE:-0}"
      ;;
    close)
      if [[ "${FAKE_HELPER_IGNORE_CLOSE:-0}" == "1" ]]; then
        # Refuse the close handshake and fail promptly without running the
        # EXIT trap. The display marker stays behind so the driver must surface
        # a cleanup failure and still attempt the jar removal/verification.
        trap - EXIT
        exit 1
      fi
      printf '{"id":%s,"ok":true,"result":{}}\n' "$id"
      exit 0
      ;;
    *)
      reply_error "$id" "unknown command"
      ;;
  esac
done