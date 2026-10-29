use crate::types::{FileKind, Raster};
use anyhow::{Context, Result, anyhow, bail, ensure};
use image::{DynamicImage, ImageDecoder, ImageReader};
use pdfium_render::prelude::*;
use resvg::{tiny_skia, usvg};
use std::fs::File;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const SOURCE_LIMIT: usize = 64 * 1024 * 1024;
const TEXT_LIMIT: usize = 256 * 1024;
const DECODE_PIXELS: u64 = 32 * 1024 * 1024;
const DECODE_BYTES: u64 = 256 * 1024 * 1024;
const OUTPUT_SIDE: u32 = 4096;
static PDFIUM_LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug, Clone)]
pub enum Preview {
    Image(Raster),
    Text {
        text: String,
        truncated: bool,
        encoding: String,
    },
    Pdf(PdfPage),
    Other,
}

#[derive(Debug, Clone)]
pub struct PdfPage {
    pub raster: Raster,
    pub text: String,
    pub page_count: usize,
    pub page: usize,
}

pub fn load_file(path: &Path, kind: FileKind, max_side: u32) -> Result<Preview> {
    match kind {
        FileKind::Image => {
            let bytes = read_bounded(path, SOURCE_LIMIT, true)?;
            let raster = if path
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| ext.eq_ignore_ascii_case("svg"))
            {
                decode_svg(&bytes, max_side)?
            } else {
                decode_image(&bytes, max_side)?
            };
            Ok(Preview::Image(raster))
        }
        FileKind::Document => {
            let bytes = read_bounded(path, TEXT_LIMIT, false)?;
            decode_text(
                &bytes[..bytes.len().min(TEXT_LIMIT)],
                bytes.len() > TEXT_LIMIT,
            )
        }
        FileKind::Pdf => Ok(Preview::Pdf(load_pdf_page(path, 0, max_side)?)),
        _ => Ok(Preview::Other),
    }
}

fn read_bounded(path: &Path, limit: usize, complete: bool) -> Result<Vec<u8>> {
    let file = File::open(path).with_context(|| format!("Cannot open {}", path.display()))?;
    let metadata = file.metadata()?;
    ensure!(metadata.is_file(), "Preview source is not a regular file");
    if complete && metadata.len() > limit as u64 {
        bail!("This file is too large to preview (64 MiB maximum). Open it in its associated app.");
    }
    let mut bytes = Vec::with_capacity((metadata.len().min(limit as u64 + 1)) as usize);
    file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(
        !complete || bytes.len() <= limit,
        "This file grew beyond the 64 MiB preview limit"
    );
    Ok(bytes)
}

fn dimensions(width: f64, height: f64, max_side: u32) -> Result<(u32, u32)> {
    ensure!(
        width.is_finite() && height.is_finite() && width > 0.0 && height > 0.0,
        "Invalid preview dimensions"
    );
    let side = max_side.clamp(1, OUTPUT_SIDE) as f64;
    let scale = (side / width.max(height)).min(1.0);
    Ok((
        (width * scale).round().max(1.0) as u32,
        (height * scale).round().max(1.0) as u32,
    ))
}

fn check_decoded_dimensions(width: u32, height: u32, bytes: u64) -> Result<()> {
    ensure!(width > 0 && height > 0, "Image has zero dimensions");
    ensure!(
        u64::from(width) * u64::from(height) <= DECODE_PIXELS && bytes <= DECODE_BYTES,
        "Image exceeds the decoded preview limit (32 megapixels / 256 MiB)"
    );
    Ok(())
}

fn image_reader(bytes: &[u8]) -> Result<ImageReader<Cursor<&[u8]>>> {
    let mut reader = ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(DECODE_PIXELS as u32);
    limits.max_image_height = Some(DECODE_PIXELS as u32);
    limits.max_alloc = Some(DECODE_BYTES);
    reader.limits(limits);
    Ok(reader)
}

