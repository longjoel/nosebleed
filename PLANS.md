# nosebleed next steps

## Immediate
- Increase framebuffer output path to websocket diffs (remove polling).
- Support larger resolutions by chunking PutImage payloads and PNG/WS streaming.
- Keep server alive for multiple clients; map window IDs to per-client state minimally.

## Short-term
- Add basic replies for GetGeometry, QueryExtension, ListExtensions to satisfy xlib clients.
- Implement CreateWindow/MapWindow tracking; simple clip/stack on root.
- Input bridge: browser -> pointer/keyboard -> X events.

## Stretch
- Add delta encoding + compression (zstd/deflate) for framebuffer.
- Begin EGL/GLX stub path with software GL (OSMesa) behind feature flag.
