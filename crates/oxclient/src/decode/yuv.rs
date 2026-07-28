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
    let mut bgra = Vec::with_capacity(len);

    // Scalar on purpose. This is the hot loop of the decode path (~2 MB of output per 800x600
    // frame) and the obvious next step is 8 pixels at a time with `wide` or `std::simd`, reading
    // one chroma sample per two luma samples. Do that here, behind this same signature, once
    // there is a profile that says it matters — not before, and not with hand-written intrinsics.
    for row in 0..planes.height {
        let y_row = row * planes.y_stride;
        let u_row = (row / 2) * planes.u_stride;
        let v_row = (row / 2) * planes.v_stride;
        for col in 0..planes.width {
            let luma = Y_SCALE * (i32::from(planes.y[y_row + col]) - 16);
            let cb = i32::from(planes.u[u_row + col / 2]) - 128;
            let cr = i32::from(planes.v[v_row + col / 2]) - 128;
            bgra.push(clamp_u8(luma + B_U * cb));
            bgra.push(clamp_u8(luma + G_U * cb + G_V * cr));
            bgra.push(clamp_u8(luma + R_V * cr));
            // Opaque. The presenter's target is softbuffer's `0x00RRGGBB`, whose top byte is
            // ignored, and the RAW_BGRA frames the agent already sends carry 0xFF here.
            bgra.push(0xff);
        }
    }
    Some(bgra)
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
