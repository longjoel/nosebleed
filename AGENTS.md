# nosebleed agent guide
- Goal: replace Xvfb with a Rust X11 server that renders off-screen and streams to browsers for interactive use.
- Milestones:
  1. Minimal X11 core: setup handshake, resource tables, root window, CreateWindow/MapWindow/DestroyWindow, PutImage/GetImage, CopyArea, PolyFillRectangle, event queue, error replies, input stubs.
  2. Transport: WebSocket server exposing framebuffer; send full frames first, then dirty-rect diffs; accept keyboard/mouse events and translate to X11 input events.
  3. Performance and compatibility: shared memory PutImage fast path, clipping/compositing, cursor support, optional zstd/deflate on framebuffer diffs.
  4. GL path: stub GLX that routes to OSMesa for software GL; expose a feature flag for future EGL/Wayland backend.
  5. UX: small HTTP UI with noVNC-compatible client; auth token option.
- Architecture decisions:
  - Crates: `nosebleed-proto` (X11 wire types + parser/serializer), `nosebleed-core` (server state, resources, raster), `nosebleed-web` (ws/http, diff encoder, input bridge), `nosebleed-bin` (CLI).
  - Framebuffer: ARGB8888 in a shared `Arc<Mutex<Vec<u8>>>`; dirty tracking via per-tile hashes to minimize diffs.
  - Input: map browser keycodes to X11 keysyms; pointer events with button state; synthesize repeats.
  - Error budget: prefer partial protocol coverage over strict completeness; log unimplemented opcodes; crash-free robustness.
- Working rules:
  - Keep changes small and testable; add an integration that runs a tiny X11 client drawing rectangles.
  - Default to ASCII; add concise comments only where non-obvious.
  - Do not regress the existing CLI flags (`--no-browser`, `--auto-display`, etc.) while new server is being wired.