/// Decode the first frame of supported native raster formats, including embedded 3MF plates.
pub fn decode_image(bytes: &[u8], max_side: u32) -> Result<Raster> {
    ensure!(
        bytes.len() <= SOURCE_LIMIT,
        "Image source exceeds the 64 MiB preview limit"
    );
    if matches!(image::guess_format(bytes), Ok(image::ImageFormat::Avif)) {
        return decode_avif(bytes, max_side);
    }
    let mut decoder = image_reader(bytes)?
        .into_decoder()
        .context("Unsupported or invalid image")?;
    let (width, height) = decoder.dimensions();
    check_decoded_dimensions(width, height, decoder.total_bytes())?;
    let orientation = decoder.orientation()?;
    let mut image = DynamicImage::from_decoder(decoder).context("Cannot decode image")?;
    image.apply_orientation(orientation);
    raster_from_image(image, max_side)
}

fn decode_avif(bytes: &[u8], max_side: u32) -> Result<Raster> {
    validate_avif_boxes(bytes)?;
    // avif-parse has debug assertions on malformed nested box state. Keep all
    // third-party parsing/decoding inside this boundary so an invalid external
    // image returns an error instead of terminating a preview worker.
    std::panic::catch_unwind(|| decode_avif_inner(bytes, max_side)).map_err(|_| {
        anyhow!("AVIF decoding failed: malformed image triggered an internal parser error")
    })?
}

fn validate_avif_boxes(bytes: &[u8]) -> Result<()> {
    let mut offset = 0;
    while offset < bytes.len() {
        let remaining = &bytes[offset..];
        ensure!(remaining.len() >= 8, "Invalid AVIF: truncated box header");
        let size = u32::from_be_bytes(remaining[..4].try_into()?);
        let (size, header_size) = match size {
            0 => (remaining.len() as u64, 8),
            1 => {
                ensure!(
                    remaining.len() >= 16,
                    "Invalid AVIF: truncated extended box header"
                );
                (u64::from_be_bytes(remaining[8..16].try_into()?), 16)
            }
            size => (u64::from(size), 8),
        };
        ensure!(
            size >= header_size,
            "Invalid AVIF: box is smaller than its header"
        );
        ensure!(
            size <= remaining.len() as u64,
            "Invalid AVIF: truncated box contents"
        );
        offset += usize::try_from(size)?;
    }
    Ok(())
}

fn decode_avif_inner(bytes: &[u8], max_side: u32) -> Result<Raster> {
    // Inspect AV1 sequence headers for BOTH color and alpha before allocating a frame.
    // Checking the container's ispe dimensions alone would trust attacker-controlled metadata.
    {
        let data = avif_parse::AvifData::from_reader(&mut Cursor::new(bytes))
            .context("Invalid AVIF container")?;
        let primary = data
            .primary_item_metadata()
            .context("Invalid AVIF color sequence header")?;
        for metadata in std::iter::once(primary).chain(data.alpha_item_metadata()?) {
            let width = metadata.max_frame_width.get();
            let height = metadata.max_frame_height.get();
            check_decoded_dimensions(width, height, u64::from(width) * u64::from(height) * 8)?;
        }
    }
    let decoded = avif_decode::Decoder::from_avif(bytes)?
        .to_image()
        .context("Cannot decode AVIF")?;
    // imgref can expose padded rows; iterate pixels rather than assuming a contiguous stride.
    macro_rules! convert {
        ($image:expr, $pixel:ident, $channels:expr) => {{
            let image = $image;
            let width = u32::try_from(image.width())?;
            let height = u32::try_from(image.height())?;
            check_decoded_dimensions(width, height, u64::from(width) * u64::from(height) * 4)?;
            let mut rgba = Vec::with_capacity(width as usize * height as usize * 4);
            for $pixel in image.pixels() {
                rgba.extend_from_slice(&$channels);
            }
            let image = image::RgbaImage::from_raw(width, height, rgba)
                .context("Invalid decoded AVIF stride")?;
            raster_from_image(DynamicImage::ImageRgba8(image), max_side)
        }};
    }
    fn to_u8(value: u16) -> u8 {
        ((u32::from(value) * 255 + 32767) / 65535) as u8
    }
    match decoded {
        avif_decode::Image::Rgb8(image) => convert!(image, p, [p.r, p.g, p.b, 255]),
        avif_decode::Image::Rgba8(image) => convert!(image, p, [p.r, p.g, p.b, p.a]),
        avif_decode::Image::Rgb16(image) => {
            convert!(image, p, [to_u8(p.r), to_u8(p.g), to_u8(p.b), 255])
        }
        avif_decode::Image::Rgba16(image) => {
            convert!(image, p, [to_u8(p.r), to_u8(p.g), to_u8(p.b), to_u8(p.a)])
        }
        avif_decode::Image::Gray8(image) => {
            convert!(image, p, [p.value(), p.value(), p.value(), 255])
        }
        avif_decode::Image::Gray16(image) => convert!(
            image,
            p,
            [to_u8(p.value()), to_u8(p.value()), to_u8(p.value()), 255]
        ),
    }
}

