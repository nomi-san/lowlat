//! AES-GCM on the x86-64 AES and carry-less multiply instructions.
//!
//! The portable implementation behind [`crate::envelope`] is correct and it is
//! the reference this module is tested against. It is also about three times
//! slower than the hardware is capable of, because it makes two passes over
//! every packet and reduces the GHASH accumulator once per 16-byte block.
//! This module makes one pass and reduces once per eight blocks.
//!
//! **Selection is a runtime decision, never a build flag.** The shipped binary
//! must run on a machine without these instructions, so the dispatch reads
//! CPUID and falls back. Building the whole daemon with `-C target-cpu` would
//! buy less and cost portability, which is the trade this module exists to
//! avoid.
//!
//! Detection is raw CPUID rather than `is_x86_feature_detected!`, which lives
//! in `std`. Both features are plain instruction-set additions with no XSAVE
//! state, so there is no operating system handshake to check: the CPUID bit is
//! the whole answer.

#![cfg(target_arch = "x86_64")]

use core::arch::x86_64::{
    __cpuid, __cpuid_count, __m128i, __m256i, _mm_aesenc_si128, _mm_aesenclast_si128,
    _mm_aeskeygenassist_si128, _mm_clmulepi64_si128, _mm_insert_epi32, _mm_loadu_si128,
    _mm_or_si128, _mm_set_epi8, _mm_setzero_si128, _mm_shuffle_epi8, _mm_shuffle_epi32,
    _mm_slli_epi32, _mm_slli_epi64, _mm_slli_si128, _mm_srli_epi32, _mm_srli_epi64, _mm_srli_si128,
    _mm_storeu_si128, _mm_xor_si128, _mm256_aesenc_epi128, _mm256_aesenclast_epi128,
    _mm256_broadcastsi128_si256, _mm256_castsi128_si256, _mm256_castsi256_si128,
    _mm256_clmulepi64_epi128, _mm256_extracti128_si256, _mm256_inserti128_si256,
    _mm256_loadu_si256, _mm256_setzero_si256, _mm256_shuffle_epi8, _mm256_shuffle_epi32,
    _mm256_storeu_si256, _mm256_xor_si256, _xgetbv,
};

/// Blocks folded into GHASH between reductions, and so also the number of
/// counter blocks encrypted together. Eight is what fits: sixteen xmm
/// registers hold eight keystream blocks plus the round key and the
/// accumulators without spilling.
const GROUP: usize = 8;
/// The same count, as the amount the block counter advances per group.
const GROUP_STEP: u32 = 8;
const _: () = assert!(GROUP_STEP as usize == GROUP);
const BLOCK: usize = 16;
const GROUP_BYTES: usize = GROUP * BLOCK;

/// Whether this processor has AES-NI and PCLMULQDQ.
///
/// CPUID leaf 1, ECX bit 25 (AESNI) and bit 1 (PCLMULQDQ).
pub(crate) fn available() -> bool {
    // CPUID leaf 1 is architecturally defined on every x86-64 part.
    let leaf = __cpuid(1);
    leaf.ecx & (1 << 25) != 0 && leaf.ecx & (1 << 1) != 0
}

/// Whether this processor has the 256-bit VAES and VPCLMULQDQ instructions
/// **and** an operating system that has enabled the register state they use.
///
/// **The second half is not optional and is the difference from
/// [`available`].** AES-NI and PCLMULQDQ carry no processor state, so CPUID is
/// the whole answer for them. AVX does carry state, and a kernel that has not
/// enabled it leaves a processor advertising instructions that fault on first
/// use. XGETBV is the only way to ask.
pub(crate) fn wide_available() -> bool {
    // Leaves 1 and 7 are architecturally defined on every x86-64 part.
    let leaf1 = __cpuid(1);
    // XSAVE has to be enabled by the operating system before XGETBV may be
    // executed at all, so this order matters.
    if leaf1.ecx & (1 << 27) == 0 {
        return false;
    }
    // SAFETY: OSXSAVE is set, which is exactly the precondition for XGETBV.
    let xcr0 = unsafe { _xgetbv(0) };
    // Bit 1 is the SSE state, bit 2 the upper half of the YMM registers.
    if xcr0 & 0b110 != 0b110 {
        return false;
    }
    let leaf7 = __cpuid_count(7, 0);
    let avx2 = leaf7.ebx & (1 << 5) != 0;
    let vaes = leaf7.ecx & (1 << 9) != 0;
    let vpclmul = leaf7.ecx & (1 << 10) != 0;
    avx2 && vaes && vpclmul
}

/// An expanded key schedule, split so no access needs a computed index.
///
/// `middle` is sized for AES-256's thirteen inner rounds; AES-128 uses nine of
/// them and `middle_len` says so.
#[derive(Clone, Copy)]
struct Schedule {
    first: __m128i,
    middle: [__m128i; 13],
    middle_len: usize,
    last: __m128i,
}

/// The same schedule and GHASH powers arranged for the 256-bit path, where
/// one register holds two blocks.
///
/// Round keys are broadcast once here rather than per round: a broadcast
/// inside the round loop would add ten to fourteen instructions to a group
/// that only issues forty.
#[derive(Clone, Copy)]
struct WideKeys {
    first: __m256i,
    middle: [__m256i; 13],
    middle_len: usize,
    last: __m256i,
    /// The powers paired to match the block pairs: `[H^8, H^7]`, `[H^6, H^5]`,
    /// `[H^4, H^3]`, `[H^2, H^1]`.
    hp: [__m256i; GROUP / 2],
}

/// AES-128-GCM or AES-256-GCM with hardware instructions.
#[derive(Clone, Copy)]
pub(crate) struct Aead {
    sched: Schedule,
    /// H^1 through H^8, byte-reflected, for the aggregated GHASH.
    hp: [__m128i; GROUP],
    /// Present when the processor offers the 256-bit instructions. The narrow
    /// path stays compiled in, and is what the wide one is tested against.
    wide: Option<WideKeys>,
}

