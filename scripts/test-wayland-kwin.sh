#!/usr/bin/env bash
set -euo pipefail

for command in dbus-run-session kwin_wayland wl-copy wl-paste cargo; do
  if ! command -v "${command}" >/dev/null 2>&1; then
    echo "missing Wayland harness command: ${command}" >&2
    exit 77
  fi
done

runtime_dir=$(mktemp -d "${TMPDIR:-/tmp}/clip-bridge-kwin.XXXXXX")
chmod 700 "${runtime_dir}"
export XDG_RUNTIME_DIR="${runtime_dir}"
export WAYLAND_DISPLAY=clip-bridge-test

cleanup() {
  rm -rf -- "${runtime_dir}"
}
trap cleanup EXIT INT TERM

dbus-run-session -- bash -euo pipefail -c '
  kwin_wayland \
    --virtual \
    --no-lockscreen \
    --no-global-shortcuts \
    --socket "${WAYLAND_DISPLAY}" \
    >"${XDG_RUNTIME_DIR}/kwin.log" 2>&1 &
  kwin_pid=$!
  cleanup_kwin() {
    kill "${kwin_pid}" >/dev/null 2>&1 || true
    wait "${kwin_pid}" >/dev/null 2>&1 || true
  }
  trap cleanup_kwin EXIT INT TERM

  for _ in $(seq 1 100); do
    if [ -S "${XDG_RUNTIME_DIR}/${WAYLAND_DISPLAY}" ]; then
      cargo test --locked \
        backend::wayland::tests::kwin_ext_actor_receives_and_serves_both_selections \
        -- --ignored --exact --nocapture --test-threads=1
      exit
    fi
    if ! kill -0 "${kwin_pid}" >/dev/null 2>&1; then
      cat "${XDG_RUNTIME_DIR}/kwin.log" >&2
      exit 1
    fi
    sleep 0.1
  done

  echo "KWin did not create ${XDG_RUNTIME_DIR}/${WAYLAND_DISPLAY}" >&2
  cat "${XDG_RUNTIME_DIR}/kwin.log" >&2
  exit 1
'
