export type TransportMode = "auto" | "websocket" | "webrtc";

export interface InputButtons {
  [key: string]: boolean;
}

export interface InputAxes {
  [key: string]: number;
}

export interface InputState {
  buttons: InputButtons;
  axes: InputAxes;
}

export interface PlayerConfig {
  baseUrl: string;
  canvas: HTMLCanvasElement;
  token?: string;
  transport?: TransportMode;
  defaultPort?: number;
  autoReconnect?: boolean;
  reconnectDelayMs?: number;
  enableAudio?: boolean;
  wsPathVideo?: string;
  wsPathAudio?: string;
  wsPathInput?: string;
  webrtcPathSession?: string;
  onStatus?: (status: string) => void;
  onError?: (error: Error | string) => void;
  onTransport?: (transport: string) => void;
  onFrame?: (meta: FrameMeta) => void;
  onAck?: (sequence: number | null, serverTimeMs: number) => void;
}

export interface FrameMeta {
  sequence: number;
  width: number;
  height: number;
  timestampUs: number;
}

interface DecodedFramePacket {
  sequence: number;
  timestampUs: number;
  width: number;
  height: number;
  pitch: number;
  pixelFormat: number;
  bytes: Uint8Array;
}

interface DecodedAudioPacket {
  sampleRateHz: number;
  channels: number;
  frameCount: number;
  payloadLen: number;
  buffer: ArrayBuffer;
  pcmOffset: number;
}

interface DecodedRtcChunk {
  messageId: number;
  chunkIndex: number;
  totalChunks: number;
  payload: Uint8Array;
}

interface ChunkMapEntry {
  totalChunks: number;
  parts: Array<Uint8Array | null>;
  received: number;
  totalBytes: number;
  createdAt: number;
}

interface Vp8Packet {
  ptsUs: number;
  durationUs: number;
  keyframe: boolean;
  payload: Uint8Array;
}

const RTC_CHUNK_MAGIC = 0x3143424e;
const RTC_CHUNK_HEADER_LEN = 12;
const RTC_CHUNK_TTL_MS = 1400;
const VP8_VIDEO_MAGIC = 0x3156424e;
const VP8_VIDEO_HEADER_LEN = 21;

export class NosebleedPlayer {
  private readonly config: Required<
    Pick<
      PlayerConfig,
      | "transport"
      | "defaultPort"
      | "autoReconnect"
      | "reconnectDelayMs"
      | "enableAudio"
      | "wsPathVideo"
      | "wsPathAudio"
      | "wsPathInput"
      | "webrtcPathSession"
    >
  > &
    Omit<PlayerConfig, "transport" | "defaultPort" | "autoReconnect" | "reconnectDelayMs" | "enableAudio" | "wsPathVideo" | "wsPathAudio" | "wsPathInput" | "webrtcPathSession">;

  private readonly ctx: CanvasRenderingContext2D;
  private videoWs: WebSocket | null = null;
  private audioWs: WebSocket | null = null;
  private inputWs: WebSocket | null = null;
  private rtcPeer: RTCPeerConnection | null = null;
  private rtcVideoDc: RTCDataChannel | null = null;
  private rtcAudioDc: RTCDataChannel | null = null;
  private rtcInputDc: RTCDataChannel | null = null;
  private rtcReconnectTimer = 0;
  private reconnectTimer = 0;
  private stopped = true;

  private lastInputSequence = 0;
  private audioCtx: AudioContext | null = null;
  private audioGainNode: GainNode | null = null;
  private audioStartTime = 0;

  private rtcVideoDecoder: VideoDecoder | null = null;
  private rtcVideoDecoderReady = false;
  private rtcVp8DecodeSupported: boolean | null = null;

  private readonly rtcVideoChunkMap = new Map<number, ChunkMapEntry>();
  private readonly rtcAudioChunkMap = new Map<number, ChunkMapEntry>();

