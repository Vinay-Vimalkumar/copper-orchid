//! Classify commas, quotes and line endings in 64-byte blocks. Every comma
//! separates fields. Quotes are legal only in pairs around whole fields.

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
struct Masks {
    comma: u64,
    quote: u64,
    nl: u64,
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
#[inline(always)]
fn classify_scalar(block: &[u8]) -> Masks {
    let mut m = Masks::default();
    for (i, &b) in block.iter().enumerate().take(64) {
        m.comma |= u64::from(b == b',') << i;
        m.quote |= u64::from(b == b'"') << i;
        m.nl |= u64::from(b == b'\n') << i;
    }
    m
}

#[cfg(target_arch = "aarch64")]
#[inline(always)]
unsafe fn classify_neon(p: *const u8) -> Masks {
    use core::arch::aarch64::*;
    // SAFETY: the caller guarantees 64 readable bytes at p; NEON is baseline on aarch64.
    unsafe {
        let v0 = vld1q_u8(p);
        let v1 = vld1q_u8(p.add(16));
        let v2 = vld1q_u8(p.add(32));
        let v3 = vld1q_u8(p.add(48));
        const BITS: [u8; 16] = [1, 2, 4, 8, 16, 32, 64, 128, 1, 2, 4, 8, 16, 32, 64, 128];
        let bits = vld1q_u8(BITS.as_ptr());
        macro_rules! movemask {
            ($c:expr) => {{
                let c = vdupq_n_u8($c);
                let t0 = vandq_u8(vceqq_u8(v0, c), bits);
                let t1 = vandq_u8(vceqq_u8(v1, c), bits);
                let t2 = vandq_u8(vceqq_u8(v2, c), bits);
                let t3 = vandq_u8(vceqq_u8(v3, c), bits);
                let s = vpaddq_u8(vpaddq_u8(t0, t1), vpaddq_u8(t2, t3));
                let s = vpaddq_u8(s, s);
                vgetq_lane_u64::<0>(vreinterpretq_u64_u8(s))
            }};
        }
        Masks {
            comma: movemask!(b','),
            quote: movemask!(b'"'),
            nl: movemask!(b'\n'),
        }
    }
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
unsafe fn classify_sse2(p: *const u8) -> Masks {
    use core::arch::x86_64::*;
    // SAFETY: the caller guarantees 64 readable bytes at p; SSE2 is baseline on x86_64.
    unsafe {
        let v = [
            _mm_loadu_si128(p as *const __m128i),
            _mm_loadu_si128(p.add(16) as *const __m128i),
            _mm_loadu_si128(p.add(32) as *const __m128i),
            _mm_loadu_si128(p.add(48) as *const __m128i),
        ];
        macro_rules! movemask {
            ($c:expr) => {{
                let c = _mm_set1_epi8($c as i8);
                let mut m = 0u64;
                for (i, x) in v.iter().enumerate() {
                    m |= (_mm_movemask_epi8(_mm_cmpeq_epi8(*x, c)) as u32 as u64) << (16 * i);
                }
                m
            }};
        }
        Masks {
            comma: movemask!(b','),
            quote: movemask!(b'"'),
            nl: movemask!(b'\n'),
        }
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
#[inline]
unsafe fn classify_avx2(p: *const u8) -> Masks {
    use core::arch::x86_64::*;
    // SAFETY: the caller guarantees 64 readable bytes at p and checked AVX2 support.
    unsafe {
        let lo = _mm256_loadu_si256(p as *const __m256i);
        let hi = _mm256_loadu_si256(p.add(32) as *const __m256i);
        macro_rules! movemask {
            ($c:expr) => {{
                let c = _mm256_set1_epi8($c as i8);
                (_mm256_movemask_epi8(_mm256_cmpeq_epi8(lo, c)) as u32 as u64)
                    | ((_mm256_movemask_epi8(_mm256_cmpeq_epi8(hi, c)) as u32 as u64) << 32)
            }};
        }
        Masks {
            comma: movemask!(b','),
            quote: movemask!(b'"'),
            nl: movemask!(b'\n'),
        }
    }
}

/// Which classifier the generic walker inlines.
trait Classify {
    /// # Safety
    /// `p` must point at 64 readable bytes, and the CPU must support the instructions used.
    unsafe fn classify(p: *const u8) -> Masks;
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
struct Scalar;
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
impl Classify for Scalar {
    #[inline(always)]
    unsafe fn classify(p: *const u8) -> Masks {
        // SAFETY: forwarded from the trait contract.
        unsafe { classify_scalar(core::slice::from_raw_parts(p, 64)) }
    }
}

#[cfg(target_arch = "aarch64")]
struct Neon;
#[cfg(target_arch = "aarch64")]
impl Classify for Neon {
    #[inline(always)]
    unsafe fn classify(p: *const u8) -> Masks {
        // SAFETY: forwarded from the trait contract.
        unsafe { classify_neon(p) }
    }
}

#[cfg(target_arch = "x86_64")]
struct Sse2;
#[cfg(target_arch = "x86_64")]
impl Classify for Sse2 {
    #[inline(always)]
    unsafe fn classify(p: *const u8) -> Masks {
        // SAFETY: forwarded from the trait contract.
        unsafe { classify_sse2(p) }
    }
}

#[cfg(target_arch = "x86_64")]
struct Avx2;
#[cfg(target_arch = "x86_64")]
impl Classify for Avx2 {
    #[inline(always)]
    unsafe fn classify(p: *const u8) -> Masks {
        // SAFETY: forwarded from the trait contract.
        unsafe { classify_avx2(p) }
    }
}

/// Walks every line of `buf`.  Generic over the classifier so each variant is fully inlined.
///
/// # Safety
/// The classifier's instructions must be supported by the running CPU.
#[inline(always)]
unsafe fn scan_with<'a, C: Classify, const N: usize, F: FnMut(&'a [u8], Option<[&'a [u8]; N]>)>(
    buf: &'a [u8],
    mut f: F,
) {
    let len = buf.len();
    let base = buf.as_ptr();
    let mut line_start = 0usize;
    let mut commas = [0usize; N];
    let mut ncomma = 0usize;
    let mut quotes = 0usize;
    let mut tail = [0u8; 64];

    #[inline(always)]
    fn finish<'a, const N: usize>(
        buf: &'a [u8],
        start: usize,
        end: usize,
        commas: &[usize; N],
        ncomma: usize,
        quotes: usize,
    ) -> Option<[&'a [u8]; N]> {
        if ncomma + 1 != N {
            return None;
        }
        let mut out: [&[u8]; N] = [&[]; N];
        let mut used = 0usize;
        let mut s = start;
        for (k, slot) in out.iter_mut().enumerate() {
            let e = if k + 1 < N { commas[k] } else { end };
            // SAFETY: s <= e <= buf.len() by construction (commas are increasing positions inside the line)
            let field = unsafe { buf.get_unchecked(s..e) };
            *slot = if field.len() >= 2 && field[0] == b'"' && field[field.len() - 1] == b'"' {
                used += 2;
                unsafe { field.get_unchecked(1..field.len() - 1) }
            } else {
                field
            };
            s = e + 1;
        }
        (used == quotes).then_some(out)
    }

    let mut block = 0usize;
    while block < len {
        // SAFETY: a full block has 64 readable bytes at base + block; a short tail is copied into `tail`.
        let m = if block + 64 <= len {
            unsafe { C::classify(base.add(block)) }
        } else {
            let rest = len - block;
            tail[..rest].copy_from_slice(&buf[block..]);
            tail[rest..].fill(0);
            unsafe { C::classify(tail.as_ptr()) }
        };
        let mut seps = m.comma | m.nl;
        let mut qmask = m.quote;
        while seps != 0 {
            let bit = seps.trailing_zeros();
            let pos = block + bit as usize;
            if (m.nl >> bit) & 1 != 0 {
                let below = (1u64 << bit) - 1;
                quotes += (qmask & below).count_ones() as usize;
                qmask &= !below;
                // SAFETY: line_start <= pos < len.
                f(
                    unsafe { buf.get_unchecked(line_start..pos) },
                    finish(buf, line_start, pos, &commas, ncomma, quotes),
                );
                line_start = pos + 1;
                ncomma = 0;
                quotes = 0;
            } else {
                if ncomma < N {
                    commas[ncomma] = pos;
                }
                ncomma += 1;
            }
            seps &= seps - 1;
        }
        quotes += qmask.count_ones() as usize;
        block += 64;
    }
    if line_start < len {
        f(
            &buf[line_start..len],
            finish(buf, line_start, len, &commas, ncomma, quotes),
        );
    }
}

/// Best available classifier for this CPU (AVX2 detected once at run time on x86_64).
#[inline]
fn scan<'a, const N: usize, F: FnMut(&'a [u8], Option<[&'a [u8]; N]>)>(buf: &'a [u8], f: F) {
    #[cfg(target_arch = "aarch64")]
    unsafe {
        scan_with::<Neon, N, F>(buf, f)
    }
    #[cfg(target_arch = "x86_64")]
    {
        if std::is_x86_feature_detected!("avx2") {
            unsafe { scan_avx2::<N, F>(buf, f) }
        } else {
            unsafe { scan_with::<Sse2, N, F>(buf, f) }
        }
    }
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    unsafe {
        scan_with::<Scalar, N, F>(buf, f)
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn scan_avx2<'a, const N: usize, F: FnMut(&'a [u8], Option<[&'a [u8]; N]>)>(
    buf: &'a [u8],
    f: F,
) {
    // SAFETY: only called after AVX2 was detected.
    unsafe { scan_with::<Avx2, N, F>(buf, f) }
}

/// A validated UTF-8 chunk ends at a record boundary. Each emitted field
/// begins and ends at the chunk boundary or at an ASCII comma, quote or LF.
#[inline]
pub(crate) fn rows<'a, const N: usize>(text: &'a str, mut visit: impl FnMut([&'a str; N])) {
    scan::<N, _>(text.as_bytes(), |_, fields| {
        if let Some(fields) = fields {
            // SAFETY: the ASCII split points are UTF-8 character boundaries.
            visit(fields.map(|bytes| unsafe { std::str::from_utf8_unchecked(bytes) }));
        }
    });
}
