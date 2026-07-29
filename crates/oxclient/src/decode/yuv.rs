//! I420 to BGRA conversion.
//!
//! Every decoder that produces planar YUV — the software H.264 decoder today, a VAAPI decoder
//! later — converts through this module, so there is exactly one place where the colour matrix
//! and the output byte order are decided.
//!
//! **Output layout.** Tightly packed `B, G, R, A` bytes, top-down, `width * 4` bytes per row.
//! That is `oxproto::codec::RAW_BGRA`, which the CPU presenter blits with `memcpy` because it is
//! byte-identical to softbuffer's `0x00RRGGBB` `u32` on a little-endian host. Changing the byte
//! order here silently costs a per-frame swizzle in the presenter, so don't.

/// Borrowed I420 planes plus the geometry needed to walk them.
#[derive(Debug, Clone, Copy)]
pub struct I420Planes<'a> {
    /// Luma plane.
    pub y: &'a [u8],
    /// Blue-difference chroma plane, half resolution in both axes.
    pub u: &'a [u8],
    /// Red-difference chroma plane, half resolution in both axes.
    pub v: &'a [u8],
    /// Picture width in pixels.
    pub width: usize,
    /// Picture height in pixels.
    pub height: usize,
    /// Bytes per row of `y`; may exceed `width` (decoders pad to macroblock multiples).
    pub y_stride: usize,
    /// Bytes per row of `u`.
    pub u_stride: usize,
    /// Bytes per row of `v`.
    pub v_stride: usize,
}

impl I420Planes<'_> {
    /// Whether the planes are long enough for the declared geometry.
    fn is_addressable(&self) -> bool {
        if self.width == 0 || self.height == 0 {
            return false;
        }
        let chroma_width = self.width.div_ceil(2);
        let chroma_height = self.height.div_ceil(2);
        if self.y_stride < self.width
            || self.u_stride < chroma_width
            || self.v_stride < chroma_width
        {
            return false;
        }
        plane_fits(self.y, self.height, self.y_stride, self.width)
            && plane_fits(self.u, chroma_height, self.u_stride, chroma_width)
            && plane_fits(self.v, chroma_height, self.v_stride, chroma_width)
    }
}

fn plane_fits(plane: &[u8], rows: usize, stride: usize, row_bytes: usize) -> bool {
    // The last row only needs `row_bytes`, not a full stride: decoders are entitled to hand back
    // a buffer that stops at the end of the final row of real samples.
    rows.checked_sub(1)
        .and_then(|last| last.checked_mul(stride))
        .and_then(|offset| offset.checked_add(row_bytes))
        .is_some_and(|needed| plane.len() >= needed)
}

// BT.601 limited ("video") range, the inverse of the forward matrix every H.264 encoder in this
// pipeline uses when it has no VUI saying otherwise, in Q16 fixed point:
//
//   R = 1.164 * (Y - 16)                        + 1.596 * (V - 128)
//   G = 1.164 * (Y - 16) - 0.391 * (U - 128)    - 0.813 * (V - 128)
//   B = 1.164 * (Y - 16) + 2.018 * (U - 128)
//
// Fixed point rather than float so the conversion is bit-reproducible across machines: a
// rendering difference between two clients must never be attributable to the FPU.
//
// A stream that signals full-range or BT.709 in its VUI needs a second matrix selected per
// stream; OXPROTO does not yet say what the agent may send (see the report accompanying this
// change), so one matrix is implemented and the choice is documented rather than guessed at.
const Y_SCALE: i32 = 76_309; // 255/219
const R_V: i32 = 104_597; // 255/224 * 1.402
const G_U: i32 = -25_675; // -255/224 * 1.772 * 0.114/0.587
const G_V: i32 = -53_279; // -255/224 * 1.402 * 0.299/0.687
const B_U: i32 = 132_201; // 255/224 * 1.772
const HALF: i32 = 1 << 15;

/// Converts an I420 picture to tightly packed BGRA.
///
/// Returns `None` if the planes are shorter than the declared geometry requires, which is the
/// one thing a caller cannot check for itself when the planes come out of a C decoder.
#[must_use]
pub fn i420_to_bgra(planes: I420Planes<'_>) -> Option<Vec<u8>> {
    if !planes.is_addressable() {
        return None;
    }
    let len = planes.width.checked_mul(planes.height)?.checked_mul(4)?;
    // Zeroed rather than `with_capacity` + push: allocating the whole output up front lets the
    // row loop write through a slice, which is what removes the per-byte capacity check and the
    // per-pixel bounds check. Large zeroed allocations come from the allocator as untouched
    // pages, so this is not a memset the old path avoided — the pages are faulted in on write
    // either way.
    let mut bgra = vec![0u8; len];

    convert_picture(planes, &mut bgra);
    Some(bgra)
}