  constructor(config: PlayerConfig) {
    this.config = {
      ...config,
      transport: config.transport ?? "auto",
      defaultPort: config.defaultPort ?? 0,
      autoReconnect: config.autoReconnect ?? true,
      reconnectDelayMs: config.reconnectDelayMs ?? 800,
      enableAudio: config.enableAudio ?? false,
      wsPathVideo: config.wsPathVideo ?? "/ws/video",
      wsPathAudio: config.wsPathAudio ?? "/ws/audio",
      wsPathInput: config.wsPathInput ?? "/ws/input",
      webrtcPathSession: config.webrtcPathSession ?? "/webrtc/session"
    };

    const ctx = this.config.canvas.getContext("2d", { alpha: false, desynchronized: true });
    if (!ctx) {
      throw new Error("failed to get 2d canvas context");
    }
    this.ctx = ctx;
  }

  async connect(): Promise<void> {
    this.stopped = false;
    this.clearReconnectTimers();
    this.emitStatus("connecting");

    if (this.shouldUseWebRtc()) {
      try {
        await this.connectWebRtc();
        return;
      } catch (err) {
        if (this.config.transport === "auto") {
          this.emitStatus("webrtc failed, falling back to websocket");
          this.emitError(err as Error);
          this.connectWebSocket();
          return;
        }
        throw err;
      }
    }

    this.connectWebSocket();
  }

  disconnect(): void {
    this.stopped = true;
    this.clearReconnectTimers();
    this.closeWebSocket();
    this.closeWebRtcTransport();
    this.emitTransport("disconnected");
    this.emitStatus("disconnected");
  }

  async enableAudio(): Promise<void> {
    if (!this.audioCtx) {
      this.audioCtx = new AudioContext({ latencyHint: "interactive" });
      this.audioGainNode = this.audioCtx.createGain();
      this.audioGainNode.gain.value = 0.9;
      this.audioGainNode.connect(this.audioCtx.destination);
    }
    if (this.audioCtx.state !== "running") {
      await this.audioCtx.resume();
    }
  }

  sendInput(state: InputState, port = this.config.defaultPort): void {
    const payload = JSON.stringify({
      type: "input",
      port,
      sequence: ++this.lastInputSequence,
      buttons: state.buttons,
      axes: state.axes
    });

    if (this.rtcInputDc?.readyState === "open") {
      this.rtcInputDc.send(payload);
      return;
    }

    if (this.inputWs?.readyState === WebSocket.OPEN) {
      this.inputWs.send(payload);
    }
  }

  static kioskPreset(config: Omit<PlayerConfig, "transport" | "enableAudio" | "autoReconnect">): PlayerConfig {
    return {
      ...config,
      transport: "websocket",
      enableAudio: false,
      autoReconnect: true,
      reconnectDelayMs: 1000
    };
  }

  static dedicatedPreset(config: Omit<PlayerConfig, "transport" | "enableAudio" | "autoReconnect">): PlayerConfig {
    return {
      ...config,
      transport: "auto",
      enableAudio: true,
      autoReconnect: true,
      reconnectDelayMs: 800
    };
  }

  private shouldUseWebRtc(): boolean {
    if (this.config.transport === "webrtc") {
      return typeof RTCPeerConnection !== "undefined";
    }
    if (this.config.transport === "websocket") {
      return false;
    }
    return typeof RTCPeerConnection !== "undefined";
  }

  private endpoint(path: string, kind: "ws" | "http"): string {
    const base = new URL(this.config.baseUrl);
    const url = new URL(path, base);
    if (kind === "ws") {
      if (url.protocol === "https:") {
        url.protocol = "wss:";
      } else if (url.protocol === "http:") {
        url.protocol = "ws:";
      }
    }
    if (this.config.token) {
      url.searchParams.set("token", this.config.token);
    }
    return url.toString();
  }

