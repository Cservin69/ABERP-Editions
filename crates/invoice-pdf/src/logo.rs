//! PR-176 — optional tenant-logo image for the printed-invoice header.
//!
//! Convention over config (per CLAUDE.md rule 2 + the brief): the
//! operator drops a PNG at `~/.aberp/<tenant>/logo.png` and the
//! orchestrator hands the decoded bytes to the renderer via
//! [`InvoiceModel::tenant_logo`](crate::InvoiceModel::tenant_logo).
//! No `seller.toml` knob, no DB column, no upload affordance yet —
//! pure filesystem convention.
//!
//! # Scope (v1)
//!
//! - **PNG only.** SVG / JPG are deferred to a follow-up; logos are
//!   small one-shot brand assets, the operator can re-export. Adding a
//!   second format here would double the decode surface for negligible
//!   gain at v1.
//! - **8-bit output.** Decoding applies `EXPAND | STRIP_16`
//!   ([`png::Transformations::normalize_to_color8`]) so every PNG
//!   variant (palette, grayscale, RGB, RGBA, 16-bit) collapses to one
//!   of four 8-bit colour families. The renderer always sees RGB8.
//! - **Aspect-preserved within a fixed box.** The renderer places the
//!   logo at a fixed 50×50-pt box top-left of the header (sized so the
//!   existing under-rule and title geometry are NOT disturbed); the
//!   decoded image is scaled to fit while preserving aspect ratio.
//! - **No caching.** Decoded on every render. Logos are tiny and
//!   render cadence is low (one PDF per issued invoice); an LRU layer
//!   would be premature per CLAUDE.md rule 13.

use crate::RenderError;

/// Decoded tenant-logo image, ready for embedding as a PDF Image
/// XObject. Always carries 8-bit RGB pixels in row-major order; alpha
/// and grayscale variants are normalised at construction time.
#[derive(Debug, Clone)]
pub struct TenantLogo {
    pub width: u32,
    pub height: u32,
    /// Raw 8-bit RGB pixels — `width * height * 3` bytes, row-major,
    /// top-left origin (matches PNG's natural scan order and PDF's
    /// Image XObject conventions when the placement matrix sets a
    /// positive vertical scale).
    pub rgb_bytes: Vec<u8>,
}

