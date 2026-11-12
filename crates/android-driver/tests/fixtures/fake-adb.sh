#!/usr/bin/env bash
# Deterministic adb stand-in for offline integration tests. It records every
# invocation and serves canned device state; it never talks to a real device.
set -euo pipefail

state="${FAKE_ADB_STATE:?FAKE_ADB_STATE is required}"
mkdir -p "$state"
serial="${FAKE_ADB_SERIAL:-FAKE0001}"

line=""
for argument in "$@"; do
  line+="$argument"$'\t'
done
printf '%s\n' "$line" >> "$state/log"

server_host=""
server_port=""
selected_serial=""
args=()
while (($#)); do
  case "$1" in
    -H) server_host="$2"; shift 2 ;;
    -P) server_port="$2"; shift 2 ;;
    -s) selected_serial="$2"; shift 2 ;;
    *) args+=("$1"); shift ;;
  esac
done

if [[ -n "$selected_serial" && "$selected_serial" != "$serial" ]]; then
  printf 'error: device %s not found\n' "$selected_serial" >&2
  exit 1
fi

command="${args[0]:-}"
case "$command" in
  devices)
    printf 'List of devices attached\n'
    if [[ "${args[1]:-}" == "-l" ]]; then
      printf '%s\tdevice product:fake model:Fake_Phone device:fake transport_id:1\n' "$serial"
    fi
    ;;
  push)
    ;;
  shell)
    shell_args=("${args[@]:1}")
    operation="${shell_args[0]:-}"
    case "$operation" in
      getprop)
        printf '%s\n' "${FAKE_ADB_SDK:-34}"
        ;;
      wm)
        case "${shell_args[1]:-}" in
          size) printf 'Physical size: %sx%s\n' "${FAKE_ADB_WIDTH:-360}" "${FAKE_ADB_HEIGHT:-640}" ;;
          density) printf 'Physical density: %s\n' "${FAKE_ADB_DPI:-420}" ;;
          *) printf 'unknown wm command\n' >&2; exit 1 ;;
        esac
        ;;
      dumpsys)
        case "${shell_args[1]:-}" in
          window)
            cat "$state/window-policy"
            ;;
          activity)
            cat "$state/activities"
            if [[ -f "$state/display-active" ]]; then
              printf '\nDisplay #%s (activities from top to bottom):\n' "$(cat "$state/display-id")"
              if [[ -f "$state/virtual-activities" ]]; then
                cat "$state/virtual-activities"
              fi
            fi
            ;;
          *)
            printf 'unknown dumpsys target\n' >&2
            exit 1
            ;;
        esac
        ;;
      am)
        display=""
        user=""
        component=""
        index=1
        while ((index < ${#shell_args[@]})); do
          case "${shell_args[$index]}" in
            --display) display="${shell_args[$((index + 1))]}"; index=$((index + 2)) ;;
            --user) user="${shell_args[$((index + 1))]}"; index=$((index + 2)) ;;
            -n) component="${shell_args[$((index + 1))]}"; index=$((index + 2)) ;;
            *) index=$((index + 1)) ;;
          esac
        done
        if [[ -n "${FAKE_ADB_START_DELAY_MS:-}" ]]; then
          sleep "$(awk -v ms="$FAKE_ADB_START_DELAY_MS" 'BEGIN { printf "%.3f", ms / 1000 }')"
        fi
        if [[ "${FAKE_ADB_START_FAIL:-0}" == "1" ]]; then
          printf 'Error: Activity not started, unable to resolve Intent\n'
          exit 0
        fi
        printf 'Starting: Intent { cmp=%s }\n' "$component"
        printf '  mResumedActivity: ActivityRecord{fake1 u%s %s t99}\n' "$user" "$component" \
          > "$state/virtual-activities"
        ;;
      input)
        if [[ -n "${FAKE_ADB_INPUT_DELAY_MS:-}" ]]; then
          sleep "$(awk -v ms="$FAKE_ADB_INPUT_DELAY_MS" 'BEGIN { printf "%.3f", ms / 1000 }')"
        fi
        if [[ "${FAKE_ADB_INPUT_FAIL:-0}" == "1" ]]; then
          printf 'Error: Invalid arguments for input command\n'
        fi
        ;;
      rm)
        if [[ -n "${FAKE_ADB_CLEANUP_DELAY_MS:-}" ]]; then
          sleep "$(awk -v ms="$FAKE_ADB_CLEANUP_DELAY_MS" 'BEGIN { printf "%.3f", ms / 1000 }')"
        fi
        if [[ "${FAKE_ADB_RM_FAIL:-0}" == "1" ]]; then
          printf 'error: failed to remove %s\n' "${shell_args[2]:-}" >&2
          exit 1
        fi
        ;;
      env)
        app_process_index=""
        index=0
        while ((index < ${#shell_args[@]})); do
          if [[ "${shell_args[$index]}" == "app_process" ]]; then
            app_process_index=$index
            break
          fi
          index=$((index + 1))
        done
        if [[ -z "$app_process_index" ]]; then
          printf 'error: app_process missing\n' >&2
          exit 1
        fi
        width="${shell_args[$((app_process_index + 4))]}"
        height="${shell_args[$((app_process_index + 5))]}"
        dpi="${shell_args[$((app_process_index + 6))]}"
        exec bash "$(dirname "$0")/fake-helper.sh" "$width" "$height" "$dpi"
        ;;
      *)
        printf 'error: unsupported fake shell command %s\n' "$operation" >&2
        exit 1
        ;;
    esac
    ;;
  *)
    printf 'error: unsupported fake adb command %s\n' "$command" >&2
    exit 1
    ;;
esac