  private connectWebSocket(): void {
    this.closeWebSocket();

    const videoWs = new WebSocket(this.endpoint(this.config.wsPathVideo, "ws"));
    videoWs.binaryType = "arraybuffer";
    this.videoWs = videoWs;

    const inputWs = new WebSocket(this.endpoint(this.config.wsPathInput, "ws"));
    this.inputWs = inputWs;

    let audioWs: WebSocket | null = null;
    if (this.config.enableAudio) {
      audioWs = new WebSocket(this.endpoint(this.config.wsPathAudio, "ws"));
      audioWs.binaryType = "arraybuffer";
      this.audioWs = audioWs;
    }

    videoWs.onopen = () => {
      this.emitTransport("websocket-video-open");
      this.emitStatus("video connected");
    };
    videoWs.onmessage = (event) => {
      if (!(event.data instanceof ArrayBuffer)) {
        return;
      }
      const frame = decodeFramePacket(event.data);
      if (!frame) {
        return;
      }
      this.renderFrame(frame);
      this.config.onFrame?.({
        sequence: frame.sequence,
        width: frame.width,
        height: frame.height,
        timestampUs: frame.timestampUs
      });
    };
    videoWs.onclose = () => this.scheduleReconnect("video closed");
    videoWs.onerror = () => this.emitError("video websocket transport error");

    inputWs.onopen = () => {
      this.emitTransport("websocket-input-open");
      this.emitStatus("input connected");
    };
    inputWs.onmessage = (event) => {
      if (typeof event.data === "string") {
        this.handleInputServerText(event.data);
      }
    };
    inputWs.onclose = () => this.scheduleReconnect("input closed");
    inputWs.onerror = () => this.emitError("input websocket transport error");

    if (audioWs) {
      audioWs.onopen = () => this.emitTransport("websocket-audio-open");
      audioWs.onmessage = (event) => {
        if (!(event.data instanceof ArrayBuffer)) {
          return;
        }
        const packet = decodeAudioPacket(event.data);
        if (!packet) {
          return;
        }
        this.enqueueAudioPacket(packet);
      };
      audioWs.onclose = () => this.scheduleReconnect("audio closed");
      audioWs.onerror = () => this.emitError("audio websocket transport error");
    }
  }

  private closeWebSocket(): void {
    this.videoWs?.close();
    this.inputWs?.close();
    this.audioWs?.close();
    this.videoWs = null;
    this.inputWs = null;
    this.audioWs = null;
  }

  private async connectWebRtc(): Promise<void> {
    this.closeWebRtcTransport();
    this.emitStatus("webrtc connecting");

    const peer = new RTCPeerConnection({
      iceServers: [{ urls: "stun:stun.l.google.com:19302" }]
    });
    this.rtcPeer = peer;

    const videoChannel = peer.createDataChannel("video", { ordered: false, maxRetransmits: 0 });
    videoChannel.binaryType = "arraybuffer";
    this.rtcVideoDc = videoChannel;

    const inputChannel = peer.createDataChannel("input", { ordered: false, maxRetransmits: 0 });
    this.rtcInputDc = inputChannel;

    const audioChannel = peer.createDataChannel("audio", { ordered: true, maxRetransmits: 2 });
    audioChannel.binaryType = "arraybuffer";
    this.rtcAudioDc = audioChannel;

    peer.onconnectionstatechange = () => {
      if (this.rtcPeer !== peer) {
        return;
      }
      if (peer.connectionState === "connected") {
        this.emitTransport("webrtc-open");
        this.emitStatus("connected (webrtc)");
      } else if (
        peer.connectionState === "failed" ||
        peer.connectionState === "disconnected" ||
        peer.connectionState === "closed"
      ) {
        this.scheduleReconnect("webrtc disconnected");
      }
    };

    videoChannel.onmessage = (event) => {
      const payload = this.decodeRtcPayload(this.rtcVideoChunkMap, event.data);
      if (!payload) {
        return;
      }
      const vp8 = decodeVp8VideoPacket(payload);
      if (vp8 && this.decodeVp8WebCodecFrame(vp8)) {
        return;
      }
      const frame = decodeFramePacket(payload);
      if (!frame) {
        return;
      }
      this.renderFrame(frame);
      this.config.onFrame?.({
        sequence: frame.sequence,
        width: frame.width,
        height: frame.height,
        timestampUs: frame.timestampUs
      });
    };
    videoChannel.onerror = () => this.emitError("webrtc video error");

    audioChannel.onmessage = (event) => {
      const payload = this.decodeRtcPayload(this.rtcAudioChunkMap, event.data);
      if (!payload) {
        return;
      }
      const packet = decodeAudioPacket(payload);
      if (!packet) {
        return;
      }
      this.enqueueAudioPacket(packet);
    };

    inputChannel.onmessage = (event) => {
      if (typeof event.data === "string") {
        this.handleInputServerText(event.data);
      } else if (event.data instanceof ArrayBuffer) {
        this.handleInputServerText(new TextDecoder().decode(new Uint8Array(event.data)));
      }
    };

    const offer = await peer.createOffer();
    await peer.setLocalDescription(offer);
    await waitForIceGatheringComplete(peer);
    if (!peer.localDescription) {
      throw new Error("missing local description");
    }

    const answer = await this.postWebRtcOffer(peer.localDescription);
    await peer.setRemoteDescription(answer);
  }