/// Converts a whole picture, eight pixels at a time where the CPU allows it.
///
/// `multiversion` compiles this once per target below and picks between them **at run time**,
/// from what the CPU actually reports — so one binary runs on a machine without AVX2 and still
/// uses AVX2 on a machine that has it. Compiling the whole crate with `-C target-feature=+avx2`
/// would be the other way to reach those instructions, and it is the wrong way: the binary then
/// dies with an illegal instruction on any older CPU.
///
/// The dispatch happens once per picture, not per row and certainly not per pixel. The row
/// helper is `#[inline(always)]` so it is compiled into each target's clone and inherits that
/// clone's instruction set; a helper that is merely `#[inline]` would silently be built once, at
/// baseline, and the SIMD would evaporate.
///
/// No `unsafe` appears here or anywhere in this crate: `multiversion` generates the feature
/// dispatch and `wide` wraps the vector types, and `oxclient` keeps `#![forbid(unsafe_code)]`,
/// which the compiler enforces against macro-generated code too.
#[multiversion::multiversion(targets("x86_64+avx2", "x86_64+sse4.1", "aarch64+neon"))]
fn convert_picture<'planes>(planes: I420Planes<'planes>, out: &mut [u8]) {
    let chroma_width = planes.width.div_ceil(2);
    for row in 0..planes.height {
        let luma = &planes.y[row * planes.y_stride..][..planes.width];
        let cb = &planes.u[(row / 2) * planes.u_stride..][..chroma_width];
        let cr = &planes.v[(row / 2) * planes.v_stride..][..chroma_width];
        let out = &mut out[row * planes.width * 4..][..planes.width * 4];
        convert_row_vector(luma, cb, cr, out);
    }
}

/// Eight pixels per step: eight luma samples against four chroma samples.
///
/// The arithmetic is the same Q16 fixed point as [`convert_row`], lane for lane, so the output is
/// **bit-identical** to the scalar reference rather than merely close — which is what lets the
/// tests assert equality instead of a tolerance, and keeps a frame from looking different on two
/// machines. Floating-point lanes would have been faster to write and would have given up both.
#[inline(always)]
fn convert_row_vector(luma: &[u8], cb: &[u8], cr: &[u8], out: &mut [u8]) {
    use wide::i32x8;

    const SHIFT: i32 = 16;
    let scale = i32x8::from([Y_SCALE; 8]);
    let half = i32x8::from([HALF; 8]);
    let floor = i32x8::from([0; 8]);
    let ceiling = i32x8::from([255; 8]);
    let blue_u = i32x8::from([B_U; 8]);
    let green_u = i32x8::from([G_U; 8]);
    let green_v = i32x8::from([G_V; 8]);
    let red_v = i32x8::from([R_V; 8]);

    let mut octets = out.chunks_exact_mut(32);
    for (((out, luma), cb), cr) in octets
        .by_ref()
        .zip(luma.chunks_exact(8))
        .zip(cb.chunks_exact(4))
        .zip(cr.chunks_exact(4))
    {
        let y = i32x8::from([
            i32::from(luma[0]) - 16,
            i32::from(luma[1]) - 16,
            i32::from(luma[2]) - 16,
            i32::from(luma[3]) - 16,
            i32::from(luma[4]) - 16,
            i32::from(luma[5]) - 16,
            i32::from(luma[6]) - 16,
            i32::from(luma[7]) - 16,
        ]) * scale;
        // Each chroma sample covers two pixels, so it occupies two lanes.
        let u = i32x8::from([
            i32::from(cb[0]) - 128,
            i32::from(cb[0]) - 128,
            i32::from(cb[1]) - 128,
            i32::from(cb[1]) - 128,
            i32::from(cb[2]) - 128,
            i32::from(cb[2]) - 128,
            i32::from(cb[3]) - 128,
            i32::from(cb[3]) - 128,
        ]);
        let v = i32x8::from([
            i32::from(cr[0]) - 128,
            i32::from(cr[0]) - 128,
            i32::from(cr[1]) - 128,
            i32::from(cr[1]) - 128,
            i32::from(cr[2]) - 128,
            i32::from(cr[2]) - 128,
            i32::from(cr[3]) - 128,
            i32::from(cr[3]) - 128,
        ]);

        let blue = ((y + u * blue_u + half) >> SHIFT)
            .max(floor)
            .min(ceiling)
            .to_array();
        let green = ((y + u * green_u + v * green_v + half) >> SHIFT)
            .max(floor)
            .min(ceiling)
            .to_array();
        let red = ((y + v * red_v + half) >> SHIFT)
            .max(floor)
            .min(ceiling)
            .to_array();

        for (lane, pixel) in out.chunks_exact_mut(4).enumerate() {
            pixel[0] = blue[lane] as u8;
            pixel[1] = green[lane] as u8;
            pixel[2] = red[lane] as u8;
            pixel[3] = 0xff;
        }
    }

    // Whatever is left of the row — up to seven pixels, and possibly an odd one — goes through
    // the scalar reference. This is the arithmetic that a "the vector width divides the row"
    // assumption gets wrong, so it is not reimplemented here.
    let whole = luma.len() - luma.len() % 8;
    if whole < luma.len() {
        convert_row(
            &luma[whole..],
            &cb[whole / 2..],
            &cr[whole / 2..],
            octets.into_remainder(),
        );
    }
}