impl core::fmt::Debug for Aead {
    /// Never prints key material, for the same reason the portable envelope
    /// does not.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("Aead(..)")
    }
}

impl Aead {
    /// Expand `key` (16 or 32 bytes) and precompute the GHASH powers.
    ///
    /// Returns `None` for any other length, and for a processor without the
    /// instructions.
    pub(crate) fn new(key: &[u8]) -> Option<Self> {
        Self::with_tier(key, wide_available())
    }

    /// Whether this instance is on the 256-bit path.
    #[cfg(test)]
    fn is_wide(&self) -> bool {
        self.wide.is_some()
    }

    /// The narrow path only. Exists so the tests can drive both tiers on a
    /// processor that offers the wide one; otherwise landing the wide tier
    /// would silently stop covering the narrow one everywhere it matters.
    #[cfg(test)]
    fn new_narrow(key: &[u8]) -> Option<Self> {
        Self::with_tier(key, false)
    }

    fn with_tier(key: &[u8], wide: bool) -> Option<Self> {
        if !available() {
            return None;
        }
        // SAFETY: `available()` just confirmed AES-NI and PCLMULQDQ, and
        // `wide` is only true when `wide_available()` confirmed the rest.
        unsafe {
            let mut aead = Self::new_unchecked(key)?;
            if wide {
                aead.wide = Some(wide_keys(&aead.sched, &aead.hp));
            }
            Some(aead)
        }
    }

    /// # Safety
    /// AES-NI and PCLMULQDQ must be present.
    #[target_feature(enable = "aes,pclmulqdq,ssse3,sse4.1")]
    unsafe fn new_unchecked(key: &[u8]) -> Option<Self> {
        unsafe {
            let sched = match key.len() {
                16 => expand_128(key)?,
                32 => expand_256(key)?,
                _ => return None,
            };
            // H is the block cipher applied to zero, in GHASH's byte order.
            let h = bswap(encrypt_block(&sched, _mm_setzero_si128()));
            let mut hp = [_mm_setzero_si128(); GROUP];
            let mut prev = h;
            for slot in hp.iter_mut() {
                *slot = prev;
                prev = gmul(prev, h);
            }
            Some(Self {
                sched,
                hp,
                wide: None,
            })
        }
    }

    /// Encrypt `buf` in place and return the tag. 96-bit nonce, no associated
    /// data, which is what the envelope uses.
    pub(crate) fn seal(&self, nonce: &[u8; 12], buf: &mut [u8]) -> [u8; BLOCK] {
        // SAFETY: an `Aead` cannot be constructed without `available()`.
        unsafe { self.seal_inner(nonce, buf) }
    }

    /// Verify the tag and decrypt `buf` in place. Returns `false` if the tag
    /// does not match.
    ///
    /// **`buf` is clobbered either way.** Decryption and authentication share
    /// one pass, exactly as the portable path does, so a caller must discard
    /// the buffer on a false return rather than inspect it.
    #[must_use]
    pub(crate) fn open(&self, nonce: &[u8; 12], buf: &mut [u8], tag: &[u8; BLOCK]) -> bool {
        // SAFETY: an `Aead` cannot be constructed without `available()`.
        unsafe { self.open_inner(nonce, buf, tag) }
    }

    /// # Safety
    /// AES-NI and PCLMULQDQ must be present.
    #[target_feature(enable = "aes,pclmulqdq,ssse3,sse4.1")]
    unsafe fn seal_inner(&self, nonce: &[u8; 12], buf: &mut [u8]) -> [u8; BLOCK] {
        unsafe {
            let base = counter_base(nonce);
            let mask = encrypt_block(&self.sched, with_counter(base, 1));
            let mut ghash = _mm_setzero_si128();
            // Counter 1 is the tag mask; data starts at the next one.
            let mut ctr: u32 = 2;
            let mut off = 0usize;

            while off + GROUP_BYTES <= buf.len() {
                let mut ks = keystream_group(&self.sched, base, ctr);
                let mut c = [_mm_setzero_si128(); GROUP];
                for (k, (slot, cipher)) in ks.iter_mut().zip(c.iter_mut()).enumerate() {
                    let at = buf.as_mut_ptr().add(off + k * BLOCK).cast::<__m128i>();
                    *cipher = _mm_xor_si128(_mm_loadu_si128(at), *slot);
                    _mm_storeu_si128(at, *cipher);
                }
                ghash = ghash_group(&self.hp, ghash, &c);
                ctr = ctr.wrapping_add(GROUP_STEP);
                off += GROUP_BYTES;
            }

            // Up to eight blocks are left, the last possibly partial. Their
            // keystream is generated in one pipelined pass exactly like a full
            // group's: encrypting them one at a time pays full AES latency per
            // block, which made a seven-block message cost more than an
            // eight-block one.
            let rem = buf.len() - off;
            if rem > 0 {
                let n_tail = rem.div_ceil(BLOCK);
                // A whole group's worth of keystream is generated even for a
                // one-block tail. Sizing this to the tail was tried and is
                // slower: a runtime-length slice cannot stay in registers, so
                // every AES round spills, and a 1200-byte packet went from 269
                // to 295 ns. Doing extra work in registers beats doing less
                // work through memory.
                let ks = keystream_group(&self.sched, base, ctr);
                let mut tail = [_mm_setzero_si128(); GROUP];
                for (k, key) in ks.iter().enumerate().take(n_tail) {
                    let at = off + k * BLOCK;
                    let take = (buf.len() - at).min(BLOCK);
                    let cipher = if take == BLOCK {
                        // A whole block moves through registers; only the
                        // short final block needs to go via a padded copy.
                        let p = buf.as_mut_ptr().add(at).cast::<__m128i>();
                        let c = _mm_xor_si128(_mm_loadu_si128(p), *key);
                        _mm_storeu_si128(p, c);
                        c
                    } else {
                        let mut block = [0u8; BLOCK];
                        copy_in(&mut block, buf, at, take);
                        let c = _mm_xor_si128(_mm_loadu_si128(block.as_ptr().cast()), *key);
                        _mm_storeu_si128(block.as_mut_ptr().cast(), c);
                        copy_out(buf, at, &block, take);
                        // GHASH sees the partial block zero-padded, never the
                        // keystream bytes past the message.
                        let mut padded = [0u8; BLOCK];
                        copy_in(&mut padded, &block, 0, take);
                        _mm_loadu_si128(padded.as_ptr().cast())
                    };
                    if let Some(slot) = tail.get_mut(k) {
                        *slot = cipher;
                    }
                    ctr = ctr.wrapping_add(1);
                }
                if let Some(blocks) = tail.get(..n_tail) {
                    ghash = ghash_group(&self.hp, ghash, blocks);
                }
            }

            let mut out = [0u8; BLOCK];
            let tag = _mm_xor_si128(bswap(self.finish(ghash, buf.len())), mask);
            _mm_storeu_si128(out.as_mut_ptr().cast(), tag);
            out
        }
    }

