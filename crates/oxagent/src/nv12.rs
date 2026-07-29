//! BGRA → NV12 color conversion, for feeding captured window pixels into an H.264 encoder that
//! wants NV12 input.
//!
//! Deliberately platform-independent and CPU-only, even though the encoder that consumes its
//! output (`crate::win::encode`) is not: this is pure integer arithmetic over a byte buffer,
//! with nothing Windows-specific about it, so it earns its own test coverage on the Linux build
//! host rather than being folded into code nobody but a Windows guest can exercise.
//!
//! **Where this sits in the pipeline, and why it costs what it costs.** `crate::win::capture`
//! already reads a captured window back from its D3D11 texture to a CPU-side BGRA buffer, for
//! the `RAW_BGRA` path, which must keep working unconditionally. Converting on the GPU — via a
//! D3D11 video processor `Blt` or a compute shader, while the frame is still a texture, before
//! that readback — would avoid ever materializing the *full-size* BGRA buffer for the H.264
//! path (NV12 is 1.5 bytes/pixel against BGRA's 4, so the readback itself would shrink too) and
//! is the more correct design for a mature version of this pipeline. It was not implemented
//! here: setting up an `ID3D11VideoDevice`/`ID3D11VideoProcessor` pipeline correctly (and its
//! failure modes — a wrong Blt color space produces a *plausible-looking but wrong* image, not
//! an error return) is not something this file's author could validate without a live Windows
//! guest to run it against, and a silently-wrong color conversion is worse than a slower correct
//! one. This CPU conversion reuses the exact BGRA buffer the already-validated `RAW_BGRA` path
//! produces, so its only risk is speed, not correctness-that-looks-fine-in-review. It is real
//! cost — a naive per-pixel conversion is squarely in the latency budget of a 1080p frame at 30
//! fps — and the GPU-side version above is the documented next step once someone can measure it
//! on real hardware rather than guess at it from a cross-compile.

/// Convert a tightly-packed, top-down BGRA8 image to NV12 (one Y byte per pixel, followed by a
/// half-resolution interleaved U/V plane, both planes strided at `width`).
///
/// `width` and `height` must both be even — NV12's chroma planes are subsampled 2:1 in each
/// dimension, so an odd dimension has no last chroma sample to pair with. Callers with an
/// odd-sized capture pad to the next even size first (`crate::win::encode` does this, since
/// what to pad with — and what `FrameData.width`/`height` then has to say — is an encoder
/// concern, not a color-conversion one). Panics if either dimension is odd or the input length
/// does not match `width * height * 4`, since a mismatched buffer means a caller bug, not bad
/// remote input — nothing about this data comes from the network.
pub fn bgra_to_nv12(bgra: &[u8], width: usize, height: usize) -> Vec<u8> {
    assert_eq!(width % 2, 0, "NV12 requires an even width");
    assert_eq!(height % 2, 0, "NV12 requires an even height");
    assert_eq!(
        bgra.len(),
        width * height * 4,
        "BGRA buffer does not match width*height*4"
    );

    let y_size = width * height;
    let uv_size = width * (height / 2);
    let mut out = vec![0u8; y_size + uv_size];
    let (y_plane, uv_plane) = out.split_at_mut(y_size);

    for row in 0..height {
        let src_row = &bgra[row * width * 4..(row + 1) * width * 4];
        let dst_row = &mut y_plane[row * width..(row + 1) * width];
        for col in 0..width {
            let px = &src_row[col * 4..col * 4 + 4];
            dst_row[col] = luma(px[2], px[1], px[0]);
        }
    }

    // Chroma is point-sampled from the top-left pixel of each 2×2 block rather than averaged
    // across all four — a standard "fast" subsampling shortcut (matching e.g. libyuv's
    // non-box-filter path) that trades a small amount of chroma sharpness for one BGRA read
    // instead of four per output sample. For UI/screen content, dominated by flat fills and
    // text edges rather than fine color gradients, the difference is not visible at the bitrate
    // this codec targets.
    for row in (0..height).step_by(2) {
        let src_row = &bgra[row * width * 4..(row + 1) * width * 4];
        let uv_row = &mut uv_plane[(row / 2) * width..(row / 2 + 1) * width];
        for col in (0..width).step_by(2) {
            let px = &src_row[col * 4..col * 4 + 4];
            let (u, v) = chroma(px[2], px[1], px[0]);
            uv_row[col] = u;
            uv_row[col + 1] = v;
        }
    }

    out
}

