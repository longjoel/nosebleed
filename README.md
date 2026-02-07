# nosebleed

`nosebleed` runs a GUI app in a virtual X11 display and optionally exposes it through a browser.

Current backend: X11 (`Xvfb` + `x11vnc` + `websockify` + browser noVNC client).

## Why

Like `xvfb-run`, but can be interactive and remotely visible in a web browser.

## Dependencies

Install these on your host:

- `Xvfb`
- `x11vnc`
- `websockify`

## Usage

```bash
cargo run -- -- retroarch
```

Pass options before `--`:

```bash
cargo run -- --display 99 --screen 1920x1080x24 --web-port 8080 --ws-port 6080 -- retroarch -f
```

Then open:

```text
http://127.0.0.1:8080
```

Run as plain xvfb-style wrapper (no browser stack):

```bash
cargo run -- --no-browser --auto-display -- your-gui-app --flag
```

## CLI

```text
nosebleed [OPTIONS] -- <COMMAND> [ARGS...]

Options:
  --display <N>     X display number (default: 99)
  --auto-display    Pick a free display number starting at --display
  --screen <WxHxD>  Virtual screen (default: 1280x720x24)
  --xvfb-arg <ARG>  Extra raw arg passed to Xvfb (repeatable)
  --vnc-port <N>    x11vnc TCP port (default: 5900)
  --ws-port <N>     websockify websocket port (default: 6080)
  --web-port <N>    HTTP UI port (default: 8080)
  --host <HOST>     Bind host for web + websocket (default: 127.0.0.1)
  --verbose         Show x11vnc/websockify output
  --no-browser      Disable browser streaming; run like xvfb wrapper
```

## Notes

- X11 backend only right now.
- Wayland/EGL headless support is a separate backend and can be added next.