fn raster_from_image(image: DynamicImage, max_side: u32) -> Result<Raster> {
    let (width, height) = dimensions(
        f64::from(image.width()),
        f64::from(image.height()),
        max_side,
    )?;
    let image = if image.width() != width || image.height() != height {
        image.resize_exact(width, height, image::imageops::FilterType::Triangle)
    } else {
        image
    };
    Ok(Raster {
        width,
        height,
        rgba: image.into_rgba8().into_raw(),
    })
}

fn decode_svg(bytes: &[u8], max_side: u32) -> Result<Raster> {
    // SVG is parsed as declarative geometry, never as HTML or a browser document.
    let xml = std::str::from_utf8(bytes).context("SVG must contain valid UTF-8 XML")?;
    let mut options = usvg::Options::default();
    options.fontdb_mut().load_system_fonts();
    let resource_error: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let errors = Arc::clone(&resource_error);
    options.image_href_resolver.resolve_string = Box::new(move |_, _| {
        if let Ok(mut error) = errors.lock() {
            *error = Some(
                "SVG external image references are not loaded; embed the image instead".into(),
            );
        }
        None
    });
    let errors = Arc::clone(&resource_error);
    let default_resolver = usvg::ImageHrefResolver::default_data_resolver();
    options.image_href_resolver.resolve_data = Box::new(move |mime, data, options| {
        let validation = (|| -> Result<()> {
            ensure!(
                data.len() <= SOURCE_LIMIT,
                "Embedded SVG image exceeds the source limit"
            );
            if mime == "image/svg+xml" {
                ensure!(
                    std::str::from_utf8(&data).is_ok(),
                    "Compressed/non-UTF8 embedded SVG is unsupported"
                );
            } else {
                let decoder = image_reader(&data)?.into_decoder()?;
                let (width, height) = decoder.dimensions();
                check_decoded_dimensions(width, height, decoder.total_bytes())?;
            }
            Ok(())
        })();
        if let Err(error) = validation {
            if let Ok(mut message) = errors.lock() {
                *message = Some(error.to_string());
            }
            return None;
        }
        default_resolver(mime, data, options)
    });
    let tree = usvg::Tree::from_str(xml, &options).context("Cannot parse SVG")?;
    if let Some(error) = resource_error
        .lock()
        .map_err(|_| anyhow!("SVG resource lock failed"))?
        .take()
    {
        bail!("{error}");
    }
    let size = tree.size();
    let (width, height) = dimensions(f64::from(size.width()), f64::from(size.height()), max_side)?;
    let mut pixmap =
        tiny_skia::Pixmap::new(width, height).context("Cannot allocate SVG preview")?;
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(
            width as f32 / size.width(),
            height as f32 / size.height(),
        ),
        &mut pixmap.as_mut(),
    );
    let mut rgba = pixmap.take();
    // tiny-skia stores premultiplied color; egui's shared Raster contract is straight RGBA.
    for pixel in rgba.as_chunks_mut::<4>().0 {
        let alpha = u32::from(pixel[3]);
        if alpha > 0 && alpha < 255 {
            for channel in &mut pixel[..3] {
                *channel = ((u32::from(*channel) * 255 + alpha / 2) / alpha).min(255) as u8;
            }
        }
    }
    Ok(Raster {
        width,
        height,
        rgba,
    })
}

