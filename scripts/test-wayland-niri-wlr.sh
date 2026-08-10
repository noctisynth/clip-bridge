#!/usr/bin/env bash
set -euo pipefail

for command in niri wayland-info wl-copy wl-paste cargo; do
  if ! command -v "${command}" >/dev/null 2>&1; then
    echo "missing Wayland harness command: ${command}" >&2
    exit 77
  fi
done

if [ -z "${XDG_RUNTIME_DIR:-}" ] || [ -z "${WAYLAND_DISPLAY:-}" ]; then
  echo "a parent Wayland session is required to start nested Niri" >&2
  exit 77
fi
if ! wayland-info >/dev/null 2>&1; then
  echo "unable to connect to the parent Wayland session" >&2
  exit 77
fi

if [[ "${WAYLAND_DISPLAY}" = /* ]]; then
  parent_socket="${WAYLAND_DISPLAY}"
else
  parent_socket="${XDG_RUNTIME_DIR}/${WAYLAND_DISPLAY}"
fi
runtime_dir=$(mktemp -d "${TMPDIR:-/tmp}/clip-bridge-niri.XXXXXX")
chmod 700 "${runtime_dir}"
export XDG_RUNTIME_DIR="${runtime_dir}"
export WAYLAND_DISPLAY="${parent_socket}"

cleanup() {
  rm -rf -- "${runtime_dir}"
}
trap cleanup EXIT INT TERM

niri -c /dev/null -- sleep 300 >"${runtime_dir}/niri.log" 2>&1 &
niri_pid=$!
cleanup_niri() {
  kill "${niri_pid}" >/dev/null 2>&1 || true
  wait "${niri_pid}" >/dev/null 2>&1 || true
}
trap 'cleanup_niri; cleanup' EXIT INT TERM

for _ in $(seq 1 100); do
  socket=$(find "${runtime_dir}" -maxdepth 1 -type s -name 'wayland-*' -printf '%f\n' -quit)
  if [ -n "${socket}" ]; then
    export WAYLAND_DISPLAY="${socket}"
    if ! globals=$(wayland-info 2>/dev/null); then
      echo "unable to query nested Niri globals" >&2
      exit 1
    fi
    if ! grep -q 'zwlr_data_control_manager_v1' <<<"${globals}"; then
      echo "nested Niri does not advertise zwlr_data_control_manager_v1" >&2
      exit 77
    fi
    cargo test --locked \
      backend::wayland::tests::wlr_session_actor_receives_and_serves_both_selections \
      -- --ignored --exact --nocapture --test-threads=1
    exit
  fi
  if ! kill -0 "${niri_pid}" >/dev/null 2>&1; then
    cat "${runtime_dir}/niri.log" >&2
    exit 1
  fi
  sleep 0.1
done

echo "Niri did not create a nested Wayland socket in ${runtime_dir}" >&2
cat "${runtime_dir}/niri.log" >&2
exit 1
