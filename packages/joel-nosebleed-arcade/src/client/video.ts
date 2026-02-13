export interface DecodedFrame {
  sequence: number;
  timestampUs: number;
  width: number;
  height: number;
  pitch: number;
  pixelFormat: number;
  bytes: Uint8Array;
}

export function decodeFramePacket(buffer: ArrayBuffer): DecodedFrame | null {
  const view = new DataView(buffer);
  if (view.byteLength < 37) {
    return null;
  }

  if (
    view.getUint8(0) !== 0x4e ||
    view.getUint8(1) !== 0x42 ||
    view.getUint8(2) !== 0x46 ||
    view.getUint8(3) !== 0x30
  ) {
    return null;
  }

  const sequence = readU64LittleEndian(view, 4);
  const timestampUs = readU64LittleEndian(view, 12);
  const width = view.getUint32(20, true);
  const height = view.getUint32(24, true);
  const pitch = view.getUint32(28, true);
  const pixelFormat = view.getUint8(32);
  const payloadLen = view.getUint32(33, true);

  if (37 + payloadLen > view.byteLength) {
    return null;
  }

  return {
    sequence,
    timestampUs,
    width,
    height,
    pitch,
    pixelFormat,
    bytes: new Uint8Array(buffer, 37, payloadLen)
  };
}

export function renderFrame(canvas: HTMLCanvasElement, frame: DecodedFrame): string {
  const ctx = canvas.getContext("2d", { alpha: false, desynchronized: true });
  if (!ctx) {
    return "2d context unavailable";
  }

  if (canvas.width !== frame.width || canvas.height !== frame.height) {
    canvas.width = frame.width;
    canvas.height = frame.height;
  }

  const rgba = new Uint8ClampedArray(frame.width * frame.height * 4);

  if (frame.pixelFormat === 0) {
    xrgb8888ToRgba(frame, rgba);
  } else if (frame.pixelFormat === 1) {
    rgb565ToRgba(frame, rgba);
  } else if (frame.pixelFormat === 2) {
    xrgb1555ToRgba(frame, rgba);
  } else {
    return `unknown pixel format ${frame.pixelFormat}`;
  }

  const imageData = new ImageData(rgba, frame.width, frame.height);
  ctx.putImageData(imageData, 0, 0);

  const ageMs = (Date.now() - Math.floor(frame.timestampUs / 1000)).toFixed(1);
  return `seq ${frame.sequence} | ${frame.width}x${frame.height} | age ${ageMs}ms`;
}

function xrgb8888ToRgba(frame: DecodedFrame, out: Uint8ClampedArray): void {
  const src = frame.bytes;
  let dstOffset = 0;
  for (let y = 0; y < frame.height; y += 1) {
    const row = y * frame.pitch;
    for (let x = 0; x < frame.width; x += 1) {
      const offset = row + x * 4;
      out[dstOffset] = src[offset + 2];
      out[dstOffset + 1] = src[offset + 1];
      out[dstOffset + 2] = src[offset];
      out[dstOffset + 3] = 255;
      dstOffset += 4;
    }
  }
}

function rgb565ToRgba(frame: DecodedFrame, out: Uint8ClampedArray): void {
  const src = frame.bytes;
  let dstOffset = 0;
  for (let y = 0; y < frame.height; y += 1) {
    const row = y * frame.pitch;
    for (let x = 0; x < frame.width; x += 1) {
      const offset = row + x * 2;
      const value = src[offset] | (src[offset + 1] << 8);
      const r = (value >> 11) & 0x1f;
      const g = (value >> 5) & 0x3f;
      const b = value & 0x1f;
      out[dstOffset] = (r * 255) / 31;
      out[dstOffset + 1] = (g * 255) / 63;
      out[dstOffset + 2] = (b * 255) / 31;
      out[dstOffset + 3] = 255;
      dstOffset += 4;
    }
  }
}

function xrgb1555ToRgba(frame: DecodedFrame, out: Uint8ClampedArray): void {
  const src = frame.bytes;
  let dstOffset = 0;
  for (let y = 0; y < frame.height; y += 1) {
    const row = y * frame.pitch;
    for (let x = 0; x < frame.width; x += 1) {
      const offset = row + x * 2;
      const value = src[offset] | (src[offset + 1] << 8);
      const r = (value >> 10) & 0x1f;
      const g = (value >> 5) & 0x1f;
      const b = value & 0x1f;
      out[dstOffset] = (r * 255) / 31;
      out[dstOffset + 1] = (g * 255) / 31;
      out[dstOffset + 2] = (b * 255) / 31;
      out[dstOffset + 3] = 255;
      dstOffset += 4;
    }
  }
}

function readU64LittleEndian(view: DataView, offset: number): number {
  if (typeof view.getBigUint64 === "function") {
    return Number(view.getBigUint64(offset, true));
  }
  return view.getUint32(offset + 4, true) * 4294967296 + view.getUint32(offset, true);
}
