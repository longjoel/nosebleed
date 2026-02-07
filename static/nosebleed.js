/**
 * Minimal client helper for nosebleed websocket framebuffer stream.
 * Binary frame format: u16 width, u16 height, followed by RGBA8 pixel data.
 */
export function connectNosebleed({ canvas, url, statusEl, targetFps = 60 }) {
  if (!canvas) throw new Error("canvas required");
  const ctx = canvas.getContext("2d");
  let fbWidth = 1;
  let fbHeight = 1;

  function resize() {
    canvas.width = fbWidth;
    canvas.height = fbHeight;
    canvas.style.width = "100%";
    canvas.style.height = "100%";
  }

  const ws = new WebSocket(url);
  ws.binaryType = "arraybuffer";

  ws.onopen = () => status("live");
  ws.onclose = () => status("closed");
  ws.onerror = () => status("error");
  ws.onmessage = (event) => {
    const buf = new Uint8Array(event.data);
    if (buf.length < 4) return;
    fbWidth = buf[0] | (buf[1] << 8);
    fbHeight = buf[2] | (buf[3] << 8);
    const pixels = buf.subarray(4);
    if (pixels.length < fbWidth * fbHeight * 4) return;
    resize();
    const imgData = new ImageData(new Uint8ClampedArray(pixels), fbWidth, fbHeight);
    ctx.putImageData(imgData, 0, 0);
  };

  function status(text) {
    if (statusEl) statusEl.textContent = text;
  }

  return {
    close: () => ws.close(),
  };
}
