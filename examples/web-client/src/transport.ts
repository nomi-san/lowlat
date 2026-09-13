// The browser pipe: a peer connection with three pre-agreed data channels,
// one per protocol channel (docs/01-protocol.md section 14). No session
// description crosses signaling; each side synthesizes its own from the
// credential triple, and the answer written here names the host as the
// active side of the handshake because that is the side it takes.

import type { Candidate, Credentials } from './signaling'

// Streams 0, 1 and 2 are agreed in advance and never opened in band: an
// in-band open is a control message the host does not answer. Stream 2 is
// sound, which this page does not play, and it exists here so the browser
// has somewhere to put what arrives on it.
const CHANNELS: ReadonlyArray<[number, string]> = [
  [0, 'control'],
  [1, 'video'],
  [2, 'audio'],
]

// The largest message the host accepts on any stream.
const HOST_MAX_MESSAGE = 4194304

export class Transport {
  private readonly pc: RTCPeerConnection
  private readonly channels = new Map<number, RTCDataChannel>()
  private mid = '0'
  private remoteReady = false
  private remoteQueue: Candidate[] = []
  private syncSent = false
  private syncTimer = 0

  onMessage: (channel: number, data: ArrayBuffer) => void = () => {}
  onOpen: () => void = () => {}
  onState: (state: RTCPeerConnectionState) => void = () => {}
  onLocalCandidate: (c: Candidate) => void = () => {}

  constructor(stunServer: string) {
    this.pc = new RTCPeerConnection({ iceServers: [{ urls: stunServer }] })
    this.pc.onicecandidate = (e) => this.gathered(e.candidate)
    this.pc.onconnectionstatechange = () => this.onState(this.pc.connectionState)
    for (const [id, label] of CHANNELS) {
      const ch = this.pc.createDataChannel(label, { id, negotiated: true, ordered: true })
      ch.binaryType = 'arraybuffer'
      ch.onmessage = (e) => this.onMessage(id, e.data as ArrayBuffer)
      if (id === 0) ch.onopen = () => this.onOpen()
      this.channels.set(id, ch)
    }
  }

  // Our credential triple, read out of the description the browser wrote.
  // Setting it as the local description starts candidate gathering at once;
  // the host buffers candidates that arrive before it has answered.
  async offer(): Promise<Credentials> {
    const offer = await this.pc.createOffer()
    await this.pc.setLocalDescription(offer)
    const sdp = offer.sdp ?? ''
    const attr = (key: string) => sdp.match(new RegExp(`^a=${key}:(.+)$`, 'm'))?.[1]?.trim() ?? ''
    this.mid = attr('mid') || '0'
    return { ice_ufrag: attr('ice-ufrag'), ice_pwd: attr('ice-pwd'), fingerprint: attr('fingerprint') }
  }

  // The host's triple becomes the answer. The data channel line is the
  // current form, which every browser family accepts; the stream port and
  // the message ceiling are the host's. Candidates queued before this point
  // are added only now, because one browser family refuses a candidate
  // offered before the remote description exists.
  async answer(host: Credentials): Promise<void> {
    const sdp = [
      'v=0',
      'o=- 0 2 IN IP4 127.0.0.1',
      's=-',
      't=0 0',
      `a=group:BUNDLE ${this.mid}`,
      'a=msid-semantic: WMS *',
      'm=application 9 UDP/DTLS/SCTP webrtc-datachannel',
      'c=IN IP4 0.0.0.0',
      `a=ice-ufrag:${host.ice_ufrag}`,
      `a=ice-pwd:${host.ice_pwd}`,
      'a=ice-options:trickle',
      `a=fingerprint:${host.fingerprint}`,
      'a=setup:active',
      `a=mid:${this.mid}`,
      'a=sctp-port:5000',
      `a=max-message-size:${HOST_MAX_MESSAGE}`,
    ].join('\r\n') + '\r\n'
    await this.pc.setRemoteDescription({ type: 'answer', sdp })
    this.remoteReady = true
    for (const c of this.remoteQueue) this.addRemote(c)
    this.remoteQueue = []
    // The readiness marker tells the host every candidate of ours is out. It
    // follows gathering, with a cap so a silent reflexive server cannot hold
    // the host's checks back indefinitely.
    this.syncTimer = window.setTimeout(() => this.sync(), 3000)
  }

  remoteCandidate(c: Candidate): void {
    // The host's own readiness marker carries no address.
    if (c.sync) return
    if (!this.remoteReady) this.remoteQueue.push(c)
    else this.addRemote(c)
  }

  send(channel: number, data: ArrayBuffer): boolean {
    const ch = this.channels.get(channel)
    if (!ch || ch.readyState !== 'open') return false
    ch.send(data)
    return true
  }

  async roundTripMs(): Promise<number | undefined> {
    const stats = await this.pc.getStats()
    let rtt: number | undefined
    for (const r of stats.values()) {
      if (r.type !== 'candidate-pair' || r.currentRoundTripTime === undefined) continue
      // The pair in use where the browser marks one, else any measured pair.
      if (r.nominated || r.selected || rtt === undefined) rtt = r.currentRoundTripTime * 1000
    }
    return rtt
  }

  close(): void {
    window.clearTimeout(this.syncTimer)
    for (const ch of this.channels.values()) ch.close()
    this.pc.close()
  }

  private addRemote(c: Candidate): void {
    const ip = c.ip.replace(/^::ffff:/, '')
    const typ = c.from_stun ? 'srflx' : 'host'
    void this.pc.addIceCandidate({
      candidate: `candidate:1 1 udp 2113937151 ${ip} ${c.port} typ ${typ} generation 0`,
      sdpMid: this.mid,
      sdpMLineIndex: 0,
    })
  }

  private gathered(cand: RTCIceCandidate | null): void {
    if (!cand) {
      this.sync()
      return
    }
    // "candidate:<foundation> <component> udp <priority> <ip> <port> typ <typ> ..."
    const parts = cand.candidate.replace(/^candidate:/, '').split(' ')
    if (parts.length < 8 || parts[2]?.toLowerCase() !== 'udp') return
    const typ = parts[7]
    const ip = parts[4] ?? ''
    if (typ !== 'host' && typ !== 'srflx') return
    // A host candidate hidden behind a multicast name carries nothing a host
    // can probe; the host learns that address from our own checks instead.
    if (ip.endsWith('.local')) return
    this.onLocalCandidate({
      ip,
      port: parseInt(parts[5] ?? '0', 10),
      lan: typ === 'host',
      from_stun: typ === 'srflx',
      sync: false,
    })
  }

  private sync(): void {
    if (this.syncSent) return
    this.syncSent = true
    window.clearTimeout(this.syncTimer)
    // The marker is read for its flag; the address is a placeholder.
    this.onLocalCandidate({ ip: '1.2.3.4', port: 1234, lan: false, from_stun: false, sync: true })
  }
}