impl TenantLogo {
    /// Decode `bytes` as a PNG into an 8-bit-RGB tenant logo.
    ///
    /// Returns [`RenderError::LogoDecode`] on:
    /// - non-PNG bytes (corrupted file, JPG dropped at the convention
    ///   path with the wrong extension, etc.),
    /// - a PNG output buffer whose channel count after
    ///   `normalize_to_color8` is anything other than the four
    ///   recognised 8-bit families (defensive — the png crate's own
    ///   invariants prevent this, but we surface a loud error rather
    ///   than panic if a future png upgrade widens the output set).
    ///
    /// The caller — `print_invoice.rs` — treats a decode error as
    /// orchestrator-fatal so the operator sees the failure rather than
    /// shipping a logo-less PDF silently (CLAUDE.md rule 12 "fail
    /// loud" — silent fallback to text-only header is acceptable when
    /// the file is *absent*, not when it's malformed).
    pub fn from_png_bytes(bytes: &[u8]) -> Result<Self, RenderError> {
        let mut decoder = png::Decoder::new(bytes);
        // `normalize_to_color8` = EXPAND | STRIP_16. EXPAND: palette →
        // RGB(A), grayscale<8bpp → 8bpp, tRNS → alpha. STRIP_16: 16bpc
        // → 8bpc. The combined output is always one of {Grayscale,
        // GrayscaleAlpha, Rgb, Rgba} at 8 bits per channel.
        decoder.set_transformations(png::Transformations::normalize_to_color8());
        let mut reader = decoder
            .read_info()
            .map_err(|e| RenderError::LogoDecode(format!("read PNG info: {e}")))?;
        let info = reader.info();
        let width = info.width;
        let height = info.height;
        let mut buf = vec![0u8; reader.output_buffer_size()];
        let frame = reader
            .next_frame(&mut buf)
            .map_err(|e| RenderError::LogoDecode(format!("decode PNG frame: {e}")))?;
        let pixels = &buf[..frame.buffer_size()];

        let rgb_bytes = match frame.color_type {
            png::ColorType::Rgb => pixels.to_vec(),
            png::ColorType::Rgba => pixels
                .chunks_exact(4)
                .flat_map(|p| [p[0], p[1], p[2]])
                .collect(),
            png::ColorType::Grayscale => pixels.iter().flat_map(|&g| [g, g, g]).collect(),
            png::ColorType::GrayscaleAlpha => pixels
                .chunks_exact(2)
                .flat_map(|p| [p[0], p[0], p[0]])
                .collect(),
            // After `normalize_to_color8`, palette + 16-bit variants
            // collapse into the four above; any other variant is the
            // defensive surface noted in the doc-comment.
            other => {
                return Err(RenderError::LogoDecode(format!(
                    "PNG decoded to unsupported colour type after normalize_to_color8: {other:?}"
                )))
            }
        };

        let expected_len = (width as usize)
            .saturating_mul(height as usize)
            .saturating_mul(3);
        if rgb_bytes.len() != expected_len {
            return Err(RenderError::LogoDecode(format!(
                "decoded PNG yielded {} RGB bytes, expected {expected_len} for {width}x{height}",
                rgb_bytes.len()
            )));
        }

        Ok(Self {
            width,
            height,
            rgb_bytes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal valid PNG of `w`×`h` solid colour at runtime via
    /// the `png` crate's encoder. Keeps the test fixture inline rather
    /// than shipping a binary PNG in the source tree — same posture as
    /// other invoice-pdf tests that synthesise their inputs.
    fn synth_png(w: u32, h: u32, color_type: png::ColorType, pixel: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut out, w, h);
            encoder.set_color(color_type);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().expect("png header");
            let mut buf = Vec::with_capacity((w as usize) * (h as usize) * pixel.len());
            for _ in 0..(w as usize) * (h as usize) {
                buf.extend_from_slice(pixel);
            }
            writer.write_image_data(&buf).expect("png write data");
        }
        out
    }

    #[test]
    fn decodes_rgb8_passthrough() {
        let png = synth_png(4, 3, png::ColorType::Rgb, &[10, 20, 30]);
        let logo = TenantLogo::from_png_bytes(&png).expect("decode");
        assert_eq!(logo.width, 4);
        assert_eq!(logo.height, 3);
        assert_eq!(logo.rgb_bytes.len(), 4 * 3 * 3);
        assert!(logo.rgb_bytes.chunks_exact(3).all(|p| p == [10, 20, 30]));
    }

    #[test]
    fn decodes_rgba_drops_alpha() {
        let png = synth_png(2, 2, png::ColorType::Rgba, &[200, 100, 50, 0]);
        let logo = TenantLogo::from_png_bytes(&png).expect("decode");
        assert_eq!(logo.rgb_bytes.len(), 2 * 2 * 3);
        // Alpha must be dropped; underlying RGB persists even at α=0
        // (the renderer composites against the white page background
        // implicitly — we do not pre-multiply, so transparent-edge
        // logos display their RGB ink, not a black halo).
        assert!(logo.rgb_bytes.chunks_exact(3).all(|p| p == [200, 100, 50]));
    }

    #[test]
    fn decodes_grayscale_expands_to_rgb() {
        let png = synth_png(3, 2, png::ColorType::Grayscale, &[128]);
        let logo = TenantLogo::from_png_bytes(&png).expect("decode");
        assert_eq!(logo.rgb_bytes.len(), 3 * 2 * 3);
        assert!(logo.rgb_bytes.chunks_exact(3).all(|p| p == [128, 128, 128]));
    }

    #[test]
    fn malformed_bytes_surface_loud_error() {
        let result = TenantLogo::from_png_bytes(b"not a PNG");
        assert!(matches!(result, Err(RenderError::LogoDecode(_))));
    }
}
