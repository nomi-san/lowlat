// Control messages: a 13-byte big-endian header of three arguments and an
// opcode, then an optional body. On the browser pipe one message on channel 0
// is one of these, header included (docs/01-protocol.md section 11).

export const HEADER_LEN = 13

export const op = {
  // Sent by a peer.
  INIT: 11,
  ENCODER_CONFIG: 13,
  USER_DATA: 17,
  DISCONNECT: 10,
  // Sent by a host.
  CURSOR: 9,
  ENCODE_LATENCY: 21,
  GUEST_LIST: 25,
  ENCODER_GENERATION: 29,
} as const

export interface Message {
  a0: number
  a1: number
  a2: number
  opcode: number
  body: Uint8Array
}

export function encode(opcode: number, a0: number, a1: number, a2: number, body?: Uint8Array): ArrayBuffer {
  const out = new ArrayBuffer(HEADER_LEN + (body?.byteLength ?? 0))
  const view = new DataView(out)
  view.setUint32(0, a0 >>> 0)
  view.setUint32(4, a1 >>> 0)
  view.setUint32(8, a2 >>> 0)
  view.setUint8(12, opcode)
  if (body) new Uint8Array(out, HEADER_LEN).set(body)
  return out
}

export function decode(data: ArrayBuffer): Message | undefined {
  if (data.byteLength < HEADER_LEN) return undefined
  const view = new DataView(data)
  return {
    a0: view.getInt32(0),
    a1: view.getInt32(4),
    a2: view.getInt32(8),
    opcode: view.getUint8(12),
    body: new Uint8Array(data, HEADER_LEN),
  }
}

// A string body is NUL terminated and its declared length counts the
// terminator (docs/01-protocol.md section 11.2a).
function stringBody(text: string): Uint8Array {
  const encoded = new TextEncoder().encode(text)
  const body = new Uint8Array(encoded.byteLength + 1)
  body.set(encoded)
  return body
}

// The session declaration: exactly these eight keys, in this order, and no
// others (docs/01-protocol.md section 11.5). 60000 is the no-limit sentinel
// and 0 is no preference.
export function init(flags: number): ArrayBuffer {
  const body = stringBody(JSON.stringify({
    _version: 1,
    _max_w: 60000,
    _max_h: 60000,
    _flags: flags,
    resolutionX: 0,
    resolutionY: 0,
    mediaContainer: 0,
    refreshRate: 60,
  }))
  return encode(op.INIT, body.byteLength, 0, 0, body)
}

// Application message: the body's length in argument 0, the sub-identifier
// in argument 1.
export function userData(id: number, text: string): ArrayBuffer {
  const body = stringBody(text)
  return encode(op.USER_DATA, body.byteLength, id, 0, body)
}

// Opcode 13 restates the flags for the primary stream; with the third
// argument set and the flags unchanged it asks for a keyframe.
export function encoderConfig(flags: number, reinit: boolean): ArrayBuffer {
  return encode(op.ENCODER_CONFIG, 0, flags, reinit ? 1 : 0)
}

// A peer leaving cleanly sends a zero status; the status field is meaningful
// host to peer only.
export function disconnect(): ArrayBuffer {
  return encode(op.DISCONNECT, 0, 0, 0)
}

export const FLAG_HEVC = 0x01
