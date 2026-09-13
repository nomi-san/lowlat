// The signaling client, in the peer role (docs/04-signaling.md). One
// websocket, authenticated by the query string; every message is
// { version, action, payload }. It carries the credential triple and the
// candidates and nothing else, and it stops mattering once the path is up.

export interface Credentials {
  ice_ufrag: string
  ice_pwd: string
  fingerprint: string
}

export interface Candidate {
  ip: string
  port: number
  lan: boolean
  from_stun: boolean
  sync: boolean
}

export interface Handlers {
  answer(creds: Credentials): void
  candidate(c: Candidate): void
  // The service or the host ended the attempt, or the socket went away.
  closed(reason: string): void
}

const VERSIONS = { p2p: 1, bud: 1, init: 1, video: 1, audio: 1, control: 1 }

export class Signaling {
  private socket: WebSocket
  private pending: string[] = []
  private closed = false

  constructor(
    server: string,
    session: string,
    private readonly peer: string,
    private readonly attempt: string,
    private readonly handlers: Handlers,
  ) {
    const query = new URLSearchParams({
      session_id: session,
      role: 'client',
      version: '1',
      build: 'lowlat-web-client',
      sdk_version: '0',
    })
    this.socket = new WebSocket(`wss://${server.replace(/\/+$/, '')}/?${query}`)
    this.socket.onopen = () => {
      for (const m of this.pending) this.socket.send(m)
      this.pending = []
    }
    this.socket.onclose = () => {
      if (!this.closed) this.handlers.closed('signaling socket closed')
    }
    this.socket.onerror = () => {
      if (!this.closed) this.handlers.closed('signaling socket failed')
    }
    this.socket.onmessage = (e) => this.onMessage(JSON.parse(e.data as string))
  }

  // Ask for a session over the browser pipe (mode 2) with our credential
  // triple; the answer carries the host's.
  offer(creds: Credentials): void {
    this.send('offer', {
      to: this.peer,
      attempt_id: this.attempt,
      secret: '',
      data: { ver_data: 1, creds, mode: 2, versions: VERSIONS },
    })
  }

  candidate(c: Candidate): void {
    this.send('candex', {
      to: this.peer,
      attempt_id: this.attempt,
      data: { ver_data: 1, versions: VERSIONS, ...c },
    })
  }

  cancel(): void {
    this.send('offer_cancel', { to: this.peer, attempt_id: this.attempt })
  }

  close(): void {
    this.closed = true
    this.socket.close(1000)
  }

  private send(action: string, payload: object): void {
    const text = JSON.stringify({ version: 1, action, payload })
    if (this.socket.readyState === WebSocket.OPEN) this.socket.send(text)
    else if (this.socket.readyState === WebSocket.CONNECTING) this.pending.push(text)
  }

  private onMessage(m: { action: string; payload: any }): void {
    switch (m.action) {
      case 'answer_relay':
        if (m.payload.attempt_id !== this.attempt) return
        if (!m.payload.approved) {
          this.handlers.closed('the host declined')
          return
        }
        this.handlers.answer(m.payload.data.creds)
        break
      case 'candex_relay':
        if (m.payload.attempt_id !== this.attempt) return
        this.handlers.candidate(m.payload.data)
        break
      case 'close':
        this.handlers.closed(`service closed the attempt: ${m.payload?.reason ?? 'no reason'}`)
        break
      default:
        break
    }
  }
}

// An attempt identifier is opaque to the service; six random 32-bit groups
// is the shape peers already use.
export function attemptId(): string {
  const words = new Uint32Array(6)
  crypto.getRandomValues(words)
  return Array.from(words, (w) => w.toString(16).padStart(8, '0')).join('-')
}