fn decode_text(mut bytes: &[u8], mut truncated: bool) -> Result<Preview> {
    let (text, encoding) = if bytes.starts_with(&[0xff, 0xfe]) || bytes.starts_with(&[0xfe, 0xff]) {
        let little = bytes[0] == 0xff;
        bytes = &bytes[2..];
        if !bytes.len().is_multiple_of(2) {
            ensure!(truncated, "Invalid UTF-16 document: incomplete code unit");
            bytes = &bytes[..bytes.len() - 1];
        }
        let mut units: Vec<u16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| {
                if little {
                    u16::from_le_bytes([pair[0], pair[1]])
                } else {
                    u16::from_be_bytes([pair[0], pair[1]])
                }
            })
            .collect();
        if truncated
            && units
                .last()
                .is_some_and(|unit| (0xd800..=0xdbff).contains(unit))
        {
            units.pop();
        }
        (
            String::from_utf16(&units).context("Invalid UTF-16 document")?,
            if little { "utf-16le" } else { "utf-16be" },
        )
    } else {
        if bytes.starts_with(&[0xef, 0xbb, 0xbf]) {
            bytes = &bytes[3..];
        }
        let text = match std::str::from_utf8(bytes) {
            Ok(text) => text,
            Err(error) if truncated && error.error_len().is_none() => {
                std::str::from_utf8(&bytes[..error.valid_up_to()])?
            }
            Err(_) => bail!("This document is binary or is not valid UTF-8 / BOM-marked UTF-16"),
        };
        (text.to_owned(), "utf-8")
    };
    ensure!(
        !text
            .chars()
            .any(|c| c == '\0' || (c.is_control() && !matches!(c, '\n' | '\r' | '\t' | '\u{c}'))),
        "This document contains binary control characters and cannot be previewed as text"
    );
    let mut text = text;
    if text.len() > TEXT_LIMIT {
        let mut end = TEXT_LIMIT;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        truncated = true;
    }
    Ok(Preview::Text {
        text,
        truncated,
        encoding: encoding.into(),
    })
}

fn pdfium_candidates() -> Vec<PathBuf> {
    let mut paths = Vec::with_capacity(3);
    if let Ok(executable) = std::env::current_exe()
        && let Some(directory) = executable.parent()
    {
        paths.push(directory.join("runtime/pdfium.dll"));
        paths.push(directory.join("pdfium.dll"));
    }
    #[cfg(debug_assertions)]
    paths.push(Path::new(env!("CARGO_MANIFEST_DIR")).join("runtime/pdfium.dll"));
    paths
}

fn bind_pdfium() -> Result<Pdfium> {
    let mut failures = Vec::new();
    for path in pdfium_candidates() {
        if !path.is_file() {
            continue;
        }
        match Pdfium::bind_to_library(&path) {
            Ok(bindings) => return Ok(Pdfium::new(bindings)),
            Err(error) => failures.push(format!("{}: {error}", path.display())),
        }
    }
    bail!(
        "PDFium runtime is missing or incompatible. Reinstall Print Atlas, or run scripts/fetch-pdfium.ps1 for development. {}",
        failures.join("; ")
    )
}