/// Converts one row, scalar: the reference implementation and the fallback.
///
/// This is what the vector path is checked against, and what runs on any target the dispatch
/// above has no clone for, so it stays a complete implementation rather than an odd-pixel helper.
///
/// Written pairwise because 4:2:0 is pairwise: two luma samples share one chroma sample, so the
/// three chroma-dependent products are computed once per two pixels instead of six times. The
/// iterators are zipped rather than indexed so the bounds checks fall away.
fn convert_row(luma: &[u8], cb: &[u8], cr: &[u8], out: &mut [u8]) {
    debug_assert_eq!(out.len(), luma.len() * 4);

    let mut pairs = out.chunks_exact_mut(8);
    for (((out, luma), &cb), &cr) in pairs.by_ref().zip(luma.chunks_exact(2)).zip(cb).zip(cr) {
        let (blue, green, red) = chroma_offsets(cb, cr);
        for (pixel, &luma) in out.chunks_exact_mut(4).zip(luma) {
            write_pixel(pixel, luma, blue, green, red);
        }
    }

    // An odd width leaves one pixel, which shares the last chroma column with the pixel before
    // it. This is the case that a "vector width divides the row" assumption gets wrong.
    let tail = pairs.into_remainder();
    if !tail.is_empty() {
        let last = cb.len() - 1;
        let (blue, green, red) = chroma_offsets(cb[last], cr[last]);
        write_pixel(tail, luma[luma.len() - 1], blue, green, red);
    }
}

/// The three chroma-dependent terms, computed once per chroma sample.
fn chroma_offsets(cb: u8, cr: u8) -> (i32, i32, i32) {
    let cb = i32::from(cb) - 128;
    let cr = i32::from(cr) - 128;
    (B_U * cb, G_U * cb + G_V * cr, R_V * cr)
}

fn write_pixel(pixel: &mut [u8], luma: u8, blue: i32, green: i32, red: i32) {
    let luma = Y_SCALE * (i32::from(luma) - 16);
    pixel[0] = clamp_u8(luma + blue);
    pixel[1] = clamp_u8(luma + green);
    pixel[2] = clamp_u8(luma + red);
    // Opaque. The presenter's target is softbuffer's `0x00RRGGBB`, whose top byte is ignored,
    // and the RAW_BGRA frames the agent already sends carry 0xFF here.
    pixel[3] = 0xff;
}