    /// # Safety
    /// AES-NI and PCLMULQDQ must be present.
    #[target_feature(enable = "aes,pclmulqdq,ssse3,sse4.1")]
    unsafe fn open_inner(&self, nonce: &[u8; 12], buf: &mut [u8], tag: &[u8; BLOCK]) -> bool {
        unsafe {
            let base = counter_base(nonce);
            let mask = encrypt_block(&self.sched, with_counter(base, 1));
            let mut ghash = _mm_setzero_si128();
            let mut ctr: u32 = 2;
            let mut off = 0usize;

            if let Some(wk) = &self.wide {
                while off + GROUP_BYTES <= buf.len() {
                    let mut c = [_mm256_setzero_si256(); GROUP / 2];
                    for (k, cipher) in c.iter_mut().enumerate() {
                        *cipher = _mm256_loadu_si256(buf.as_ptr().add(off + k * 2 * BLOCK).cast());
                    }
                    ghash = wide_ghash(wk, ghash, &c);
                    let ks = wide_keystream(wk, base, ctr);
                    for (k, (slot, cipher)) in ks.iter().zip(c.iter()).enumerate() {
                        let at = buf.as_mut_ptr().add(off + k * 2 * BLOCK).cast::<__m256i>();
                        _mm256_storeu_si256(at, _mm256_xor_si256(*cipher, *slot));
                    }
                    ctr = ctr.wrapping_add(GROUP_STEP);
                    off += GROUP_BYTES;
                }
            }

            while off + GROUP_BYTES <= buf.len() {
                let mut c = [_mm_setzero_si128(); GROUP];
                for (k, cipher) in c.iter_mut().enumerate() {
                    *cipher = _mm_loadu_si128(buf.as_ptr().add(off + k * BLOCK).cast());
                }
                // GHASH covers the ciphertext, so it is read before the
                // keystream lands on it.
                ghash = ghash_group(&self.hp, ghash, &c);
                let ks = keystream_group(&self.sched, base, ctr);
                for (k, (slot, cipher)) in ks.iter().zip(c.iter()).enumerate() {
                    let at = buf.as_mut_ptr().add(off + k * BLOCK).cast::<__m128i>();
                    _mm_storeu_si128(at, _mm_xor_si128(*cipher, *slot));
                }
                ctr = ctr.wrapping_add(GROUP_STEP);
                off += GROUP_BYTES;
            }

            // Same pipelined tail as the sealing side, with GHASH taking the
            // ciphertext before the keystream lands on it.
            let rem = buf.len() - off;
            if rem > 0 {
                let n_tail = rem.div_ceil(BLOCK);
                // A whole group's worth of keystream is generated even for a
                // one-block tail. Sizing this to the tail was tried and is
                // slower: a runtime-length slice cannot stay in registers, so
                // every AES round spills, and a 1200-byte packet went from 269
                // to 295 ns. Doing extra work in registers beats doing less
                // work through memory.
                let ks = keystream_group(&self.sched, base, ctr);
                let mut tail = [_mm_setzero_si128(); GROUP];
                for (k, key) in ks.iter().enumerate().take(n_tail) {
                    let at = off + k * BLOCK;
                    let take = (buf.len() - at).min(BLOCK);
                    if take == BLOCK {
                        let p = buf.as_mut_ptr().add(at).cast::<__m128i>();
                        let cipher = _mm_loadu_si128(p);
                        if let Some(slot) = tail.get_mut(k) {
                            *slot = cipher;
                        }
                        _mm_storeu_si128(p, _mm_xor_si128(cipher, *key));
                    } else {
                        let mut padded = [0u8; BLOCK];
                        copy_in(&mut padded, buf, at, take);
                        let cipher = _mm_loadu_si128(padded.as_ptr().cast());
                        if let Some(slot) = tail.get_mut(k) {
                            *slot = cipher;
                        }
                        let mut block = [0u8; BLOCK];
                        _mm_storeu_si128(block.as_mut_ptr().cast(), _mm_xor_si128(cipher, *key));
                        copy_out(buf, at, &block, take);
                    }
                    ctr = ctr.wrapping_add(1);
                }
                if let Some(blocks) = tail.get(..n_tail) {
                    ghash = ghash_group(&self.hp, ghash, blocks);
                }
            }

            let mut want = [0u8; BLOCK];
            let computed = _mm_xor_si128(bswap(self.finish(ghash, buf.len())), mask);
            _mm_storeu_si128(want.as_mut_ptr().cast(), computed);
            eq_ct(&want, tag)
        }
    }

    /// Fold the length block in. Associated data is empty, so its bit count is
    /// zero and only the ciphertext length is carried.
    ///
    /// # Safety
    /// AES-NI and PCLMULQDQ must be present.
    #[target_feature(enable = "pclmulqdq,ssse3,sse4.1")]
    unsafe fn finish(&self, ghash: __m128i, len: usize) -> __m128i {
        unsafe {
            let bits = (len as u64).wrapping_mul(8);
            let mut lens = [0u8; BLOCK];
            if let Some(half) = lens.get_mut(8..) {
                half.copy_from_slice(&bits.to_be_bytes());
            }
            let block = bswap(_mm_loadu_si128(lens.as_ptr().cast()));
            let h = self.hp.first().copied().unwrap_or(_mm_setzero_si128());
            gmul(_mm_xor_si128(ghash, block), h)
        }
    }
}