  private closeWebRtcTransport(): void {
    this.rtcVideoDc?.close();
    this.rtcAudioDc?.close();
    this.rtcInputDc?.close();
    this.rtcVideoDc = null;
    this.rtcAudioDc = null;
    this.rtcInputDc = null;

    this.rtcPeer?.close();
    this.rtcPeer = null;

    this.rtcVideoDecoder?.close();
    this.rtcVideoDecoder = null;
    this.rtcVideoDecoderReady = false;

    this.rtcVideoChunkMap.clear();
    this.rtcAudioChunkMap.clear();
  }

  private async postWebRtcOffer(localDescription: RTCSessionDescription): Promise<RTCSessionDescriptionInit> {
    const requestedVideoMode = this.probeVp8WebCodecDecode() ? "vp8" : "raw";
    const response = await fetch(this.endpoint(this.config.webrtcPathSession, "http"), {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({
        type: localDescription.type,
        sdp: localDescription.sdp,
        video_mode: requestedVideoMode
      })
    });

    if (!response.ok) {
      throw new Error(`webrtc signaling failed: ${response.status} ${await response.text()}`);
    }
    const answer = (await response.json()) as RTCSessionDescriptionInit;
    if (!answer || answer.type !== "answer" || typeof answer.sdp !== "string") {
      throw new Error("invalid webrtc signaling response");
    }
    return answer;
  }

  private scheduleReconnect(reason: string): void {
    this.emitStatus(reason);
    if (this.stopped || !this.config.autoReconnect) {
      return;
    }
    if (this.reconnectTimer || this.rtcReconnectTimer) {
      return;
    }

    if (this.shouldUseWebRtc()) {
      this.rtcReconnectTimer = window.setTimeout(() => {
        this.rtcReconnectTimer = 0;
        this.connect().catch((err) => this.emitError(err as Error));
      }, this.config.reconnectDelayMs);
      return;
    }

    this.reconnectTimer = window.setTimeout(() => {
      this.reconnectTimer = 0;
      this.connectWebSocket();
    }, this.config.reconnectDelayMs);
  }

  private clearReconnectTimers(): void {
    if (this.reconnectTimer) {
      window.clearTimeout(this.reconnectTimer);
      this.reconnectTimer = 0;
    }
    if (this.rtcReconnectTimer) {
      window.clearTimeout(this.rtcReconnectTimer);
      this.rtcReconnectTimer = 0;
    }
  }

  private handleInputServerText(text: string): void {
    try {
      const payload = JSON.parse(text) as { type?: string; sequence?: number; server_time_ms?: number; message?: string };
      if (payload.type === "ack" && typeof payload.server_time_ms === "number") {
        this.config.onAck?.(typeof payload.sequence === "number" ? payload.sequence : null, payload.server_time_ms);
      } else if (payload.type === "error" && payload.message) {
        this.emitError(payload.message);
      }
    } catch {
      // Ignore malformed server text.
    }
  }

  private emitStatus(status: string): void {
    this.config.onStatus?.(status);
  }

  private emitTransport(transport: string): void {
    this.config.onTransport?.(transport);
  }