/// BT.601 full-range luma, as a fixed-point approximation of `Y = 0.299R + 0.587G + 0.114B`
/// (coefficients ×256, rounded): screen content has no studio-range mastering to respect, so
/// full range (0–255) is used rather than the 16–235 "video range" broadcast studios target —
/// using video range here would visibly crush blacks and whites for no benefit.
fn luma(r: u8, g: u8, b: u8) -> u8 {
    let y = (77 * i32::from(r) + 150 * i32::from(g) + 29 * i32::from(b)) >> 8;
    y.clamp(0, 255) as u8
}

/// BT.601 full-range chroma pair, same fixed-point convention as [`luma`].
fn chroma(r: u8, g: u8, b: u8) -> (u8, u8) {
    let (r, g, b) = (i32::from(r), i32::from(g), i32::from(b));
    let u = ((-43 * r - 85 * g + 128 * b) >> 8) + 128;
    let v = ((128 * r - 107 * g - 21 * b) >> 8) + 128;
    (u.clamp(0, 255) as u8, v.clamp(0, 255) as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 2×2 BGRA image of a single solid color, the smallest size NV12 can represent.
    fn solid(b: u8, g: u8, r: u8) -> Vec<u8> {
        [b, g, r, 255].repeat(4)
    }

    #[test]
    fn white_is_full_luma_and_neutral_chroma() {
        let nv12 = bgra_to_nv12(&solid(255, 255, 255), 2, 2);
        assert_eq!(&nv12[0..4], &[255, 255, 255, 255], "Y plane");
        assert_eq!(&nv12[4..6], &[128, 128], "U/V plane, within 1 of neutral");
    }

    #[test]
    fn black_is_zero_luma_and_neutral_chroma() {
        let nv12 = bgra_to_nv12(&solid(0, 0, 0), 2, 2);
        assert_eq!(&nv12[0..4], &[0, 0, 0, 0], "Y plane");
        assert_eq!(&nv12[4..6], &[128, 128]);
    }

    #[test]
    fn output_is_sized_for_4_2_0_subsampling() {
        let bgra = vec![0u8; 4 * 4 * 4]; // 4x4, all zero
        let nv12 = bgra_to_nv12(&bgra, 4, 4);
        // Y plane: 4*4 = 16. UV plane: 4 * (4/2) = 8. Total 24.
        assert_eq!(nv12.len(), 16 + 8);
    }

    #[test]
    fn chroma_is_point_sampled_from_the_top_left_of_each_block() {
        // Each 2x2 block is two different colors stacked so only the top row can possibly be
        // what a top-left point sample reads; if the implementation ever changes to box-filter
        // all four pixels, the bottom row's (very different) color would visibly shift the U/V
        // result and this assertion would catch it.
        let mut bgra = vec![0u8; 2 * 2 * 4];
        // Top-left and top-right: pure red. Bottom row: pure blue.
        bgra[0..4].copy_from_slice(&[0, 0, 255, 255]); // (0,0) BGRA red
        bgra[4..8].copy_from_slice(&[0, 0, 255, 255]); // (1,0) BGRA red
        bgra[8..12].copy_from_slice(&[255, 0, 0, 255]); // (0,1) BGRA blue
        bgra[12..16].copy_from_slice(&[255, 0, 0, 255]); // (1,1) BGRA blue

        let nv12 = bgra_to_nv12(&bgra, 2, 2);
        let (expected_u, expected_v) = chroma(255, 0, 0); // pure red, matching the top row
        assert_eq!(&nv12[4..6], &[expected_u, expected_v]);
    }

    #[test]
    #[should_panic(expected = "even width")]
    fn odd_width_panics() {
        let bgra = vec![0u8; 3 * 2 * 4];
        let _ = bgra_to_nv12(&bgra, 3, 2);
    }

    #[test]
    #[should_panic(expected = "even height")]
    fn odd_height_panics() {
        let bgra = vec![0u8; 2 * 3 * 4];
        let _ = bgra_to_nv12(&bgra, 2, 3);
    }

    #[test]
    #[should_panic(expected = "width*height*4")]
    fn mismatched_buffer_length_panics() {
        let bgra = vec![0u8; 10]; // too short for any real 2x2 BGRA image
        let _ = bgra_to_nv12(&bgra, 2, 2);
    }
}