pub fn load_pdf_page(path: &Path, page: usize, max_side: u32) -> Result<PdfPage> {
    let bytes = read_bounded(path, SOURCE_LIMIT, true)?;
    ensure!(bytes.starts_with(b"%PDF-"), "Invalid PDF signature");
    // PDFium's initialization/destruction is process-global. Serialize the whole document
    // lifetime, including teardown, instead of racing separate preview worker instances.
    let _guard = PDFIUM_LOCK
        .lock()
        .map_err(|_| anyhow!("PDFium preview worker lock failed"))?;
    let pdfium = bind_pdfium()?;
    let document = pdfium
        .load_pdf_from_byte_slice(&bytes, None)
        .map_err(|error| match error {
            PdfiumError::PdfiumLibraryInternalError(PdfiumInternalError::PasswordError) => {
                anyhow!("This PDF requires a password; open it in its associated PDF application")
            }
            _ => anyhow!("Cannot open PDF: {error}"),
        })?;
    let page_count = usize::from(document.pages().len());
    ensure!(
        page < page_count,
        "PDF page {} is outside this {}-page document",
        page.saturating_add(1),
        page_count
    );
    let source = document.pages().get(
        page.try_into()
            .context("PDF page index exceeds PDFium's limit")?,
    )?;
    let page_width = f64::from(source.width().value);
    let page_height = f64::from(source.height().value);
    // PDF points are not raster pixels. Render small vector pages at the requested
    // resolution too, so zooming does not magnify a 72-DPI bitmap.
    let scale = (f64::from(max_side.clamp(1, OUTPUT_SIDE)) / page_width.max(page_height)).max(1.0);
    let (width, height) = dimensions(page_width * scale, page_height * scale, max_side)?;
    // Fixed dimensions avoid all implicit PDF DPI scaling; maximum is 4096² = 16 MP.
    // No form events, links, actions or JavaScript APIs are called. Packaged PDFium has no V8.
    let config = PdfRenderConfig::new()
        .set_fixed_size(width as i32, height as i32)
        .render_form_data(false)
        .render_annotations(true)
        .limit_render_image_cache_size(true);
    let bitmap = source
        .render_with_config(&config)
        .context("Cannot render PDF page")?;
    let text = source.text().context("Cannot extract PDF page text")?.all();
    Ok(PdfPage {
        raster: Raster {
            width,
            height,
            rgba: bitmap.as_rgba_bytes(),
        },
        text,
        page_count,
        page,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Fixture(PathBuf);
    impl Fixture {
        fn new(extension: &str, bytes: &[u8]) -> Self {
            let path = std::env::temp_dir().join(format!(
                "print-atlas-preview-{}-{}.{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed),
                extension
            ));
            std::fs::write(&path, bytes).unwrap();
            Self(path)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    fn text(preview: Preview) -> (String, bool, String) {
        match preview {
            Preview::Text {
                text,
                truncated,
                encoding,
            } => (text, truncated, encoding),
            _ => panic!("expected text"),
        }
    }

    #[test]
    fn text_boms_and_truncation_preserve_unicode() {
        let content = "<script>not executed</script>\nRésumé 🖨";
        let mut utf8_bom = vec![0xef, 0xbb, 0xbf];
        utf8_bom.extend_from_slice(content.as_bytes());
        assert_eq!(
            text(decode_text(&utf8_bom, false).unwrap()),
            (content.into(), false, "utf-8".into())
        );
        assert_eq!(
            text(decode_text(&[0xff, 0xfe, b'a', 0, 0x3d, 0xd8], true).unwrap()).0,
            "a"
        );
        for little in [true, false] {
            let mut bytes = if little {
                vec![0xff, 0xfe]
            } else {
                vec![0xfe, 0xff]
            };
            for unit in content.encode_utf16() {
                bytes.extend(if little {
                    unit.to_le_bytes()
                } else {
                    unit.to_be_bytes()
                });
            }
            let (actual, truncated, encoding) = text(decode_text(&bytes, false).unwrap());
            assert_eq!(actual, content);
            assert!(!truncated);
            assert_eq!(encoding, if little { "utf-16le" } else { "utf-16be" });
        }
        let mut bytes = vec![b'a'; TEXT_LIMIT - 1];
        bytes.extend("🖨end".as_bytes());
        let fixture = Fixture::new("txt", &bytes);
        let (actual, truncated, _) = text(load_file(&fixture.0, FileKind::Document, 64).unwrap());
        assert!(truncated);
        assert_eq!(actual.len(), TEXT_LIMIT - 1);
        assert!(!actual.contains('\u{fffd}'));
        assert!(decode_text(&[0, 1, 0xff], false).is_err());
        assert!(decode_text(&[0xff, 0xfe, 0x00, 0xd8], false).is_err());
    }

    #[test]
    fn native_raster_formats_decode_and_bound_dimensions() {
        for format in [
            image::ImageFormat::Png,
            image::ImageFormat::Jpeg,
            image::ImageFormat::WebP,
            image::ImageFormat::Bmp,
            image::ImageFormat::Gif,
            image::ImageFormat::Tiff,
            image::ImageFormat::Ico,
        ] {
            let image = DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
                24,
                12,
                image::Rgb([220, 80, 20]),
            ));
            // ICO's PNG payload must be RGBA; other formats retain their RGB
            // fixture (notably JPEG, whose encoder does not accept RGBA).
            let image = if format == image::ImageFormat::Ico {
                DynamicImage::ImageRgba8(image.into_rgba8())
            } else {
                image
            };
            let mut output = Cursor::new(Vec::new());
            image.write_to(&mut output, format).unwrap();
            let raster = decode_image(output.get_ref(), 8).unwrap();
            assert_eq!((raster.width, raster.height), (8, 4), "{format:?}");
            assert_eq!(raster.rgba.len(), 8 * 4 * 4);
            assert!(
                raster
                    .rgba
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .all(|pixel| pixel[0] > pixel[2])
            );
        }
        assert!(check_decoded_dimensions(1_000_000, 1_000_000, 4_000_000_000_000).is_err());
        assert!(decode_image(b"not an image", 128).is_err());
        assert_eq!(dimensions(10000.0, 5000.0, u32::MAX).unwrap(), (4096, 2048));
    }

    #[test]
    fn genuine_avif_decodes_without_os_codec_and_rejects_corrupt_payload() {
        // libavif white_1x1.avif, immutable git blob:
        // https://api.github.com/repos/AOMediaCodec/libavif/git/blobs/9d5c466444de5680b539673b26f5c770e98dacc0
        // A one-pixel constant-white test image, not an executable or synthetic decoder stub.
        let hex = concat!(
            "00000020667479706176696600000000617669666d6966316d6961664d413141",
            "000000f26d657461000000000000002868646c72000000000000000070696374",
            "0000000000000000000000006c696261766966000000000e7069746d00000000",
            "00010000001e696c6f6300000000440000010001000000010000011a00000017",
            "0000002869696e660000000000010000001a696e666502000000000100006176",
            "3031436f6c6f72000000006a697072700000004b6970636f0000001469737065",
            "00000000000000010000000100000010706978690000000003080808000000",
            "0c617631438120000000000013636f6c726e636c780001000d00068000000017",
            "69706d610000000000000001000104010283040000001f6d64617412000a0738",
            "00061010d069320a1ff03fffffc40000af70"
        );
        let bytes: Vec<u8> = hex
            .as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect();
        let raster = decode_image(&bytes, 32).unwrap();
        assert_eq!((raster.width, raster.height), (1, 1));
        assert!(raster.rgba[..3].iter().all(|&value| value >= 250));
        assert_eq!(raster.rgba[3], 255);
        // Every truncated suffix must return an error, never unwind out of the
        // decoder and strand the background preview job.
        for end in 0..bytes.len() {
            let result = std::panic::catch_unwind(|| decode_image(&bytes[..end], 32));
            assert!(
                result.is_ok(),
                "decoder panicked for AVIF prefix length {end}"
            );
            assert!(
                result.unwrap().is_err(),
                "accepted truncated AVIF prefix length {end}"
            );
        }
        // A damaged nested box passes root size checks, exercising the guarded
        // third-party parser rather than only the root truncation check.
        let mut malformed = bytes.clone();
        malformed[44..48].copy_from_slice(&u32::MAX.to_be_bytes());
        let result = std::panic::catch_unwind(|| decode_image(&malformed, 32));
        assert!(result.is_ok());
        assert!(result.unwrap().is_err());
    }

    #[test]
    fn svg_is_rasterized_with_straight_alpha_and_external_resources_blocked() {
        let svg = br##"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="50"><rect width="100" height="50" fill="#ff0000" opacity="0.5"/></svg>"##;
        let raster = decode_svg(svg, 20).unwrap();
        assert_eq!((raster.width, raster.height), (20, 10));
        assert_eq!(raster.rgba[0], 255);
        assert!((126..=129).contains(&raster.rgba[3]));
        let external = br#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10"><image href="file:///secret.png" width="10" height="10"/></svg>"#;
        assert!(decode_svg(external, 10).is_err());
    }

    #[test]
    fn source_limits_and_pdf_signature_are_checked_before_decoder_loading() {
        let fixture = Fixture::new("png", b"");
        File::options()
            .write(true)
            .open(&fixture.0)
            .unwrap()
            .set_len(SOURCE_LIMIT as u64 + 1)
            .unwrap();
        assert!(
            load_file(&fixture.0, FileKind::Image, 64)
                .unwrap_err()
                .to_string()
                .contains("64 MiB")
        );
        assert!(
            load_pdf_page(&fixture.0, 0, 64)
                .unwrap_err()
                .to_string()
                .contains("64 MiB")
        );
        let invalid = Fixture::new("pdf", b"not a PDF");
        assert!(
            load_pdf_page(&invalid.0, 0, 64)
                .unwrap_err()
                .to_string()
                .contains("signature")
        );
    }

    #[test]
    fn packaged_pdfium_renders_real_multipage_pdf_and_selectable_text() {
        // This test requires the redistributable just like the shipped application.
        // A missing DLL is intentionally a failure, not a silently skipped PDF test.
        let mut bytes = b"%PDF-1.4\n".to_vec();
        let stream = b"BT /F1 18 Tf 15 60 Td (Print Atlas PDF text) Tj ET";
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            "<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 >>".to_string(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 100] /Resources << /Font << /F1 5 0 R >> >> /Contents 6 0 R >>".to_string(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 200] /Resources << /Font << /F1 5 0 R >> >> /Contents 6 0 R >>".to_string(),
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string(),
            format!("<< /Length {} >>\nstream\n{}\nendstream", stream.len(), std::str::from_utf8(stream).unwrap()),
        ];
        let mut offsets = Vec::new();
        for (i, object) in objects.iter().enumerate() {
            offsets.push(bytes.len());
            bytes.extend(format!("{} 0 obj\n{}\nendobj\n", i + 1, object).as_bytes());
        }
        let xref = bytes.len();
        bytes.extend(format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1).as_bytes());
        for offset in offsets {
            bytes.extend(format!("{offset:010} 00000 n \n").as_bytes());
        }
        bytes.extend(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                objects.len() + 1
            )
            .as_bytes(),
        );
        let fixture = Fixture::new("pdf", &bytes);
        let first = load_pdf_page(&fixture.0, 0, 128).unwrap();
        assert_eq!((first.page, first.page_count), (0, 2));
        assert_eq!((first.raster.width, first.raster.height), (128, 64));
        assert!(first.text.contains("Print Atlas PDF text"));
        assert!(
            first
                .raster
                .rgba
                .as_chunks::<4>()
                .0
                .iter()
                .any(|pixel| pixel[0] < 128)
        );
        let second = load_pdf_page(&fixture.0, 1, 128).unwrap();
        assert_eq!((second.raster.width, second.raster.height), (64, 128));
        let enlarged = load_pdf_page(&fixture.0, 0, 512).unwrap();
        assert_eq!((enlarged.raster.width, enlarged.raster.height), (512, 256));
        assert!(load_pdf_page(&fixture.0, 2, 128).is_err());
    }
}