  private emitError(error: Error | string): void {
    this.config.onError?.(error);
  }

  private decodeRtcPayload(map: Map<number, ChunkMapEntry>, eventData: unknown): ArrayBuffer | null {
    if (!(eventData instanceof ArrayBuffer)) {
      return null;
    }

    const chunk = decodeRtcChunk(eventData);
    if (!chunk) {
      return null;
    }

    this.pruneRtcChunkMap(map);
    let entry = map.get(chunk.messageId);
    if (!entry) {
      entry = {
        totalChunks: chunk.totalChunks,
        parts: new Array(chunk.totalChunks).fill(null),
        received: 0,
        totalBytes: 0,
        createdAt: performance.now()
      };
      map.set(chunk.messageId, entry);
    }

    if (entry.totalChunks !== chunk.totalChunks) {
      map.delete(chunk.messageId);
      return null;
    }

    if (!entry.parts[chunk.chunkIndex]) {
      entry.parts[chunk.chunkIndex] = chunk.payload;
      entry.received += 1;
      entry.totalBytes += chunk.payload.length;
    }

    if (entry.received !== entry.totalChunks) {
      return null;
    }

    const out = new Uint8Array(entry.totalBytes);
    let offset = 0;
    for (const part of entry.parts) {
      if (!part) {
        map.delete(chunk.messageId);
        return null;
      }
      out.set(part, offset);
      offset += part.length;
    }

    map.delete(chunk.messageId);
    return out.buffer;
  }

  private pruneRtcChunkMap(map: Map<number, ChunkMapEntry>): void {
    const now = performance.now();
    for (const [messageId, entry] of map.entries()) {
      if (now - entry.createdAt > RTC_CHUNK_TTL_MS) {
        map.delete(messageId);
      }
    }
  }

  private probeVp8WebCodecDecode(): boolean {
    if (this.rtcVp8DecodeSupported !== null) {
      return this.rtcVp8DecodeSupported;
    }
    if (typeof VideoDecoder === "undefined" || typeof EncodedVideoChunk === "undefined") {
      this.rtcVp8DecodeSupported = false;
      return false;
    }

    try {
      const decoder = new VideoDecoder({ output: (frame) => frame.close(), error: () => {} });
      decoder.configure({ codec: "vp8", optimizeForLatency: true, hardwareAcceleration: "prefer-hardware" });
      decoder.close();
      this.rtcVp8DecodeSupported = true;
    } catch {
      this.rtcVp8DecodeSupported = false;
    }

    return this.rtcVp8DecodeSupported;
  }

  private ensureRtcVideoDecoder(): boolean {
    if (!this.probeVp8WebCodecDecode()) {
      return false;
    }
    if (this.rtcVideoDecoderReady && this.rtcVideoDecoder) {
      return true;
    }

    try {
      this.rtcVideoDecoder = new VideoDecoder({
        output: (frame) => {
          if (this.config.canvas.width !== frame.displayWidth || this.config.canvas.height !== frame.displayHeight) {
            this.config.canvas.width = frame.displayWidth;
            this.config.canvas.height = frame.displayHeight;
          }
          this.ctx.drawImage(frame, 0, 0, this.config.canvas.width, this.config.canvas.height);
          frame.close();
        },
        error: () => {
          this.rtcVideoDecoderReady = false;
        }
      });
      this.rtcVideoDecoder.configure({ codec: "vp8", optimizeForLatency: true, hardwareAcceleration: "prefer-hardware" });
      this.rtcVideoDecoderReady = true;
      return true;
    } catch {
      this.rtcVideoDecoder = null;
      this.rtcVideoDecoderReady = false;
      return false;
    }
  }

  private decodeVp8WebCodecFrame(packet: Vp8Packet): boolean {
    if (!this.ensureRtcVideoDecoder() || !this.rtcVideoDecoder) {
      return false;
    }

    try {
      const chunk = new EncodedVideoChunk({
        type: packet.keyframe ? "key" : "delta",
        timestamp: packet.ptsUs,
        duration: packet.durationUs,
        data: packet.payload
      });
      this.rtcVideoDecoder.decode(chunk);
      return true;
    } catch {
      this.rtcVideoDecoderReady = false;
      return false;
    }
  }

