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
//!
//! # Safety caps (PR-185 / S185)
//!
//! - Dimensions ≤ [`MAX_LOGO_DIMENSION`] × [`MAX_LOGO_DIMENSION`]
//!   (4096×4096). The dimension check runs on `read_info()` output
//!   BEFORE the decode buffer is allocated, so a maliciously-headered
//!   PNG cannot trigger a multi-gigabyte allocation in
//!   `vec![0u8; reader.output_buffer_size()]`.
//! - Explicit png-crate [`png::Limits`] cap on the decoder, matched to
//!   the dimension cap. Defends against a future png-crate upgrade that
//!   would relax the default (currently 64 MiB) and silently re-open
//!   the decompression-bomb path that the dimension check guards.
//! - File-on-disk size cap lives in the orchestrator
//!   (`print_invoice::load_tenant_logo`, currently 2 MiB) — a logo is
//!   a 50×50pt header asset; anything larger is operator error or
//!   attack. The orchestrator downgrades any decode/limit/IO failure
//!   to a `tracing::warn!` + text-only header per the PR-185 brief
//!   (legal-document rendering must NEVER block on a branding asset).

use crate::RenderError;

/// Maximum width or height accepted from a PNG header. The header logo
/// renders into a 50×50pt box on the printed invoice; 4096 is two
/// orders of magnitude over what any operator would supply
/// intentionally. The cap exists to bound the decoded-buffer
/// allocation against malformed or hostile PNG headers.
pub const MAX_LOGO_DIMENSION: u32 = 4096;

/// Decoder buffer cap fed to [`png::Decoder::set_limits`]. Sized to the
/// dimension cap (4096 × 4096 × 4 bytes RGBA) so the two checks are
/// consistent: legitimate logos up to [`MAX_LOGO_DIMENSION`] always
/// decode; the explicit `Limits` is the secondary defence in case the
/// reported dimensions and the actual buffer size diverge across a
/// future png-crate revision.
const MAX_PNG_DECODE_BYTES: usize =
    (MAX_LOGO_DIMENSION as usize) * (MAX_LOGO_DIMENSION as usize) * 4;

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

/// PR-185 / Fix C — alpha-composite one 8-bit channel over white.
///
/// out = round((src·α + 255·(255-α)) / 255)
///
/// α = 0   → 255 (fully transparent pixel becomes white)
/// α = 255 → src (fully opaque pixel passes through)
/// α = 128 → ≈(src + 255)/2 (half-blend with the page background)
///
/// Integer math with +127 for round-to-nearest; the +127 also keeps
/// α = 255 lossless: (src · 255 + 0 + 127) / 255 == src for any src∈[0,255].
#[inline]
fn composite_over_white(src: u8, alpha: u8) -> u8 {
    let s = src as u32;
    let a = alpha as u32;
    let blended = s * a + 255 * (255 - a);
    ((blended + 127) / 255) as u8
}

