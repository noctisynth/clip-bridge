# Clip Bridge

Clip Bridge synchronizes non-empty UTF-8 plain text between X11/XWayland selections and a native
Wayland compositor. It is a small foreground daemon: it keeps no clipboard history, writes no
clipboard data to disk, and exposes no network service.

## Supported behavior

- Bidirectional X11 ↔ Wayland synchronization.
- Independent X11 `CLIPBOARD` and `PRIMARY` selections.
- Wayland `ext-data-control-v1`, with `wlr-data-control-unstable-v1` as a fallback.
- Wayland Primary when the selected provider supports it. ext enables Primary; wlr requires
  manager version 2 or newer. A wlr v1 session continues with Clipboard only.
- X11 `TARGETS` negotiation, UTF-8 targets, explicit `TEXT` handling, lossless ISO-8859-1
  `STRING`, and direct/INCR transfers.
- Wayland `text/plain;charset=utf-8` and `text/plain` offers with complete pipe reads and writes.
- A 16 MiB decoded payload limit, 5-second transfer idle timeout, and 30-second total timeout.

Images, HTML, RTF, files, application-specific MIME types, arbitrary binary content, clipboard
history, and backend reconnection are intentionally out of scope.

## Synchronization semantics

At startup, Clip Bridge inspects both protocol domains for each supported selection. If only one
side contains valid text, it fills the empty or unsupported side. Equal text causes no write. If
both sides contain different valid text, neither is overwritten; the next external ownership
change determines what is synchronized.

Empty, cleared, unsupported, invalid UTF-8, timed-out, and oversized selections never clear the
other backend. This avoids data loss during transient owner hand-offs. Rapid changes use bounded
channels and latest-command mailboxes; stale transfers and stale ownership results are discarded.

## Requirements

- The latest stable Rust toolchain for building from source. `rust-toolchain.toml` selects stable
  with Rustfmt and Clippy.
- An X11 or XWayland server with XFixes, available through `DISPLAY`.
- A Wayland compositor available through `WAYLAND_DISPLAY` and `XDG_RUNTIME_DIR`.
- Either `ext_data_control_manager_v1` or `zwlr_data_control_manager_v1` advertised by that
  compositor.
- Development libraries required by Wayland and X11 on the build machine.

The process exits if either required connection fails, XFixes is unavailable, or the compositor
does not advertise a supported data-control manager. Primary capability loss is non-fatal.

## Install

To install the current checkout:

```bash
cargo install --locked --path .
```

Or build a repository-local binary:

```bash
cargo build --release
```

After the `0.2.0` release is published, the same version can be installed from crates.io with
`cargo install clip-bridge`.

## Run

Start Clip Bridge inside the graphical desktop session:

```bash
clip-bridge
```

For a repository-local build, use `./target/release/clip-bridge`. The process has no command-line
options or configuration file and runs in the foreground until SIGINT or SIGTERM.

Both display environments must be visible to the process. A terminal opened by the desktop usually
already has the required values. Check them with:

```bash
printf 'DISPLAY=%s\nWAYLAND_DISPLAY=%s\nXDG_RUNTIME_DIR=%s\n' \
  "${DISPLAY:-}" "${WAYLAND_DISPLAY:-}" "${XDG_RUNTIME_DIR:-}"
```

If they are missing, export the values used by the current session before starting the bridge. For
example:

```bash
DISPLAY=:0 WAYLAND_DISPLAY=wayland-0 XDG_RUNTIME_DIR=/run/user/1000 clip-bridge
```

Do not copy the example values blindly: socket names and user IDs differ between sessions. X11 may
also require the session's `XAUTHORITY`. When launching from a systemd user unit, first confirm that
the user manager received the same variables:

```bash
systemctl --user show-environment | grep -E '^(DISPLAY|WAYLAND_DISPLAY|XDG_RUNTIME_DIR|XAUTHORITY)='
```

Clip Bridge stops both protocol actors and their bounded transfer workers before exiting. The first
backend connection failure ends the whole process; automatic reconnection is not implemented.

## Check synchronization

With Clip Bridge running, `xclip` and `wl-clipboard` can verify both directions:

```bash
# X11 Clipboard -> Wayland Clipboard
printf 'from X11\n' | xclip -selection clipboard -in
wl-paste

# Wayland Clipboard -> X11 Clipboard
printf 'from Wayland\n' | wl-copy
xclip -selection clipboard -out
```

If the selected Wayland provider supports Primary, verify it separately:

```bash
printf 'X11 Primary\n' | xclip -selection primary -in
wl-paste --primary

printf 'Wayland Primary\n' | wl-copy --primary
xclip -selection primary -out
```

A startup warning that Primary is unavailable is non-fatal; Clipboard synchronization continues.
If startup reports that no data-control provider exists, the compositor must advertise either
`ext_data_control_manager_v1` or `zwlr_data_control_manager_v1`. `wayland-info` can list the globals
available in the current session.

## Logging and privacy

The default log filter is `info`. Override it with standard `RUST_LOG` syntax:

```bash
RUST_LOG=clip_bridge=debug clip-bridge
```

Diagnostics include backend, selection, revision/command identifiers, MIME type, transfer stage,
and byte length where useful. Clipboard text itself is not logged or persisted. Rust strings,
protocol libraries, and display servers may still copy text in memory, so secure memory erasure is
not promised.

## Library API

The crate deliberately exposes only the runtime entry point and its typed error:

```rust
#[tokio::main]
async fn main() -> Result<(), clip_bridge::BridgeError> {
    clip_bridge::run().await
}
```

Backend connections, atoms, Wayland proxies, coordinator state, and transfer types are internal and
are not compatibility APIs. The same example is available as `cargo run --example run`.

## Verification

The default checks do not connect to the developer's current display session:

```bash
cargo fmt --all -- --check
cargo check --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
```

X11 wire tests start and clean up their own Xvfb instance and use `xclip` when available. In a
restricted sandbox they report an explicit skip; run the targeted suite in an environment that
permits local display sockets:

```bash
cargo test --locked backend::x11::tests::actor -- --nocapture --test-threads=1
```

The ext-data-control harness requires KWin, D-Bus, and wl-clipboard. It creates an isolated virtual
session and does not use the current desktop:

```bash
scripts/test-wayland-kwin.sh
```

The real wlr-data-control harness requires Niri, wayland-info, and wl-clipboard. It creates a nested
Niri session and changes selections only on that child Wayland socket. The test build forces the
wlr v2 path because production correctly prefers ext when Niri advertises both:

```bash
scripts/test-wayland-niri-wlr.sh
```

The default wlr v1/v2 protocol tests additionally use an in-process Wayland test server and Unix
socket pair. KWin 6.7.4, Niri 26.04, and wl-clipboard 2.3.0 are the currently recorded compositor
harness versions. See
[`docs/DESIGN.md`](docs/DESIGN.md) for the architecture, protocol contract, and complete acceptance
matrix.

## License

MIT. See [`LICENSE`](LICENSE).