  private renderFrame(frame: DecodedFramePacket): void {
    if (this.config.canvas.width !== frame.width || this.config.canvas.height !== frame.height) {
      this.config.canvas.width = frame.width;
      this.config.canvas.height = frame.height;
    }

    const rgba = new Uint8ClampedArray(frame.width * frame.height * 4);
    if (frame.pixelFormat === 0) {
      xrgb8888ToRgba(frame, rgba);
    } else if (frame.pixelFormat === 1) {
      rgb565ToRgba(frame, rgba);
    } else if (frame.pixelFormat === 2) {
      xrgb1555ToRgba(frame, rgba);
    } else {
      return;
    }

    const imageData = new ImageData(rgba, frame.width, frame.height);
    this.ctx.putImageData(imageData, 0, 0);
  }

  private enqueueAudioPacket(packet: DecodedAudioPacket): void {
    if (!this.config.enableAudio || !this.audioCtx || this.audioCtx.state !== "running" || !this.audioGainNode) {
      return;
    }

    const sampleCount = packet.frameCount * packet.channels;
    if (sampleCount === 0 || sampleCount * 2 > packet.payloadLen) {
      return;
    }

    const pcm = new Int16Array(packet.buffer, packet.pcmOffset, sampleCount);
    const audioBuffer = this.audioCtx.createBuffer(packet.channels, packet.frameCount, packet.sampleRateHz);

    for (let channel = 0; channel < packet.channels; channel++) {
      const out = audioBuffer.getChannelData(channel);
      for (let i = 0; i < packet.frameCount; i++) {
        out[i] = pcm[i * packet.channels + channel] / 32768.0;
      }
    }

    const now = this.audioCtx.currentTime;
    if (this.audioStartTime < now + 0.04) {
      this.audioStartTime = now + 0.04;
    }
    if (this.audioStartTime - now > 0.3) {
      this.audioStartTime = now + 0.08;
    }

    const source = this.audioCtx.createBufferSource();
    source.buffer = audioBuffer;
    source.connect(this.audioGainNode);
    source.start(this.audioStartTime);
    source.onended = () => source.disconnect();
    this.audioStartTime += audioBuffer.duration;
  }
}

function waitForIceGatheringComplete(peer: RTCPeerConnection): Promise<void> {
  if (peer.iceGatheringState === "complete") {
    return Promise.resolve();
  }

  return new Promise((resolve) => {
    const timeout = window.setTimeout(() => {
      peer.removeEventListener("icegatheringstatechange", onState);
      resolve();
    }, 1800);

    const onState = (): void => {
      if (peer.iceGatheringState === "complete") {
        window.clearTimeout(timeout);
        peer.removeEventListener("icegatheringstatechange", onState);
        resolve();
      }
    };

    peer.addEventListener("icegatheringstatechange", onState);
  });
}

function readU64LittleEndian(view: DataView, offset: number): number {
  if (typeof view.getBigUint64 === "function") {
    return Number(view.getBigUint64(offset, true));
  }
  return view.getUint32(offset + 4, true) * 4294967296 + view.getUint32(offset, true);
}