fn clamp_u8(q16: i32) -> u8 {
    let value = (q16 + HALF) >> 16;
    value.clamp(0, 255) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds single-colour planes, the smallest input that pins down the colour matrix.
    fn flat(width: usize, height: usize, y: u8, u: u8, v: u8) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        let chroma = width.div_ceil(2) * height.div_ceil(2);
        (vec![y; width * height], vec![u; chroma], vec![v; chroma])
    }

    fn convert(width: usize, height: usize, y: u8, u: u8, v: u8) -> Vec<u8> {
        let (y_plane, u_plane, v_plane) = flat(width, height, y, u, v);
        i420_to_bgra(I420Planes {
            y: &y_plane,
            u: &u_plane,
            v: &v_plane,
            width,
            height,
            y_stride: width,
            u_stride: width.div_ceil(2),
            v_stride: width.div_ceil(2),
        })
        .expect("planes match the declared geometry")
    }

    /// The scalar path alone, row by row — what the vector path must reproduce exactly.
    fn scalar_reference(planes: I420Planes<'_>) -> Vec<u8> {
        let mut out = vec![0u8; planes.width * planes.height * 4];
        let chroma_width = planes.width.div_ceil(2);
        for row in 0..planes.height {
            convert_row(
                &planes.y[row * planes.y_stride..][..planes.width],
                &planes.u[(row / 2) * planes.u_stride..][..chroma_width],
                &planes.v[(row / 2) * planes.v_stride..][..chroma_width],
                &mut out[row * planes.width * 4..][..planes.width * 4],
            );
        }
        out
    }

    /// Deterministic pseudo-random planes with padded strides.
    ///
    /// Random rather than flat on purpose: flat planes cannot catch a lane fed from the wrong
    /// sample, which is the characteristic SIMD bug. Deterministic so a failure is reproducible.
    fn noisy_planes(width: usize, height: usize) -> (Vec<u8>, Vec<u8>, Vec<u8>, usize, usize) {
        let mut state = 0x2545_f491_4f6c_dd1d_u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 33) as u8
        };
        let y_stride = width + 13;
        let chroma_stride = width.div_ceil(2) + 5;
        let y = (0..y_stride * height).map(|_| next()).collect();
        let u = (0..chroma_stride * height.div_ceil(2))
            .map(|_| next())
            .collect();
        let v = (0..chroma_stride * height.div_ceil(2))
            .map(|_| next())
            .collect();
        (y, u, v, y_stride, chroma_stride)
    }

    #[test]
    fn the_vector_path_is_bit_identical_to_the_scalar_reference() {
        // Every width from 1 to 40 covers each remainder modulo the eight-pixel vector step,
        // both parities, and the widths too narrow for a single vector step at all. The larger
        // sizes straddle the step boundary from both sides.
        let widths = (1usize..=40).chain([63, 64, 65, 127, 128, 129, 255, 256, 257]);
        for width in widths {
            for height in [1usize, 2, 3, 5] {
                let (y, u, v, y_stride, chroma_stride) = noisy_planes(width, height);
                let planes = I420Planes {
                    y: &y,
                    u: &u,
                    v: &v,
                    width,
                    height,
                    y_stride,
                    u_stride: chroma_stride,
                    v_stride: chroma_stride,
                };

                let vector = i420_to_bgra(planes).expect("planes are addressable");
                let scalar = scalar_reference(planes);

                assert_eq!(
                    vector.len(),
                    width * height * 4,
                    "wrong output length at {width}x{height}"
                );
                // Compared as whole buffers, then narrowed to the first differing pixel so a
                // failure names the pixel rather than dumping a megabyte.
                if vector != scalar {
                    let at = vector
                        .iter()
                        .zip(&scalar)
                        .position(|(a, b)| a != b)
                        .expect("the buffers differ somewhere");
                    panic!(
                        "vector and scalar differ at {width}x{height}, byte {at} \
                         (pixel {}, channel {}): {} vs {}",
                        at / 4,
                        at % 4,
                        vector[at],
                        scalar[at]
                    );
                }
            }
        }
    }

    #[test]
    fn both_paths_clamp_the_whole_input_range_the_same_way() {
        // A row that sweeps luma across its full range against extreme chroma drives every
        // pixel past both ends of the clamp, in both implementations.
        let width: usize = 256;
        let height: usize = 2;
        let y: Vec<u8> = (0..width * height).map(|i| (i % 256) as u8).collect();
        let u: Vec<u8> = (0..width.div_ceil(2) * height.div_ceil(2))
            .map(|i| if i % 2 == 0 { 0 } else { 255 })
            .collect();
        let v: Vec<u8> = (0..width.div_ceil(2) * height.div_ceil(2))
            .map(|i| if i % 3 == 0 { 255 } else { 0 })
            .collect();
        let planes = I420Planes {
            y: &y,
            u: &u,
            v: &v,
            width,
            height,
            y_stride: width,
            u_stride: width.div_ceil(2),
            v_stride: width.div_ceil(2),
        };

        assert_eq!(
            i420_to_bgra(planes).expect("addressable"),
            scalar_reference(planes)
        );
    }

    #[test]
    fn converts_the_limited_range_end_points() {
        // Y=16 is black and Y=235 is white in limited range; neutral chroma is 128.
        assert_eq!(convert(2, 2, 16, 128, 128)[..4], [0, 0, 0, 255]);
        assert_eq!(convert(2, 2, 235, 128, 128)[..4], [255, 255, 255, 255]);
        // Below black and above white clamp instead of wrapping.
        assert_eq!(convert(2, 2, 0, 128, 128)[..4], [0, 0, 0, 255]);
        assert_eq!(convert(2, 2, 255, 128, 128)[..4], [255, 255, 255, 255]);
    }

    #[test]
    fn writes_blue_green_red_alpha_in_that_order() {
        // Pure red in BT.601 limited range: Y 81, U 90, V 240.
        let red = convert(2, 2, 81, 90, 240);
        assert!(red[2] > 240, "red channel is byte 2, got {red:?}");
        assert!(
            red[0] < 24 && red[1] < 24,
            "blue and green are low, got {red:?}"
        );
        // Pure blue: Y 41, U 240, V 110.
        let blue = convert(2, 2, 41, 240, 110);
        assert!(blue[0] > 240, "blue channel is byte 0, got {blue:?}");
        assert!(
            blue[1] < 24 && blue[2] < 24,
            "green and red are low, got {blue:?}"
        );
        assert_eq!(blue[3], 255, "alpha is opaque");
    }

    #[test]
    fn honours_row_padding_in_the_source_planes() {
        // A decoder that pads rows to macroblock multiples must not shear the picture, so the
        // luma pattern here is a checkerboard: reading rows at `width` instead of `y_stride`
        // shifts every row but the first and changes what comes out.
        let width = 4;
        let height = 2;
        let y_stride = 16;
        let mut y_plane = vec![16u8; y_stride * height];
        y_plane[..width].copy_from_slice(&[235, 16, 235, 16]);
        y_plane[y_stride..y_stride + width].copy_from_slice(&[16, 235, 16, 235]);
        let chroma = vec![128u8; 8];

        let bgra = i420_to_bgra(I420Planes {
            y: &y_plane,
            u: &chroma,
            v: &chroma,
            width,
            height,
            y_stride,
            u_stride: 8,
            v_stride: 8,
        })
        .expect("padded planes convert");

        assert_eq!(bgra.len(), width * height * 4);
        let luma: Vec<u8> = bgra.chunks_exact(4).map(|pixel| pixel[1]).collect();
        assert_eq!(luma, [255, 0, 255, 0, 0, 255, 0, 255]);
    }

    #[test]
    fn rejects_planes_that_are_too_short_for_the_geometry() {
        let short = vec![16u8; 4];
        assert!(i420_to_bgra(I420Planes {
            y: &short,
            u: &short,
            v: &short,
            width: 64,
            height: 64,
            y_stride: 64,
            u_stride: 32,
            v_stride: 32,
        })
        .is_none());
    }

    #[test]
    fn rejects_a_stride_narrower_than_the_picture() {
        let plane = vec![16u8; 4096];
        assert!(i420_to_bgra(I420Planes {
            y: &plane,
            u: &plane,
            v: &plane,
            width: 64,
            height: 64,
            y_stride: 32,
            u_stride: 32,
            v_stride: 32,
        })
        .is_none());
    }

    #[test]
    fn rejects_empty_geometry() {
        let plane = vec![16u8; 16];
        assert!(i420_to_bgra(I420Planes {
            y: &plane,
            u: &plane,
            v: &plane,
            width: 0,
            height: 4,
            y_stride: 0,
            u_stride: 0,
            v_stride: 0,
        })
        .is_none());
    }

    #[test]
    fn odd_geometry_reads_the_shared_chroma_sample() {
        // 3x3 has 2x2 chroma; the last column and row share the neighbouring sample.
        let bgra = convert(3, 3, 235, 128, 128);
        assert_eq!(bgra.len(), 3 * 3 * 4);
        assert!(bgra.chunks_exact(4).all(|px| px == [255, 255, 255, 255]));
    }
}