/// Constant-time tag comparison. A tag check that returns early leaks where
/// the first difference is, which is enough to forge one byte at a time.
fn eq_ct(a: &[u8; BLOCK], b: &[u8; BLOCK]) -> bool {
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn copy_in(dst: &mut [u8; BLOCK], src: &[u8], off: usize, take: usize) {
    if let (Some(d), Some(s)) = (dst.get_mut(..take), src.get(off..off + take)) {
        d.copy_from_slice(s);
    }
}

fn copy_out(dst: &mut [u8], off: usize, src: &[u8; BLOCK], take: usize) {
    if let (Some(d), Some(s)) = (dst.get_mut(off..off + take), src.get(..take)) {
        d.copy_from_slice(s);
    }
}

// ---- the 256-bit path ----

/// Build the wide schedule and the paired GHASH powers.
///
/// # Safety
/// AVX2, VAES and VPCLMULQDQ must be present.
#[target_feature(enable = "avx2,vaes,vpclmulqdq")]
unsafe fn wide_keys(s: &Schedule, hp: &[__m128i; GROUP]) -> WideKeys {
    unsafe {
        let mut middle = [_mm256_setzero_si256(); 13];
        for (dst, src) in middle.iter_mut().zip(s.middle.iter()) {
            *dst = _mm256_broadcastsi128_si256(*src);
        }
        // Pair k carries blocks 2k and 2k+1, so it needs H^(8-2k) in the low
        // lane and H^(7-2k) in the high one. `hp[i]` is H^(i+1).
        let mut paired = [_mm256_setzero_si256(); GROUP / 2];
        for (k, slot) in paired.iter_mut().enumerate() {
            let low = hp.get(7 - 2 * k).copied().unwrap_or(_mm_setzero_si128());
            let high = hp.get(6 - 2 * k).copied().unwrap_or(_mm_setzero_si128());
            *slot = lanes(low, high);
        }
        WideKeys {
            first: _mm256_broadcastsi128_si256(s.first),
            middle,
            middle_len: s.middle_len,
            last: _mm256_broadcastsi128_si256(s.last),
            hp: paired,
        }
    }
}

/// Two 128-bit values into one register, `low` in lane zero.
///
/// The cast alone leaves the upper lane undefined, which is a real hazard when
/// the upper lane is meant to be zero, so it is always written explicitly.
///
/// # Safety
/// AVX2 must be present.
#[target_feature(enable = "avx2")]
unsafe fn lanes(low: __m128i, high: __m128i) -> __m256i {
    _mm256_inserti128_si256::<1>(_mm256_castsi128_si256(low), high)
}

/// Fold a register's two lanes together. GHASH sums with xor, so the two
/// halves of an accumulator combine the same way its terms did.
///
/// # Safety
/// AVX2 must be present.
#[target_feature(enable = "avx2")]
unsafe fn fold_lanes(v: __m256i) -> __m128i {
    _mm_xor_si128(_mm256_castsi256_si128(v), _mm256_extracti128_si256::<1>(v))
}

/// Eight counter blocks as four registers, two blocks per instruction.
///
/// # Safety
/// AVX2, VAES and SSE4.1 must be present.
#[target_feature(enable = "avx2,vaes,sse4.1")]
unsafe fn wide_keystream(wk: &WideKeys, base: __m128i, ctr: u32) -> [__m256i; GROUP / 2] {
    unsafe {
        let mut b = [_mm256_setzero_si256(); GROUP / 2];
        for (pair, slot) in (0u32..).zip(b.iter_mut()) {
            let low = with_counter(base, ctr.wrapping_add(pair.wrapping_mul(2)));
            let high = with_counter(base, ctr.wrapping_add(pair.wrapping_mul(2).wrapping_add(1)));
            *slot = _mm256_xor_si256(lanes(low, high), wk.first);
        }
        for key in wk.middle.iter().take(wk.middle_len) {
            for slot in b.iter_mut() {
                *slot = _mm256_aesenc_epi128(*slot, *key);
            }
        }
        for slot in b.iter_mut() {
            *slot = _mm256_aesenclast_epi128(*slot, wk.last);
        }
        b
    }
}

/// The byte reversal GHASH needs, applied within each lane.
///
/// # Safety
/// AVX2 must be present.
#[target_feature(enable = "avx2")]
unsafe fn wide_bswap(x: __m256i) -> __m256i {
    let mask = _mm_set_epi8(0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15);
    _mm256_shuffle_epi8(x, _mm256_broadcastsi128_si256(mask))
}

/// Karatsuba on both lanes at once, partial products left unassembled.
///
/// # Safety
/// AVX2 and VPCLMULQDQ must be present.
#[target_feature(enable = "avx2,vpclmulqdq")]
unsafe fn wide_clmul(a: __m256i, b: __m256i) -> (__m256i, __m256i, __m256i) {
    let fold_a = _mm256_xor_si256(a, _mm256_shuffle_epi32::<0x4e>(a));
    let fold_b = _mm256_xor_si256(b, _mm256_shuffle_epi32::<0x4e>(b));
    (
        _mm256_clmulepi64_epi128::<0x00>(a, b),
        _mm256_clmulepi64_epi128::<0x00>(fold_a, fold_b),
        _mm256_clmulepi64_epi128::<0x11>(a, b),
    )
}

/// The eight-block GHASH step, four multiplies instead of eight.
///
/// Assembly and reduction stay 128-bit: both are linear over xor, so folding
/// the lanes first and assembling once gives the same answer as assembling
/// each lane, and it costs one reduction rather than two.
///
/// # Safety
/// AVX2, VPCLMULQDQ and SSE2 must be present.
#[target_feature(enable = "avx2,vpclmulqdq,sse2")]
unsafe fn wide_ghash(wk: &WideKeys, state: __m128i, c: &[__m256i; GROUP / 2]) -> __m128i {
    unsafe {
        let mut lo = _mm256_setzero_si256();
        let mut mid = _mm256_setzero_si256();
        let mut hi = _mm256_setzero_si256();
        for (k, (pair, power)) in c.iter().zip(wk.hp.iter()).enumerate() {
            let mut x = wide_bswap(*pair);
            if k == 0 {
                // The accumulator folds into the first block only, which is
                // the low lane of the first pair.
                x = _mm256_xor_si256(x, lanes(state, _mm_setzero_si128()));
            }
            let (l, m, h) = wide_clmul(x, *power);
            lo = _mm256_xor_si256(lo, l);
            mid = _mm256_xor_si256(mid, m);
            hi = _mm256_xor_si256(hi, h);
        }
        let (lo, hi) = assemble(fold_lanes(lo), fold_lanes(mid), fold_lanes(hi));
        reduce(lo, hi)
    }
}

// ---- AES ----

/// # Safety
/// AES-NI must be present.
#[target_feature(enable = "aes,sse2")]
unsafe fn encrypt_block(s: &Schedule, block: __m128i) -> __m128i {
    let mut b = _mm_xor_si128(block, s.first);
    for key in s.middle.iter().take(s.middle_len) {
        b = _mm_aesenc_si128(b, *key);
    }
    _mm_aesenclast_si128(b, s.last)
}

/// Encrypt eight counter blocks together. The eight are independent, so the
/// AES pipeline stays full instead of paying full latency per block.
///
/// # Safety
/// AES-NI must be present.
#[target_feature(enable = "aes,sse4.1")]
unsafe fn keystream_group(s: &Schedule, base: __m128i, ctr: u32) -> [__m128i; GROUP] {
    unsafe {
        let mut b = [_mm_setzero_si128(); GROUP];
        for (step, slot) in (0u32..).zip(b.iter_mut()) {
            *slot = _mm_xor_si128(with_counter(base, ctr.wrapping_add(step)), s.first);
        }
        for key in s.middle.iter().take(s.middle_len) {
            for slot in b.iter_mut() {
                *slot = _mm_aesenc_si128(*slot, *key);
            }
        }
        for slot in b.iter_mut() {
            *slot = _mm_aesenclast_si128(*slot, s.last);
        }
        b
    }
}

/// The counter block with its counter field cleared. Splicing the counter in
/// per block is one instruction; rebuilding the block through a stack array
/// costs a store-to-load round trip on every one of them.
///
/// # Safety
/// SSE2 must be present.
#[target_feature(enable = "sse2")]
unsafe fn counter_base(nonce: &[u8; 12]) -> __m128i {
    unsafe {
        let mut b = [0u8; BLOCK];
        if let Some(head) = b.get_mut(..12) {
            head.copy_from_slice(nonce);
        }
        _mm_loadu_si128(b.as_ptr().cast())
    }
}

/// # Safety
/// SSE4.1 must be present.
#[target_feature(enable = "sse4.1")]
unsafe fn with_counter(base: __m128i, ctr: u32) -> __m128i {
    // The counter occupies the last four bytes of the block, big-endian. The
    // lane is written as a machine integer, so the big-endian *bytes* are
    // reinterpreted little-endian to land in that order in memory. Reading
    // them back big-endian instead yields the value again, which stores
    // reversed and silently produces a valid-looking stream that no peer can
    // decrypt.
    _mm_insert_epi32::<3>(base, i32::from_le_bytes(ctr.to_be_bytes()))
}

/// # Safety
/// AES-NI must be present.
#[target_feature(enable = "aes,sse2")]
unsafe fn expand_128(key: &[u8]) -> Option<Schedule> {
    unsafe {
        let mut k = [_mm_setzero_si128(); 11];
        *k.first_mut()? = _mm_loadu_si128(key.as_ptr().cast());
        macro_rules! step {
            ($i:expr, $rcon:expr) => {{
                let prev = *k.get($i - 1)?;
                let t = _mm_shuffle_epi32::<0xff>(_mm_aeskeygenassist_si128::<$rcon>(prev));
                *k.get_mut($i)? = _mm_xor_si128(smear(prev), t);
            }};
        }
        step!(1, 0x01);
        step!(2, 0x02);
        step!(3, 0x04);
        step!(4, 0x08);
        step!(5, 0x10);
        step!(6, 0x20);
        step!(7, 0x40);
        step!(8, 0x80);
        step!(9, 0x1b);
        step!(10, 0x36);
        schedule_from(&k, 9)
    }
}

/// # Safety
/// AES-NI must be present.
#[target_feature(enable = "aes,sse2")]
unsafe fn expand_256(key: &[u8]) -> Option<Schedule> {
    unsafe {
        let mut k = [_mm_setzero_si128(); 15];
        *k.first_mut()? = _mm_loadu_si128(key.as_ptr().cast());
        *k.get_mut(1)? = _mm_loadu_si128(key.as_ptr().add(BLOCK).cast());
        // AES-256 alternates: even steps take a round constant and the high
        // word, odd steps take no constant and the third word.
        macro_rules! even {
            ($i:expr, $rcon:expr) => {{
                let t =
                    _mm_shuffle_epi32::<0xff>(_mm_aeskeygenassist_si128::<$rcon>(*k.get($i - 1)?));
                *k.get_mut($i)? = _mm_xor_si128(smear(*k.get($i - 2)?), t);
            }};
        }
        macro_rules! odd {
            ($i:expr) => {{
                let t =
                    _mm_shuffle_epi32::<0xaa>(_mm_aeskeygenassist_si128::<0x00>(*k.get($i - 1)?));
                *k.get_mut($i)? = _mm_xor_si128(smear(*k.get($i - 2)?), t);
            }};
        }
        even!(2, 0x01);
        odd!(3);
        even!(4, 0x02);
        odd!(5);
        even!(6, 0x04);
        odd!(7);
        even!(8, 0x08);
        odd!(9);
        even!(10, 0x10);
        odd!(11);
        even!(12, 0x20);
        odd!(13);
        even!(14, 0x40);
        schedule_from(&k, 13)
    }
}

/// The xor-with-shifted-self that every key expansion step shares.
///
/// # Safety
/// SSE2 must be present.
#[target_feature(enable = "sse2")]
unsafe fn smear(mut k: __m128i) -> __m128i {
    k = _mm_xor_si128(k, _mm_slli_si128::<4>(k));
    k = _mm_xor_si128(k, _mm_slli_si128::<4>(k));
    _mm_xor_si128(k, _mm_slli_si128::<4>(k))
}

fn schedule_from(keys: &[__m128i], middle_len: usize) -> Option<Schedule> {
    let mut middle = [unsafe { _mm_setzero_si128() }; 13];
    for (slot, key) in middle.iter_mut().zip(keys.get(1..1 + middle_len)?) {
        *slot = *key;
    }
    Some(Schedule {
        first: *keys.first()?,
        middle,
        middle_len,
        last: *keys.get(1 + middle_len)?,
    })
}

// ---- GHASH ----

/// GHASH works on bit-reflected values, which for these instructions means the
/// block is byte-reversed on the way in and on the way out.
///
/// # Safety
/// SSSE3 must be present.
#[target_feature(enable = "ssse3")]
unsafe fn bswap(x: __m128i) -> __m128i {
    let mask = _mm_set_epi8(0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15);
    _mm_shuffle_epi8(x, mask)
}

/// Carry-less 128x128, Karatsuba, returning the three partial products
/// unassembled.
///
/// Three multiplies rather than schoolbook's four. The carry-less multiply
/// issues on one port and the AES rounds on another, so on a packet this size
/// the multiply count is the binding constraint and dropping a quarter of them
/// is worth the two shuffles it costs.
///
/// The caller assembles them, because assembly is linear over xor and can
/// therefore be deferred to the end of a whole group.
///
/// # Safety
/// PCLMULQDQ must be present.
#[target_feature(enable = "pclmulqdq,sse2")]
unsafe fn clmul_wide(a: __m128i, b: __m128i) -> (__m128i, __m128i, __m128i) {
    let fold_a = _mm_xor_si128(a, _mm_shuffle_epi32::<0x4e>(a));
    let fold_b = _mm_xor_si128(b, _mm_shuffle_epi32::<0x4e>(b));
    (
        _mm_clmulepi64_si128::<0x00>(a, b),
        _mm_clmulepi64_si128::<0x00>(fold_a, fold_b),
        _mm_clmulepi64_si128::<0x11>(a, b),
    )
}

/// Assemble summed Karatsuba partials into the 256-bit product.
///
/// # Safety
/// SSE2 must be present.
#[target_feature(enable = "sse2")]
unsafe fn assemble(lo: __m128i, mid: __m128i, hi: __m128i) -> (__m128i, __m128i) {
    let mid = _mm_xor_si128(mid, _mm_xor_si128(lo, hi));
    (
        _mm_xor_si128(lo, _mm_slli_si128::<8>(mid)),
        _mm_xor_si128(hi, _mm_srli_si128::<8>(mid)),
    )
}

/// Shift the 256-bit product left by one and reduce modulo the GCM polynomial.
///
/// Both steps are linear over xor, which is the property that lets eight
/// blocks be multiplied, summed, and then reduced once. That deferral is where
/// most of the speedup lives; a per-block reduction is what the portable
/// implementation spends its time on.
///
/// # Safety
/// SSE2 must be present.
#[target_feature(enable = "sse2")]
unsafe fn reduce(lo: __m128i, hi: __m128i) -> __m128i {
    let carry_lo = _mm_srli_epi64::<63>(lo);
    let carry_hi = _mm_srli_epi64::<63>(hi);
    let lo = _mm_or_si128(_mm_slli_epi64::<1>(lo), _mm_slli_si128::<8>(carry_lo));
    let hi = _mm_or_si128(
        _mm_or_si128(_mm_slli_epi64::<1>(hi), _mm_slli_si128::<8>(carry_hi)),
        _mm_srli_si128::<8>(carry_lo),
    );
    let folded = _mm_xor_si128(
        _mm_xor_si128(_mm_slli_epi32::<31>(lo), _mm_slli_epi32::<30>(lo)),
        _mm_slli_epi32::<25>(lo),
    );
    let spill = _mm_srli_si128::<4>(folded);
    let lo = _mm_xor_si128(lo, _mm_slli_si128::<12>(folded));
    let tail = _mm_xor_si128(
        _mm_xor_si128(_mm_srli_epi32::<1>(lo), _mm_srli_epi32::<2>(lo)),
        _mm_xor_si128(_mm_srli_epi32::<7>(lo), spill),
    );
    _mm_xor_si128(hi, _mm_xor_si128(lo, tail))
}

/// # Safety
/// PCLMULQDQ must be present.
#[target_feature(enable = "pclmulqdq,sse2")]
unsafe fn gmul(a: __m128i, b: __m128i) -> __m128i {
    unsafe {
        let (lo, mid, hi) = clmul_wide(a, b);
        let (lo, hi) = assemble(lo, mid, hi);
        reduce(lo, hi)
    }
}

/// `state = (state ^ C0)*H^n ^ C1*H^(n-1) ^ ... ^ C(n-1)*H^1`, reduced once.
///
/// The blocks pair with descending powers of H, which is what `rev` is doing:
/// the oldest block has been multiplied by H the most times.
///
/// # Safety
/// PCLMULQDQ and SSSE3 must be present.
#[target_feature(enable = "pclmulqdq,ssse3,sse2")]
unsafe fn ghash_group(hp: &[__m128i; GROUP], state: __m128i, blocks: &[__m128i]) -> __m128i {
    unsafe {
        debug_assert!(!blocks.is_empty() && blocks.len() <= GROUP);
        let mut lo = _mm_setzero_si128();
        let mut mid = _mm_setzero_si128();
        let mut hi = _mm_setzero_si128();
        let powers = hp.iter().take(blocks.len()).rev();
        for (k, (block, power)) in blocks.iter().zip(powers).enumerate() {
            let mut x = bswap(*block);
            if k == 0 {
                x = _mm_xor_si128(x, state);
            }
            let (l, m, h) = clmul_wide(x, *power);
            lo = _mm_xor_si128(lo, l);
            mid = _mm_xor_si128(mid, m);
            hi = _mm_xor_si128(hi, h);
        }
        // One assembly and one reduction for the whole group.
        let (lo, hi) = assemble(lo, mid, hi);
        reduce(lo, hi)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aes_gcm::aead::AeadInPlace;
    use aes_gcm::aead::generic_array::GenericArray;
    use aes_gcm::{Aes128Gcm, Aes256Gcm, KeyInit};
    use std::vec;
    use std::vec::Vec;

    /// Lengths chosen for the boundaries this code actually has: empty, under
    /// one block, exactly one block, a real packet at the PMTU floor, and the
    /// eight-block group boundary from both sides.
    ///
    /// **Every possible tail size appears.** The tail runs a different path
    /// from a whole group, so 80, 96 and 112 bytes are here to give it five,
    /// six and seven whole blocks, and 113 and 127 give it eight with a
    /// partial one on the end.
    const LENGTHS: [usize; 18] = [
        0, 1, 15, 16, 17, 31, 80, 96, 112, 113, 127, 128, 129, 1200, 1201, 2047, 2048, 4096,
    ];

    fn material(seed: u64, len: usize) -> Vec<u8> {
        let mut s = seed;
        (0..len)
            .map(|_| {
                s = s
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                (s >> 33) as u8
            })
            .collect()
    }

    /// The portable implementation's answer, which is the reference.
    fn reference(key: &[u8], nonce: &[u8; 12], buf: &mut [u8]) -> [u8; 16] {
        let n = GenericArray::from_slice(nonce);
        let tag = if key.len() == 16 {
            Aes128Gcm::new(GenericArray::from_slice(key))
                .encrypt_in_place_detached(n, &[], buf)
                .unwrap()
        } else {
            Aes256Gcm::new(GenericArray::from_slice(key))
                .encrypt_in_place_detached(n, &[], buf)
                .unwrap()
        };
        tag.into()
    }

    /// Every tier this processor can run, so a machine with the wide
    /// instructions still exercises the narrow path.
    fn tiers(key: &[u8]) -> Vec<(&'static str, Aead)> {
        let mut v = vec![("narrow", Aead::new_narrow(key).expect("hardware AES"))];
        if wide_available() {
            v.push(("wide", Aead::new(key).expect("hardware AES")));
        }
        v
    }

    #[test]
    fn matches_the_portable_implementation_byte_for_byte() {
        // The whole argument for this module is that it is indistinguishable
        // from the path it replaces. Anything less and a peer cannot decrypt.
        for key_len in [16usize, 32] {
            for (i, len) in LENGTHS.iter().enumerate() {
                let key = material(0x5eed ^ key_len as u64, key_len);
                let nonce: [u8; 12] = material(0xbeef ^ i as u64, 12).try_into().unwrap();
                let plain = material(0xfade ^ i as u64, *len);

                let hw = Aead::new(&key).expect("hardware AES");
                let mut ours = plain.clone();
                let our_tag = hw.seal(&nonce, &mut ours);

                let mut theirs = plain.clone();
                let their_tag = reference(&key, &nonce, &mut theirs);

                assert_eq!(
                    ours, theirs,
                    "ciphertext differs at len {len}, key {key_len}"
                );
                assert_eq!(
                    our_tag, their_tag,
                    "tag differs at len {len}, key {key_len}"
                );
            }
        }
    }

    #[test]
    fn what_it_seals_the_portable_implementation_opens() {
        // The other direction of the same property: a peer running the
        // portable path must accept what this one produced.
        for key_len in [16usize, 32] {
            let key = material(0xa11ce, key_len);
            let nonce: [u8; 12] = material(0xb0b, 12).try_into().unwrap();
            let plain = material(0xc0ffee, 1200);

            let mut sealed = plain.clone();
            let tag = Aead::new(&key).unwrap().seal(&nonce, &mut sealed);

            let n = GenericArray::from_slice(&nonce);
            let t = GenericArray::from_slice(&tag);
            if key_len == 16 {
                Aes128Gcm::new(GenericArray::from_slice(&key))
                    .decrypt_in_place_detached(n, &[], &mut sealed, t)
                    .expect("portable open");
            } else {
                Aes256Gcm::new(GenericArray::from_slice(&key))
                    .decrypt_in_place_detached(n, &[], &mut sealed, t)
                    .expect("portable open");
            }
            assert_eq!(sealed, plain);
        }
    }

    #[test]
    fn opens_what_it_sealed() {
        for key_len in [16usize, 32] {
            for len in LENGTHS {
                let key = material(0xd00d ^ key_len as u64, key_len);
                let nonce: [u8; 12] = material(0xfeed ^ len as u64, 12).try_into().unwrap();
                let plain = material(0xbead ^ len as u64, len);

                // Every tier opens every other tier's output. A session does
                // not know which one the peer is running, so this is the
                // property that actually has to hold.
                for (sealer, s) in tiers(&key) {
                    let mut sealed = plain.clone();
                    let tag = s.seal(&nonce, &mut sealed);
                    for (opener, o) in tiers(&key) {
                        let mut buf = sealed.clone();
                        assert!(
                            o.open(&nonce, &mut buf, &tag),
                            "{opener} could not open {sealer} at len {len}"
                        );
                        assert_eq!(buf, plain, "{opener}/{sealer} differs at len {len}");
                    }
                }
            }
        }
    }

    #[test]
    fn the_wide_tier_is_taken_when_the_processor_offers_it() {
        // Without this, breaking the selection fails nothing: both tiers
        // produce identical bytes, so every other test passes either way.
        let key = material(0xfab, 32);
        assert_eq!(Aead::new(&key).unwrap().is_wide(), wide_available());
        assert!(!Aead::new_narrow(&key).unwrap().is_wide());
    }

    #[test]
    fn a_flipped_ciphertext_bit_is_refused() {
        let key = material(1, 32);
        let nonce = [3u8; 12];
        let hw = Aead::new(&key).unwrap();
        // Every position matters, not just the first block: a GHASH that drops
        // the tail would still authenticate the head.
        for at in [0usize, 15, 16, 127, 128, 1199] {
            let mut buf = material(2, 1200);
            let tag = hw.seal(&nonce, &mut buf);
            buf[at] ^= 1;
            assert!(!hw.open(&nonce, &mut buf, &tag), "accepted a flip at {at}");
        }
    }

    #[test]
    fn a_flipped_tag_bit_is_refused() {
        let key = material(4, 16);
        let nonce = [5u8; 12];
        let hw = Aead::new(&key).unwrap();
        for at in 0..16 {
            let mut buf = material(6, 1200);
            let mut tag = hw.seal(&nonce, &mut buf);
            tag[at] ^= 0x80;
            assert!(
                !hw.open(&nonce, &mut buf, &tag),
                "accepted a tag flip at {at}"
            );
        }
    }

    #[test]
    fn the_wrong_nonce_is_refused() {
        // The counter is spliced into the last dword of the nonce block, so a
        // nonce that differs only in its last byte is the case that would
        // survive a splice built at the wrong offset.
        let key = material(7, 32);
        let hw = Aead::new(&key).unwrap();
        let mut buf = material(8, 1200);
        let tag = hw.seal(&[9u8; 12], &mut buf);
        let mut other = [9u8; 12];
        other[11] ^= 1;
        assert!(!hw.open(&other, &mut buf, &tag));
    }

    #[test]
    fn only_the_two_key_lengths_are_accepted() {
        for len in [0usize, 1, 15, 17, 24, 31, 33, 64] {
            assert!(
                Aead::new(&material(10, len)).is_none(),
                "accepted {len} bytes"
            );
        }
        assert!(Aead::new(&material(10, 16)).is_some());
        assert!(Aead::new(&material(10, 32)).is_some());
    }

    #[test]
    fn the_tag_comparison_reads_every_byte() {
        // A comparison that stops at the first difference leaks where it is,
        // which is enough to forge a tag one byte at a time.
        let a = [0u8; 16];
        for at in 0..16 {
            let mut b = [0u8; 16];
            b[at] = 1;
            assert!(!eq_ct(&a, &b), "missed a difference at {at}");
        }
        assert!(eq_ct(&a, &[0u8; 16]));
    }

    #[test]
    fn key_material_does_not_reach_the_debug_output() {
        let hw = Aead::new(&material(11, 32)).unwrap();
        assert_eq!(std::format!("{hw:?}"), "Aead(..)");
    }
}

#[cfg(test)]
mod fips {
    use super::*;

    #[test]
    fn aes128_matches_fips197() {
        let key: [u8; 16] = [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
            0x0e, 0x0f,
        ];
        let pt: [u8; 16] = [
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff,
        ];
        let want: [u8; 16] = [
            0x69, 0xc4, 0xe0, 0xd8, 0x6a, 0x7b, 0x04, 0x30, 0xd8, 0xcd, 0xb7, 0x80, 0x70, 0xb4,
            0xc5, 0x5a,
        ];
        let mut got = [0u8; 16];
        unsafe {
            let s = expand_128(&key).unwrap();
            let c = encrypt_block(&s, _mm_loadu_si128(pt.as_ptr().cast()));
            _mm_storeu_si128(got.as_mut_ptr().cast(), c);
        }
        assert_eq!(got, want);
    }

    /// NIST GCM test case 1: AES-128, zero key, zero IV, empty message.
    /// The tag is the whole output, so this isolates J0 and the length block
    /// from everything else.
    #[test]
    fn gcm_empty_message_matches_the_published_vector() {
        let want: [u8; 16] = [
            0x58, 0xe2, 0xfc, 0xce, 0xfa, 0x7e, 0x30, 0x61, 0x36, 0x7f, 0x1d, 0x57, 0xa4, 0xe7,
            0x45, 0x5a,
        ];
        let got = Aead::new(&[0u8; 16]).unwrap().seal(&[0u8; 12], &mut []);
        assert_eq!(got, want);
    }

    /// NIST GCM test case 2: same key and IV, one all-zero block of plaintext.
    #[test]
    fn gcm_one_block_matches_the_published_vector() {
        let want_ct: [u8; 16] = [
            0x03, 0x88, 0xda, 0xce, 0x60, 0xb6, 0xa3, 0x92, 0xf3, 0x28, 0xc2, 0xb9, 0x71, 0xb2,
            0xfe, 0x78,
        ];
        let want_tag: [u8; 16] = [
            0xab, 0x6e, 0x47, 0xd4, 0x2c, 0xec, 0x13, 0xbd, 0xf5, 0x3a, 0x67, 0xb2, 0x12, 0x57,
            0xbd, 0xdf,
        ];
        let mut buf = [0u8; 16];
        let tag = Aead::new(&[0u8; 16]).unwrap().seal(&[0u8; 12], &mut buf);
        assert_eq!(buf, want_ct, "ciphertext");
        assert_eq!(tag, want_tag, "tag");
    }

    #[test]
    fn aes256_matches_fips197() {
        let key: [u8; 32] = [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
            0x0e, 0x0f, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b,
            0x1c, 0x1d, 0x1e, 0x1f,
        ];
        let pt: [u8; 16] = [
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff,
        ];
        let want: [u8; 16] = [
            0x8e, 0xa2, 0xb7, 0xca, 0x51, 0x67, 0x45, 0xbf, 0xea, 0xfc, 0x49, 0x90, 0x4b, 0x49,
            0x60, 0x89,
        ];
        let mut got = [0u8; 16];
        unsafe {
            let s = expand_256(&key).unwrap();
            let c = encrypt_block(&s, _mm_loadu_si128(pt.as_ptr().cast()));
            _mm_storeu_si128(got.as_mut_ptr().cast(), c);
        }
        assert_eq!(got, want);
    }
}
