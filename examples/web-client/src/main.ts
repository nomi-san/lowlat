import { Client } from './client'
import { Video } from './video'

const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T

const form = $<HTMLFormElement>('form')
const status = $<HTMLDivElement>('status')
const canvas = $<HTMLCanvasElement>('screen')
const connect = $<HTMLButtonElement>('connect')
const disconnect = $<HTMLButtonElement>('disconnect')
const keyframe = $<HTMLButtonElement>('keyframe')
const hevc = $<HTMLInputElement>('hevc')

let client: Client | undefined
let ticker = 0
let phase = 'idle'

// Both codecs are offered; the second only where this browser decodes it.
hevc.disabled = true
VideoDecoder.isConfigSupported(Video.config(true))
  .then((r) => { hevc.disabled = !r.supported })
  .catch(() => {})

form.onsubmit = (e) => {
  e.preventDefault()
  if (client) return
  const value = (id: string) => $<HTMLInputElement>(id).value.trim()
  client = new Client(canvas, {
    server: value('server'),
    session: value('session'),
    peer: value('peer'),
    stunServer: 'stun:stun.l.google.com:19302',
    hevc: hevc.checked,
    bitrateMbit: Number(value('bitrate')) || 30,
    fps: Number(value('fps')) || 60,
  })
  client.onStatus = (text) => { phase = text }
  client.onEnd = (reason) => {
    phase = `ended: ${reason}`
    client = undefined
    window.clearInterval(ticker)
    render()
    connect.disabled = false
    disconnect.disabled = keyframe.disabled = true
  }
  connect.disabled = true
  disconnect.disabled = keyframe.disabled = false
  void client.start().catch((err) => client?.onEnd(String(err)))
  ticker = window.setInterval(render, 1000)
}

disconnect.onclick = () => client?.leave()
keyframe.onclick = () => client?.requestKeyframe()
window.addEventListener('beforeunload', () => client?.leave())

let lastFrames = 0
let lastBytes = 0

function render(): void {
  if (!client) {
    status.textContent = phase
    return
  }
  const v = client.videoStats
  const fps = v.frames - lastFrames
  const kbit = ((v.bytes - lastBytes) * 8) / 1000
  lastFrames = v.frames
  lastBytes = v.bytes
  const c = client
  void client.roundTripMs().then((rtt) => {
    if (client !== c) return
    status.textContent =
      `${phase} | ${v.width}x${v.height} ${fps} fps ${kbit.toFixed(0)} kbit/s` +
      ` | rtt ${rtt === undefined ? '?' : rtt.toFixed(1)} ms encode ${c.stats.encodeMs.toFixed(1)} ms` +
      ` decode ${v.decodeMs.toFixed(1)} ms | frames ${v.frames} keyframes ${v.keyframes}` +
      ` largest ${v.largestUnit} B | control ${c.stats.controlMessages} last op ${c.stats.lastOpcode}`
  })
}
