// An H.264 sequence set that does not say how far its pictures reorder is
// held back by a browser's decoder to the level's worst case: four pictures
// measured, 130 ms at 30 frames a second, from a stream that reorders
// nothing. Where a unit's sequence set leaves the depth unsaid and its order
// counts follow frame_num -- picture order count type 2, under which output
// order is decode order -- the set is rewritten to say so: no reordering, a
// buffer of its reference count. A set of any other type may reorder and
// passes as it came, as do one that states its depth already, one this does
// not read (scaling lists), and HEVC, whose sequence set always carries it.

// The profiles whose sequence set carries the chroma and depth fields.
const HIGH_PROFILES = new Set([100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135])

// The unit with its sequence set stating the reorder depth, or the unit
// itself. Only the parameter sets before the unit's first slice are looked at.
export function stateReorderDepth(unit: Uint8Array): Uint8Array {
  let at = startAfter(unit, 0)
  while (at >= 0 && at < unit.length) {
    const type = unit[at]! & 0x1f
    if (type >= 1 && type <= 5) break
    const next = startAfter(unit, at)
    let end = next < 0 ? unit.length : next - 3
    while (end > at && unit[end - 1] === 0) end--
    if (type === 7) {
      const sps = restricted(unit.subarray(at, end))
      if (!sps) break
      const out = new Uint8Array(unit.length - (end - at) + sps.length)
      out.set(unit.subarray(0, at), 0)
      out.set(sps, at)
      out.set(unit.subarray(end), at + sps.length)
      return out
    }
    at = next
  }
  return unit
}

// The index just past the next start code at or after `from`, or -1.
function startAfter(b: Uint8Array, from: number): number {
  for (let i = from; i + 2 < b.length; i++) {
    if (b[i] === 0 && b[i + 1] === 0 && b[i + 2] === 1) return i + 3
  }
  return -1
}

// The sequence set (header byte included) with a bitstream restriction, or
// null where it has one already or cannot be read.
function restricted(nal: Uint8Array): Uint8Array | null {
  const raw = unescape(nal.subarray(1))
  const r = new Reader(raw)
  let keep: number
  let added: number[]
  let refs: number
  try {
    const profile = r.u(8)
    r.u(16) // constraint flags, level
    r.ue() // seq_parameter_set_id
    if (HIGH_PROFILES.has(profile)) {
      if (r.ue() === 3) r.u(1) // chroma_format_idc, separate planes
      r.ue() // bit depths
      r.ue()
      r.u(1)
      if (r.u(1)) return null // scaling lists
    }
    r.ue() // log2_max_frame_num_minus4
    if (r.ue() !== 2) return null // pic_order_cnt_type: only 2 cannot reorder
    refs = r.ue()
    r.u(1) // gaps in frame_num
    r.ue() // width and height in macroblocks
    r.ue()
    if (!r.u(1)) r.u(1) // frame_mbs_only, mb_adaptive_frame_field
    r.u(1) // direct_8x8_inference
    if (r.u(1)) {
      r.ue()
      r.ue()
      r.ue()
      r.ue()
    }
    const vuiAt = r.pos
    if (!r.u(1)) {
      // No usability information: a block that holds the restriction alone.
      keep = vuiAt
      added = [1, 0, 0, 0, 0, 0, 0, 0, 0, 1]
    } else {
      if (r.u(1) && r.u(8) === 255) r.u(32) // aspect ratio, extended
      if (r.u(1)) r.u(1) // overscan
      if (r.u(1)) {
        r.u(4) // video format, range
        if (r.u(1)) r.u(24) // colour description
      }
      if (r.u(1)) {
        r.ue() // chroma sample location
        r.ue()
      }
      if (r.u(1)) {
        r.u(32) // timing
        r.u(32)
        r.u(1)
      }
      const nalHrd = r.u(1)
      if (nalHrd) r.hrd()
      const vclHrd = r.u(1)
      if (vclHrd) r.hrd()
      if (nalHrd || vclHrd) r.u(1) // low_delay_hrd
      r.u(1) // pic_struct_present
      const flagAt = r.pos
      if (r.u(1)) return null // the depth is stated
      keep = flagAt
      added = [1]
    }
    if (r.pos > raw.length * 8) return null
  } catch {
    return null
  }

  const w = new Writer()
  for (let i = 0; i < keep; i++) w.bits.push(((raw[i >> 3] ?? 0) >> (7 - (i & 7))) & 1)
  w.bits.push(...added)
  w.bits.push(1) // motion vectors may cross the picture edge
  w.ue(2) // max_bytes_per_pic_denom, the default
  w.ue(1) // max_bits_per_mb_denom, the default
  w.ue(16) // log2_max_mv_length_horizontal
  w.ue(16) // log2_max_mv_length_vertical
  w.ue(0) // max_num_reorder_frames
  w.ue(Math.max(1, refs)) // max_dec_frame_buffering
  w.bits.push(1) // rbsp_stop_one_bit
  while (w.bits.length % 8) w.bits.push(0)
  return new Uint8Array([nal[0]!, ...escape(w.bytes())])
}

class Reader {
  pos = 0
  constructor(private readonly raw: number[]) {}

  u(n: number): number {
    let v = 0
    for (let i = 0; i < n; i++) {
      const byte = this.raw[this.pos >> 3] ?? 0
      v = v * 2 + ((byte >> (7 - (this.pos & 7))) & 1)
      this.pos++
    }
    return v
  }

  ue(): number {
    let zeros = 0
    while (this.u(1) === 0) {
      if (++zeros > 31) throw new Error('exp-Golomb code too long')
    }
    return 2 ** zeros - 1 + this.u(zeros)
  }

  hrd(): void {
    const count = this.ue() + 1
    this.u(8) // bit rate and buffer size scales
    for (let i = 0; i < count; i++) {
      this.ue()
      this.ue()
      this.u(1)
    }
    this.u(20) // four delay and length fields
  }
}

class Writer {
  readonly bits: number[] = []

  ue(v: number): void {
    const n = Math.floor(Math.log2(v + 1)) + 1
    for (let i = 1; i < n; i++) this.bits.push(0)
    for (let i = n - 1; i >= 0; i--) this.bits.push(((v + 1) >> i) & 1)
  }

  bytes(): number[] {
    const out: number[] = []
    for (let i = 0; i < this.bits.length; i += 8) {
      let v = 0
      for (let j = 0; j < 8; j++) v = v * 2 + (this.bits[i + j] ?? 0)
      out.push(v)
    }
    return out
  }
}

// Emulation prevention, both ways: a 03 after two zeros is not payload.
function unescape(b: Uint8Array): number[] {
  const out: number[] = []
  let zeros = 0
  for (const x of b) {
    if (zeros >= 2 && x === 3) {
      zeros = 0
      continue
    }
    out.push(x)
    zeros = x === 0 ? zeros + 1 : 0
  }
  return out
}

function escape(b: number[]): number[] {
  const out: number[] = []
  let zeros = 0
  for (const x of b) {
    if (zeros >= 2 && x <= 3) {
      out.push(3)
      zeros = 0
    }
    out.push(x)
    zeros = x === 0 ? zeros + 1 : 0
  }
  return out
}
