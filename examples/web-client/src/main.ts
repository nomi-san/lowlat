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
const relayOnly = $<HTMLInputElement>('relay')

let client: Client | undefined
let ticker = 0
let phase = 'idle'

// Both codecs are offered; the second only where this browser decodes it.
hevc.disabled = true
VideoDecoder.isConfigSupported(Video.config(true))
  .then((r) => { hevc.disabled = !r.supported })
  .catch(() => {})

// The ICE servers are those in ice-servers.json beside the page, in the form
// relay services hand them out ({ "iceServers": [...] }), else one public
// reflexive server. A relay's credentials reach the page that way rather than
// through the form, and the relay is offered once one is named.
const PUBLIC_REFLEXIVE: RTCIceServer[] = [{ urls: 'stun:stun.l.google.com:19302' }]
let iceServers = PUBLIC_REFLEXIVE
relayOnly.disabled = true

const urlsOf = (s: RTCIceServer) => (Array.isArray(s.urls) ? s.urls : [s.urls])

// A relayed session goes through the relay alone; a direct one is given the
// reflexive servers alone, so the browser never moves it onto a relay's
// longer path mid-session, which it did when both were on offer.
function serversFor(relay: boolean): RTCIceServer[] {
  if (relay) return iceServers
  const direct = iceServers
    .map((s) => ({ urls: urlsOf(s).filter((u) => /^stuns?:/.test(u)) }))
    .filter((s) => s.urls.length > 0)
  return direct.length > 0 ? direct : PUBLIC_REFLEXIVE
}
fetch('ice-servers.json', { cache: 'no-store' })
  .then((r) => (r.ok ? r.json() : undefined))
  .then((j) => {
    const list: unknown = Array.isArray(j) ? j : j?.iceServers
    if (!Array.isArray(list) || list.length === 0) return
    iceServers = list as RTCIceServer[]
    const urls = iceServers.flatMap(urlsOf)
    const relays = urls.filter((u) => /^turns?:/.test(u)).length
    relayOnly.disabled = relays === 0
    if (client) return
    phase = `idle | ice-servers.json: ${urls.length} urls, ${relays} relays`
    render()
  })
  .catch(() => {})

form.onsubmit = (e) => {
  e.preventDefault()
  if (client) return
  const value = (id: string) => $<HTMLInputElement>(id).value.trim()
  client = new Client(canvas, {
    server: value('server'),
    session: value('session'),
    peer: value('peer'),
    iceServers: serversFor(relayOnly.checked),
    relayOnly: relayOnly.checked,
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
  void client.path().then(({ rttMs, via }) => {
    if (client !== c) return
    status.textContent =
      `${phase} | ${v.width}x${v.height} ${fps} fps ${kbit.toFixed(0)} kbit/s` +
      ` | rtt ${rttMs === undefined ? '?' : rttMs.toFixed(1)} ms via ${via ?? '?'}` +
      ` | encode ${c.stats.encodeMs.toFixed(1)} ms` +
      ` decode ${v.decodeMs.toFixed(1)} ms | frames ${v.frames} keyframes ${v.keyframes}` +
      ` largest ${v.largestUnit} B | control ${c.stats.controlMessages} last op ${c.stats.lastOpcode}`
  })
}
