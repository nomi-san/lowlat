// One session: signaling to a path, the declaration on channel 0, then the
// picture until either side leaves or the control channel falls silent.

import * as control from './control'
import { Signaling, attemptId, type Credentials } from './signaling'
import { Transport } from './transport'
import { Video } from './video'

export interface Options {
  server: string
  session: string
  peer: string
  stunServer: string
  hevc: boolean
  // The video configuration a guest asks for through the application
  // message; the host reports what it applied on its own log.
  bitrateMbit: number
  fps: number
}

export interface ClientStats {
  encodeMs: number
  controlMessages: number
  lastOpcode: number
}

// A page that hears nothing on the control channel for this long reads the
// link as dead; the host's encode-latency report every two seconds is what
// keeps it alive on a still desktop.
const SILENCE_MS = 5000

export class Client {
  private readonly signaling: Signaling
  private readonly transport: Transport
  private readonly video: Video
  private readonly attempt = attemptId()
  private lastControl = performance.now()
  private readonly liveness: number
  private opened_ = false
  private configured = false
  private ended = false
  readonly stats: ClientStats = { encodeMs: 0, controlMessages: 0, lastOpcode: -1 }

  onStatus: (text: string) => void = () => {}
  onEnd: (reason: string) => void = () => {}

  constructor(canvas: HTMLCanvasElement, private readonly options: Options) {
    this.video = new Video(canvas, options.hevc)
    this.video.onNeedKeyframe = () => this.send(control.encoderConfig(this.flags(), true))

    this.transport = new Transport(options.stunServer)
    this.transport.onMessage = (channel, data) => this.receive(channel, data)
    this.transport.onOpen = () => this.opened()
    this.transport.onState = (state) => {
      this.onStatus(`peer connection ${state}`)
      if (state === 'failed' || state === 'closed') this.end(`peer connection ${state}`)
    }
    this.transport.onLocalCandidate = (c) => this.signaling.candidate(c)

    this.signaling = new Signaling(options.server, options.session, options.peer, this.attempt, {
      answer: (creds) => this.answered(creds),
      candidate: (c) => this.transport.remoteCandidate(c),
      closed: (reason) => this.end(reason),
    })
    this.liveness = window.setInterval(() => {
      if (performance.now() - this.lastControl > SILENCE_MS) this.end('control channel silent for 5 s')
    }, 1000)
  }

  async start(): Promise<void> {
    this.onStatus('offering')
    const creds = await this.transport.offer()
    this.signaling.offer(creds)
  }

  // Leave cleanly: tell the host, or withdraw the offer if it never answered,
  // then take the pipe down.
  leave(): void {
    if (this.opened_) this.send(control.disconnect())
    else this.signaling.cancel()
    this.end('left')
  }

  requestKeyframe(): void {
    this.send(control.encoderConfig(this.flags(), true))
  }

  get videoStats() {
    return this.video.stats
  }

  roundTripMs(): Promise<number | undefined> {
    return this.transport.roundTripMs()
  }

  private flags(): number {
    return this.options.hevc ? control.FLAG_HEVC : 0
  }

  private async answered(creds: Credentials): Promise<void> {
    this.onStatus('answered, connecting')
    await this.transport.answer(creds)
  }

  // Channel 0 is open: declare. The host begins the stream on the declaration.
  private opened(): void {
    this.opened_ = true
    this.lastControl = performance.now()
    this.send(control.init(this.flags()))
    this.onStatus('connected')
    // Signaling has done its work; the path carries the session from here.
    this.signaling.close()
  }

  // The rate and frame rate are a change to a running stream, so they are
  // asked for once the first picture proves there is one.
  private configure(): void {
    this.configured = true
    this.send(control.userData(11, JSON.stringify({
      video: [{
        encoderMaxBitrate: this.options.bitrateMbit,
        encoderFPS: this.options.fps,
        fullFPS: false,
      }],
    })))
  }

  private receive(channel: number, data: ArrayBuffer): void {
    switch (channel) {
      case 0:
        this.lastControl = performance.now()
        this.stats.controlMessages++
        this.onControl(data)
        break
      case 1:
        if (!this.configured) this.configure()
        this.video.push(data)
        break
      default:
        // Sound, which this page does not play.
        break
    }
  }

  private onControl(data: ArrayBuffer): void {
    const m = control.decode(data)
    if (!m) return
    this.stats.lastOpcode = m.opcode
    switch (m.opcode) {
      case control.op.DISCONNECT:
        this.end(`host ended the session, status ${m.a0}`)
        break
      case control.op.ENCODE_LATENCY:
        // (1, microseconds, stream) from a host.
        this.stats.encodeMs = m.a1 / 1000
        break
      default:
        break
    }
  }

  private send(message: ArrayBuffer): void {
    this.transport.send(0, message)
  }

  private end(reason: string): void {
    if (this.ended) return
    this.ended = true
    window.clearInterval(this.liveness)
    this.signaling.close()
    this.transport.close()
    this.video.close()
    this.onEnd(reason)
  }
}