impl TenantLogo {
    /// Decode `bytes` as a PNG into an 8-bit-RGB tenant logo.
    ///
    /// Returns [`RenderError::LogoDecode`] on:
    /// - non-PNG bytes (corrupted file, JPG dropped at the convention
    ///   path with the wrong extension, etc.),
    /// - a PNG header reporting width or height greater than
    ///   [`MAX_LOGO_DIMENSION`] (PR-185 — decompression-bomb defence;
    ///   the check runs before the output buffer is allocated),
    /// - a PNG output buffer whose channel count after
    ///   `normalize_to_color8` is anything other than the four
    ///   recognised 8-bit families (defensive — the png crate's own
    ///   invariants prevent this, but we surface a loud error rather
    ///   than panic if a future png upgrade widens the output set).
    ///
    /// The caller — `print_invoice::load_tenant_logo` — downgrades any
    /// error to a `tracing::warn!` and falls back to a text-only header
    /// per the PR-185 brief: legal-document rendering must NEVER block
    /// on a branding asset. The "fail loud" surface still exists (the
    /// log line is operator-visible and points at the broken file), but
    /// the invoice render itself proceeds.
    ///
    /// PR-185 / Fix C — pixels with non-opaque alpha are composited
    /// against white at decode time (out = src·α + 255·(1-α)). The PDF
    /// pipeline cannot composite alpha at paint time (lopdf has no
    /// alpha mask in our render path); compositing here means that
    /// transparent edges integrate naturally with the white invoice
    /// page background rather than displaying whatever RGB happened to
    /// be encoded under α=0 (PNG optimisers commonly fill α=0 pixels
    /// with garbage RGB, which previously surfaced on the printed
    /// invoice as colored ghosts on the page).
    pub fn from_png_bytes(bytes: &[u8]) -> Result<Self, RenderError> {
        let mut decoder = png::Decoder::new(bytes);
        // PR-185 — explicit decode-buffer cap. Matched to
        // MAX_LOGO_DIMENSION² × 4 so legitimate logos at the cap still
        // decode; the explicit value defends against a future png-crate
        // upgrade that would relax the (currently 64 MiB) default.
        decoder.set_limits(png::Limits {
            bytes: MAX_PNG_DECODE_BYTES,
        });
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
        // PR-185 — dimension cap BEFORE the output_buffer_size()
        // allocation below. A 50000×50000 PNG header would otherwise
        // request ~10 GB of zeroed memory before the limits-cap above
        // had a chance to fire on the decode itself.
        if width > MAX_LOGO_DIMENSION || height > MAX_LOGO_DIMENSION {
            return Err(RenderError::LogoDecode(format!(
                "PNG dimensions {width}×{height} exceed MAX_LOGO_DIMENSION ({MAX_LOGO_DIMENSION})"
            )));
        }
        let mut buf = vec![0u8; reader.output_buffer_size()];
        let frame = reader
            .next_frame(&mut buf)
            .map_err(|e| RenderError::LogoDecode(format!("decode PNG frame: {e}")))?;
        let pixels = &buf[..frame.buffer_size()];

        let rgb_bytes = match frame.color_type {
            png::ColorType::Rgb => pixels.to_vec(),
            png::ColorType::Rgba => pixels
                .chunks_exact(4)
                .flat_map(|p| {
                    let a = p[3];
                    [
                        composite_over_white(p[0], a),
                        composite_over_white(p[1], a),
                        composite_over_white(p[2], a),
                    ]
                })
                .collect(),
            png::ColorType::Grayscale => pixels.iter().flat_map(|&g| [g, g, g]).collect(),
            png::ColorType::GrayscaleAlpha => pixels
                .chunks_exact(2)
                .flat_map(|p| {
                    let v = composite_over_white(p[0], p[1]);
                    [v, v, v]
                })
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
    fn rgba_transparent_pixels_composite_to_white() {
        // PR-185 / Fix C — α=0 pixels MUST be white in the decoded
        // buffer; the renderer cannot composite alpha at paint time,
        // so transparent regions in the source PNG would otherwise
        // display whatever RGB happened to be encoded under α (PNG
        // optimisers commonly fill these with garbage).
        let png = synth_png(2, 2, png::ColorType::Rgba, &[200, 100, 50, 0]);
        let logo = TenantLogo::from_png_bytes(&png).expect("decode");
        assert_eq!(logo.rgb_bytes.len(), 2 * 2 * 3);
        assert!(
            logo.rgb_bytes.chunks_exact(3).all(|p| p == [255, 255, 255]),
            "α=0 must composite to white; got {:?}",
            logo.rgb_bytes
        );
    }

    #[test]
    fn rgba_opaque_pixels_pass_rgb_through_unchanged() {
        // α=255 must be lossless — the integer composite formula
        // resolves to src for any src∈[0,255].
        let png = synth_png(2, 2, png::ColorType::Rgba, &[200, 100, 50, 255]);
        let logo = TenantLogo::from_png_bytes(&png).expect("decode");
        assert!(logo.rgb_bytes.chunks_exact(3).all(|p| p == [200, 100, 50]));
    }

    #[test]
    fn rgba_half_alpha_blends_with_white() {
        // α=128 produces ≈(src + 255)/2. For src=100, a=128:
        //   blended = 100·128 + 255·127 = 12800 + 32385 = 45185
        //   (45185 + 127) / 255 = 45312 / 255 = 177
        let png = synth_png(1, 1, png::ColorType::Rgba, &[100, 100, 100, 128]);
        let logo = TenantLogo::from_png_bytes(&png).expect("decode");
        assert_eq!(logo.rgb_bytes, vec![177, 177, 177]);
    }

    #[test]
    fn grayscale_alpha_transparent_pixels_composite_to_white() {
        // Same defence as RGBA, on the GrayscaleAlpha arm.
        let png = synth_png(1, 1, png::ColorType::GrayscaleAlpha, &[42, 0]);
        let logo = TenantLogo::from_png_bytes(&png).expect("decode");
        assert_eq!(logo.rgb_bytes, vec![255, 255, 255]);
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

    #[test]
    fn rejects_png_width_above_cap() {
        // PR-185 / Fix B — a structurally honest PNG just beyond the
        // dimension cap on the width axis must surface LogoDecode
        // BEFORE the output buffer is allocated. The check runs on
        // `read_info()`'s reported width, so a (MAX+1)×1 grayscale PNG
        // is enough — the encoded buffer is ~4 KB, well within test
        // budgets, but the decoder would otherwise size the output
        // buffer at (MAX+1) bytes before our dimension guard fires.
        let png = synth_png(MAX_LOGO_DIMENSION + 1, 1, png::ColorType::Grayscale, &[0]);
        let result = TenantLogo::from_png_bytes(&png);
        match result {
            Err(RenderError::LogoDecode(msg)) => {
                assert!(
                    msg.contains("exceed MAX_LOGO_DIMENSION"),
                    "expected dim-cap message, got: {msg}"
                );
            }
            other => panic!("expected LogoDecode dimension error, got {other:?}"),
        }
    }

    #[test]
    fn rejects_png_height_above_cap() {
        // Same defence on the height axis.
        let png = synth_png(1, MAX_LOGO_DIMENSION + 1, png::ColorType::Grayscale, &[0]);
        let result = TenantLogo::from_png_bytes(&png);
        assert!(
            matches!(result, Err(RenderError::LogoDecode(ref m)) if m.contains("exceed MAX_LOGO_DIMENSION")),
            "expected dim-cap rejection, got {result:?}"
        );
    }

    /// S192 — extreme aspect ratio (1×N strip). PR-182 review's S176
    /// 🟢 named this as a gap: no test confirmed the decoder + the
    /// placement-matrix code path stayed sane for a single-pixel-wide
    /// vertical strip. The PR-185 dimension cap (`MAX_LOGO_DIMENSION`,
    /// 4096) bounds N; a 1×1024 fixture sits well under that and
    /// exercises the `chunks_exact(...)` paths for grayscale + the
    /// `(width as usize).saturating_mul(height as usize)` length-check
    /// without tripping the cap.
    ///
    /// The companion placement-matrix pin lives in
    /// `crates/invoice-pdf/src/lib.rs` (`place_logo` is module-private);
    /// this test focuses on the decoder contract: extreme aspect ratios
    /// decode without panic, divide-by-zero, or buffer-size mismatch.
    #[test]
    fn decodes_extreme_aspect_ratio_1xn_strip() {
        // 1 wide × 1024 tall grayscale strip. The aspect ratio (1:1024)
        // is two orders of magnitude beyond any sane operator-supplied
        // logo; this is the defensive surface for a corrupted file
        // whose header is nonetheless structurally honest.
        let png = synth_png(1, 1024, png::ColorType::Grayscale, &[128]);
        let logo = TenantLogo::from_png_bytes(&png).expect("decode 1×1024 strip");
        assert_eq!(logo.width, 1);
        assert_eq!(logo.height, 1024);
        assert_eq!(
            logo.rgb_bytes.len(),
            1 * 1024 * 3,
            "1×1024 grayscale must expand to 3 bytes per pixel"
        );
        // Pixel sanity — every byte is the source grayscale value
        // (no rogue zero-fill from a buffer-size off-by-one).
        assert!(
            logo.rgb_bytes.iter().all(|&b| b == 128),
            "every byte must round-trip the source grayscale value"
        );

        // The opposite axis — 1024×1 horizontal strip — must also
        // decode cleanly. Exercises the same expand-grayscale path
        // but with width/height swapped.
        let png_h = synth_png(1024, 1, png::ColorType::Grayscale, &[200]);
        let logo_h = TenantLogo::from_png_bytes(&png_h).expect("decode 1024×1 strip");
        assert_eq!(logo_h.width, 1024);
        assert_eq!(logo_h.height, 1);
        assert_eq!(logo_h.rgb_bytes.len(), 1024 * 1 * 3);
    }

    #[test]
    fn composite_over_white_helper_matches_formula() {
        // Pin a few canonical points so the integer rounding stays
        // honest under future refactors.
        assert_eq!(composite_over_white(0, 0), 255); // transparent black → white
        assert_eq!(composite_over_white(255, 0), 255); // transparent white → white
        assert_eq!(composite_over_white(0, 255), 0); // opaque black passes through
        assert_eq!(composite_over_white(255, 255), 255); // opaque white passes through
        assert_eq!(composite_over_white(128, 255), 128); // opaque mid-gray passes through
                                                         // α=128 mid-blend: (0·128 + 255·127 + 127) / 255 = 32512/255 = 127
        assert_eq!(composite_over_white(0, 128), 127);
    }
}
