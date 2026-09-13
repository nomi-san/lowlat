// The picture: one message on channel 1 is one access unit with no header,
// so what the stream is -- its size, its codec, whether a unit is a keyframe
// -- is read from the bitstream itself. Decoded by the browser's own decoder,
// drawn as a textured quad; the newest frame wins when the display is slower
// than the stream.

const VERTEX = `#version 300 es
in vec2 xy;
out vec2 uv;
void main() {
  gl_Position = vec4(xy, 0.0, 1.0);
  uv = vec2((xy.x + 1.0) * 0.5, (1.0 - xy.y) * 0.5);
}`

const FRAGMENT = `#version 300 es
precision mediump float;
in vec2 uv;
uniform sampler2D picture;
out vec4 color;
void main() { color = texture(picture, uv); }`

export interface VideoStats {
  frames: number
  bytes: number
  largestUnit: number
  decodeMs: number
  width: number
  height: number
  keyframes: number
}

export class Video {
  private readonly gl: WebGL2RenderingContext
  private decoder?: VideoDecoder
  private pending?: VideoFrame
  private scheduled = false
  private awaitingKey = true
  private lastKeyRequest = 0
  readonly stats: VideoStats = { frames: 0, bytes: 0, largestUnit: 0, decodeMs: 0, width: 0, height: 0, keyframes: 0 }

  // Called when the decoder needs a fresh reference chain.
  onNeedKeyframe: () => void = () => {}

  constructor(private readonly canvas: HTMLCanvasElement, private readonly hevc: boolean) {
    const gl = canvas.getContext('webgl2', { depth: false, antialias: false, alpha: false })
    if (!gl) throw new Error('WebGL2 is unavailable')
    this.gl = gl
    const program = gl.createProgram()!
    for (const [kind, source] of [[gl.VERTEX_SHADER, VERTEX], [gl.FRAGMENT_SHADER, FRAGMENT]] as const) {
      const shader = gl.createShader(kind)!
      gl.shaderSource(shader, source)
      gl.compileShader(shader)
      if (!gl.getShaderParameter(shader, gl.COMPILE_STATUS)) throw new Error(gl.getShaderInfoLog(shader) ?? 'shader')
      gl.attachShader(program, shader)
    }
    gl.linkProgram(program)
    if (!gl.getProgramParameter(program, gl.LINK_STATUS)) throw new Error(gl.getProgramInfoLog(program) ?? 'link')
    gl.useProgram(program)
    gl.bindBuffer(gl.ARRAY_BUFFER, gl.createBuffer())
    gl.bufferData(gl.ARRAY_BUFFER, new Float32Array([-1, -1, 1, -1, -1, 1, 1, 1]), gl.STATIC_DRAW)
    const xy = gl.getAttribLocation(program, 'xy')
    gl.enableVertexAttribArray(xy)
    gl.vertexAttribPointer(xy, 2, gl.FLOAT, false, 0, 0)
    gl.bindTexture(gl.TEXTURE_2D, gl.createTexture())
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.LINEAR)
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.LINEAR)
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE)
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE)
    gl.pixelStorei(gl.UNPACK_FLIP_Y_WEBGL, false)
    this.configure()
  }

  static config(hevc: boolean): VideoDecoderConfig {
    return { codec: hevc ? 'hvc1.1.6.L120.00' : 'avc1.42001E', optimizeForLatency: true }
  }

  push(unit: ArrayBuffer): void {
    const key = isKeyframe(new Uint8Array(unit), this.hevc)
    this.stats.bytes += unit.byteLength
    if (unit.byteLength > this.stats.largestUnit) this.stats.largestUnit = unit.byteLength
    if (key) this.stats.keyframes++
    // A decoder that has just been built takes nothing until a keyframe.
    if (this.awaitingKey && !key) {
      this.requestKeyframe()
      return
    }
    this.awaitingKey = false
    try {
      this.decoder!.decode(new EncodedVideoChunk({
        type: key ? 'key' : 'delta',
        timestamp: Math.round(performance.now() * 1000),
        data: unit,
      }))
    } catch (err) {
      console.warn('[video] decode refused:', err)
      this.recover()
    }
  }

  close(): void {
    this.decoder?.close()
    this.decoder = undefined
    this.pending?.close()
    this.pending = undefined
  }

  private configure(): void {
    this.decoder = new VideoDecoder({
      output: (frame) => this.output(frame),
      error: (err) => {
        console.warn('[video] decoder error:', err)
        this.recover()
      },
    })
    this.decoder.configure(Video.config(this.hevc))
    this.awaitingKey = true
  }

  private recover(): void {
    if (this.decoder && this.decoder.state !== 'closed') this.decoder.close()
    this.configure()
    this.requestKeyframe()
  }

  private requestKeyframe(): void {
    const now = performance.now()
    if (now - this.lastKeyRequest < 1000) return
    this.lastKeyRequest = now
    this.onNeedKeyframe()
  }

  private output(frame: VideoFrame): void {
    const s = this.stats
    s.frames++
    s.decodeMs = 0.9 * s.decodeMs + 0.1 * (performance.now() - frame.timestamp / 1000)
    this.pending?.close()
    this.pending = frame
    if (!this.scheduled) {
      this.scheduled = true
      requestAnimationFrame(() => this.draw())
    }
  }

  private draw(): void {
    this.scheduled = false
    const frame = this.pending
    if (!frame) return
    this.pending = undefined
    const gl = this.gl
    this.stats.width = frame.displayWidth
    this.stats.height = frame.displayHeight
    if (this.canvas.width !== frame.displayWidth || this.canvas.height !== frame.displayHeight) {
      this.canvas.width = frame.displayWidth
      this.canvas.height = frame.displayHeight
    }
    gl.viewport(0, 0, this.canvas.width, this.canvas.height)
    gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA, gl.RGBA, gl.UNSIGNED_BYTE, frame)
    gl.drawArrays(gl.TRIANGLE_STRIP, 0, 4)
    frame.close()
  }
}

// Whether an access unit carries an intra picture: an IDR unit for H.264, any
// of the random-access types for HEVC. Start codes are three or four bytes.
function isKeyframe(unit: Uint8Array, hevc: boolean): boolean {
  const n = unit.length
  for (let i = 0; i + 3 < n; i++) {
    if (unit[i] !== 0 || unit[i + 1] !== 0) continue
    let at: number
    if (unit[i + 2] === 1) at = i + 3
    else if (unit[i + 2] === 0 && unit[i + 3] === 1) at = i + 4
    else continue
    if (at >= n) break
    const header = unit[at]!
    if (hevc) {
      const type = (header >> 1) & 0x3f
      if (type >= 16 && type <= 21) return true
    } else if ((header & 0x1f) === 5) {
      return true
    }
    i = at
  }
  return false
}