function decodeFramePacket(buffer: ArrayBuffer): DecodedFramePacket | null {
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

function decodeAudioPacket(buffer: ArrayBuffer): DecodedAudioPacket | null {
  const view = new DataView(buffer);
  if (view.byteLength < 34) {
    return null;
  }
  if (
    view.getUint8(0) !== 0x4e ||
    view.getUint8(1) !== 0x42 ||
    view.getUint8(2) !== 0x41 ||
    view.getUint8(3) !== 0x30
  ) {
    return null;
  }

  const sampleRateHz = view.getUint32(20, true);
  const channels = view.getUint8(24);
  const sampleFormat = view.getUint8(25);
  const frameCount = view.getUint32(26, true);
  const payloadLen = view.getUint32(30, true);

  if (sampleFormat !== 0 || channels < 1 || channels > 2 || sampleRateHz < 8000) {
    return null;
  }
  if (34 + payloadLen > view.byteLength) {
    return null;
  }

  return {
    sampleRateHz,
    channels,
    frameCount,
    payloadLen,
    buffer,
    pcmOffset: 34
  };
}

function decodeRtcChunk(buffer: ArrayBuffer): DecodedRtcChunk | null {
  const view = new DataView(buffer);
  if (view.byteLength < RTC_CHUNK_HEADER_LEN) {
    return null;
  }
  if (view.getUint32(0, true) !== RTC_CHUNK_MAGIC) {
    return null;
  }

  const messageId = view.getUint32(4, true);
  const chunkIndex = view.getUint16(8, true);
  const totalChunks = view.getUint16(10, true);
  if (totalChunks < 1 || chunkIndex >= totalChunks) {
    return null;
  }

  return {
    messageId,
    chunkIndex,
    totalChunks,
    payload: new Uint8Array(buffer, RTC_CHUNK_HEADER_LEN, view.byteLength - RTC_CHUNK_HEADER_LEN).slice()
  };
}

function decodeVp8VideoPacket(buffer: ArrayBuffer): Vp8Packet | null {
  const view = new DataView(buffer);
  if (view.byteLength < VP8_VIDEO_HEADER_LEN) {
    return null;
  }
  if (view.getUint32(0, true) !== VP8_VIDEO_MAGIC) {
    return null;
  }

  const ptsUs = readU64LittleEndian(view, 4);
  const durationUs = view.getUint32(12, true);
  const flags = view.getUint8(16);
  const payloadLen = view.getUint32(17, true);
  if (VP8_VIDEO_HEADER_LEN + payloadLen > view.byteLength) {
    return null;
  }

  return {
    ptsUs,
    durationUs,
    keyframe: (flags & 0x01) !== 0,
    payload: new Uint8Array(buffer, VP8_VIDEO_HEADER_LEN, payloadLen)
  };
}

function xrgb8888ToRgba(frame: DecodedFramePacket, out: Uint8ClampedArray): void {
  const src = frame.bytes;
  let dstOffset = 0;
  for (let y = 0; y < frame.height; y++) {
    const row = y * frame.pitch;
    for (let x = 0; x < frame.width; x++) {
      const i = row + x * 4;
      out[dstOffset++] = src[i + 2];
      out[dstOffset++] = src[i + 1];
      out[dstOffset++] = src[i];
      out[dstOffset++] = 255;
    }
  }
}

function rgb565ToRgba(frame: DecodedFramePacket, out: Uint8ClampedArray): void {
  const src = frame.bytes;
  let dstOffset = 0;
  for (let y = 0; y < frame.height; y++) {
    const row = y * frame.pitch;
    for (let x = 0; x < frame.width; x++) {
      const i = row + x * 2;
      const value = src[i] | (src[i + 1] << 8);
      const r = (value >> 11) & 0x1f;
      const g = (value >> 5) & 0x3f;
      const b = value & 0x1f;
      out[dstOffset++] = (r * 255) / 31;
      out[dstOffset++] = (g * 255) / 63;
      out[dstOffset++] = (b * 255) / 31;
      out[dstOffset++] = 255;
    }
  }
}

function xrgb1555ToRgba(frame: DecodedFramePacket, out: Uint8ClampedArray): void {
  const src = frame.bytes;
  let dstOffset = 0;
  for (let y = 0; y < frame.height; y++) {
    const row = y * frame.pitch;
    for (let x = 0; x < frame.width; x++) {
      const i = row + x * 2;
      const value = src[i] | (src[i + 1] << 8);
      const r = (value >> 10) & 0x1f;
      const g = (value >> 5) & 0x1f;
      const b = value & 0x1f;
      out[dstOffset++] = (r * 255) / 31;
      out[dstOffset++] = (g * 255) / 31;
      out[dstOffset++] = (b * 255) / 31;
      out[dstOffset++] = 255;
    }
  }
}
