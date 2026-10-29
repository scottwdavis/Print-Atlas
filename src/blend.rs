//! Independently implemented embedded-preview reader; no scene decoding or external programs.
//!
//! Binary-layout references (not copied implementations): Blender's
//! scripts/modules/_blendfile_header.py, blenkernel/BKE_main.hh (char RGBA),
//! and blendthumb/src/blendthumb_extract.cc (bottom-up rows).
//! Only the prefix through TEST or ENDB is consumed; trailing scene/compression data is not validated.

use crate::types::Raster;
use anyhow::{Context, Result, anyhow, bail, ensure};
use image::{
    RgbaImage,
    imageops::{self, FilterType},
};
use std::fs::File;
use std::io::{self, BufReader, Cursor, Read};
use std::path::Path;

const SCAN_BYTES: u64 = 128 * 1024 * 1024;
const MAX_BLOCKS: usize = 65_536;
const MAX_SIDE: u32 = 4096;
const MAX_IMAGE_BYTES: u64 = 64 * 1024 * 1024;
const ZSTD_WINDOW_LOG: u32 = 26; // At most 64 MiB of decoder history; gzip uses 32 KiB.

#[derive(Clone, Copy)]
struct Limits {
    source_bytes: u64,
    scan_bytes: u64,
    blocks: usize,
    image_bytes: u64,
}

const LIMITS: Limits = Limits {
    source_bytes: SCAN_BYTES,
    scan_bytes: SCAN_BYTES,
    blocks: MAX_BLOCKS,
    image_bytes: MAX_IMAGE_BYTES,
};

/// Read only the embedded TEST thumbnail from a read-only .blend file.
/// Returns None at ENDB when no thumbnail exists. Unsupported headers, truncated
/// prefixes, corrupt previews and resource-limit violations are errors, not missing previews.
/// Both input and decompressed scanning are capped at 128 MiB, with at most 65,536
/// blocks, 4096 pixels per side and 64 MiB of RGBA pixels. Never upscales.
/// Files larger than the scan limit remain usable when TEST is near the start.
pub fn load_thumbnail(path: &Path, max_side: u32) -> Result<Option<Raster>> {
    let file = File::open(path).with_context(|| format!("Cannot open {}", path.display()))?;
    ensure!(
        file.metadata()?.is_file(),
        "Blender preview source is not a regular file"
    );
    load_stream(file, max_side, LIMITS)
}

// Bound bytes before buffering/decompression, as well as after decompression.
// In particular, empty/skippable compressed frames cannot consume unlimited input.
struct Budget<R> {
    inner: R,
    remaining: u64,
    label: &'static str,
}

impl<R: Read> Read for Budget<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        if self.remaining == 0 {
            return Err(io::Error::other(format!(
                "Blender {} byte limit exceeded",
                self.label
            )));
        }
        let len = out.len().min(self.remaining as usize);
        let count = self.inner.read(&mut out[..len])?;
        self.remaining -= count as u64;
        Ok(count)
    }
}

fn load_stream<R: Read + 'static>(
    source: R,
    max_side: u32,
    limits: Limits,
) -> Result<Option<Raster>> {
    ensure!(
        max_side > 0,
        "Blender thumbnail maximum side must be positive"
    );
    let mut source = Budget {
        inner: source,
        remaining: limits.source_bytes,
        label: "compressed/source scan",
    };
    let mut magic = [0; 4];
    exact(&mut source, &mut magic, "signature")?;
    let input = BufReader::new(Cursor::new(magic).chain(source));
    let decoded: Box<dyn Read> = if magic.starts_with(&[0x1f, 0x8b]) {
        Box::new(flate2::read::MultiGzDecoder::new(input))
    } else if magic == [0x28, 0xb5, 0x2f, 0xfd]
        || ((0x50..=0x5f).contains(&magic[0]) && magic[1..4] == [0x2a, 0x4d, 0x18])
    {
        // Decoder deliberately retains its default concatenated-frame behavior.
        let mut decoder = zstd::stream::read::Decoder::with_buffer(input)
            .map_err(|e| anyhow!("Cannot initialize Blender zstd decoder: {e}"))?;
        decoder
            .window_log_max(ZSTD_WINDOW_LOG)
            .map_err(|e| anyhow!("Cannot bound Blender zstd decoder window: {e}"))?;
        Box::new(decoder)
    } else {
        Box::new(input)
    };
    let mut stream = Budget {
        inner: decoded,
        remaining: limits.scan_bytes,
        label: "decompressed scan",
    };
    read_preview(&mut stream, max_side, limits)
}

fn exact(reader: &mut impl Read, out: &mut [u8], what: &str) -> Result<()> {
    reader
        .read_exact(out)
        .map_err(|e| anyhow!("Cannot read Blender {what} (truncated, corrupt or over limit): {e}"))
}

#[derive(Clone, Copy)]
struct Layout {
    little: bool,
    modern: bool,
    block_size: usize,
}

impl Layout {
    fn i32(self, bytes: &[u8]) -> i32 {
        let bytes: [u8; 4] = bytes.try_into().expect("four-byte field");
        if self.little {
            i32::from_le_bytes(bytes)
        } else {
            i32::from_be_bytes(bytes)
        }
    }

    fn length(self, bytes: &[u8]) -> Result<u64> {
        let value = if self.modern {
            i64::from_le_bytes(bytes[16..24].try_into().expect("eight-byte field"))
        } else {
            i64::from(self.i32(&bytes[4..8]))
        };
        ensure!(
            value >= 0,
            "Malformed Blender block: negative payload length"
        );
        Ok(value as u64)
    }
}

fn read_layout(reader: &mut impl Read) -> Result<Layout> {
    let mut header = [0; 17];
    exact(reader, &mut header[..12], "file header")?;
    ensure!(
        &header[..7] == b"BLENDER",
        "Unsupported format: not a Blender file or supported gzip/zstd stream"
    );
    if matches!(header[7], b'_' | b'-') {
        ensure!(
            matches!(header[8], b'v' | b'V'),
            "Unsupported Blender endian marker"
        );
        ensure!(
            header[9..12].iter().all(u8::is_ascii_digit),
            "Malformed Blender version"
        );
        return Ok(Layout {
            little: header[8] == b'v',
            modern: false,
            block_size: if header[7] == b'_' { 20 } else { 24 },
        });
    }
    ensure!(
        &header[7..9] == b"17",
        "Unsupported Blender file header size or pointer marker"
    );
    ensure!(
        header[9] == b'-' && &header[10..12] == b"01",
        "Unsupported Blender file format version (only 0 and 1 are supported)"
    );
    exact(reader, &mut header[12..17], "extended file header")?;
    ensure!(
        header[12] == b'v',
        "Unsupported Blender format-1 endian marker"
    );
    ensure!(
        header[13..17].iter().all(u8::is_ascii_digit),
        "Malformed Blender version"
    );
    Ok(Layout {
        little: true,
        modern: true,
        block_size: 32,
    })
}

fn read_preview(reader: &mut impl Read, max_side: u32, limits: Limits) -> Result<Option<Raster>> {
    let layout = read_layout(reader)?;
    let mut block = [0; 32];
    let mut discard = [0; 8192];
    for _ in 0..limits.blocks {
        exact(reader, &mut block[..8], "block header")?;
        // Historic writers may terminate with only the eight-byte ENDB header.
        if !layout.modern && &block[..4] == b"ENDB" {
            ensure!(
                layout.length(&block)? == 0,
                "Malformed Blender ENDB payload length"
            );
            return Ok(None);
        }
        exact(reader, &mut block[8..layout.block_size], "block header")?;
        let length = layout.length(&block)?;
        ensure!(
            length <= limits.scan_bytes,
            "Blender block exceeds the decompressed scan byte limit"
        );
        if &block[..4] == b"ENDB" {
            ensure!(length == 0, "Malformed Blender ENDB payload length");
            return Ok(None);
        }
        if &block[..4] == b"TEST" {
            ensure!(
                length >= 8,
                "Malformed Blender TEST block: missing dimensions"
            );
            let mut shape = [0; 8];
            exact(reader, &mut shape, "thumbnail dimensions")?;
            let width = layout.i32(&shape[..4]);
            let height = layout.i32(&shape[4..]);
            ensure!(
                width > 0 && height > 0,
                "Malformed Blender thumbnail: dimensions must be positive"
            );
            let (width, height) = (width as u32, height as u32);
            ensure!(
                width <= MAX_SIDE && height <= MAX_SIDE,
                "Blender thumbnail exceeds the 4096-pixel side limit"
            );
            let bytes = u64::from(width) * u64::from(height) * 4;
            ensure!(
                bytes <= limits.image_bytes,
                "Blender thumbnail exceeds the RGBA allocation limit"
            );
            ensure!(
                length == bytes + 8,
                "Malformed Blender TEST block: RGBA payload length does not match dimensions"
            );
            let mut pixels = Vec::new();
            pixels
                .try_reserve_exact(bytes as usize)
                .context("Cannot allocate Blender thumbnail pixels")?;
            pixels.resize(bytes as usize, 0);
            // Read each stored bottom-up row directly into its final top-down slot.
            let stride = width as usize * 4;
            for row in pixels.chunks_exact_mut(stride).rev() {
                exact(reader, row, "thumbnail RGBA pixels")?;
            }
            if width <= max_side && height <= max_side {
                return Ok(Some(Raster {
                    width,
                    height,
                    rgba: pixels,
                }));
            }
            let longest = width.max(height);
            let scaled = |side: u32| {
                ((u64::from(side) * u64::from(max_side) + u64::from(longest) / 2)
                    / u64::from(longest))
                .max(1) as u32
            };
            let image = RgbaImage::from_raw(width, height, pixels)
                .ok_or_else(|| anyhow!("Malformed Blender RGBA image"))?;
            let image =
                imageops::resize(&image, scaled(width), scaled(height), FilterType::Triangle);
            return Ok(Some(Raster {
                width: image.width(),
                height: image.height(),
                rgba: image.into_raw(),
            }));
        }
        // Skip scene blocks in constant space, including on non-seekable compressed streams.
        let mut remaining = length;
        while remaining > 0 {
            let count = remaining.min(discard.len() as u64) as usize;
            exact(reader, &mut discard[..count], "skipped block payload")?;
            remaining -= count as u64;
        }
    }
    bail!("Blender preview scan exceeds the 65,536 block limit")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};

    #[derive(Clone, Copy)]
    enum Format {
        Legacy { pointer: usize, little: bool },
        Modern,
    }

    impl Format {
        fn little(self) -> bool {
            !matches!(self, Self::Legacy { little: false, .. })
        }

        fn int(self, value: i32) -> [u8; 4] {
            if self.little() {
                value.to_le_bytes()
            } else {
                value.to_be_bytes()
            }
        }

        fn header(self) -> Vec<u8> {
            match self {
                Self::Modern => b"BLENDER17-01v0500".to_vec(),
                Self::Legacy { pointer, little } => {
                    let mut bytes = b"BLENDER".to_vec();
                    bytes.extend([
                        if pointer == 4 { b'_' } else { b'-' },
                        if little { b'v' } else { b'V' },
                    ]);
                    bytes.extend(b"300");
                    bytes
                }
            }
        }

        fn block(self, code: &[u8; 4], payload: &[u8]) -> Vec<u8> {
            let mut block = code.to_vec();
            match self {
                Self::Modern => {
                    block.extend(0_i32.to_le_bytes());
                    block.extend(0_u64.to_le_bytes());
                    block.extend((payload.len() as i64).to_le_bytes());
                    block.extend(1_i64.to_le_bytes());
                }
                Self::Legacy { pointer, .. } => {
                    block.extend(self.int(payload.len() as i32));
                    block.resize(8 + pointer, 0);
                    block.extend(self.int(0));
                    block.extend(self.int(1));
                }
            }
            block.extend(payload);
            block
        }

        fn preview(self) -> Vec<u8> {
            let mut bytes = self.header();
            bytes.extend(self.block(b"REND", &[1, 2, 3, 4]));
            let mut pixels = self.int(2).to_vec();
            pixels.extend(self.int(2));
            // Bottom row: blue/white. Top row: red/green. Bytes are RGBA, not endian-swapped words.
            pixels.extend([
                0, 0, 255, 64, 255, 255, 255, 128, 255, 0, 0, 192, 0, 255, 0, 255,
            ]);
            bytes.extend(self.block(b"TEST", &pixels));
            bytes.extend(self.block(b"ENDB", &[]));
            bytes
        }
    }

    fn decode(bytes: Vec<u8>, side: u32) -> Result<Option<Raster>> {
        load_stream(Cursor::new(bytes), side, LIMITS)
    }

    const MODERN: Format = Format::Modern;

    #[test]
    fn all_layouts_preserve_orientation_color_and_alpha() {
        for format in [
            Format::Legacy {
                pointer: 4,
                little: true,
            },
            Format::Legacy {
                pointer: 4,
                little: false,
            },
            Format::Legacy {
                pointer: 8,
                little: true,
            },
            Format::Legacy {
                pointer: 8,
                little: false,
            },
            MODERN,
        ] {
            let raster = decode(format.preview(), 100).unwrap().unwrap();
            assert_eq!((raster.width, raster.height), (2, 2));
            assert_eq!(
                raster.rgba,
                [
                    255, 0, 0, 192, 0, 255, 0, 255, 0, 0, 255, 64, 255, 255, 255, 128
                ]
            );
        }
    }

    #[test]
    fn downscales_without_upscaling() {
        let raster = decode(MODERN.preview(), 1).unwrap().unwrap();
        assert_eq!((raster.width, raster.height, raster.rgba.len()), (1, 1, 4));
        assert!(decode(MODERN.preview(), 0).is_err());
    }

    #[test]
    fn preserves_aspect_ratio_for_nonsquare_preview() {
        let mut bytes = MODERN.header();
        let mut payload = MODERN.int(4).to_vec();
        payload.extend(MODERN.int(2));
        payload.extend([255, 0, 0, 255].repeat(8));
        bytes.extend(MODERN.block(b"TEST", &payload));
        let raster = decode(bytes, 2).unwrap().unwrap();
        assert_eq!((raster.width, raster.height), (2, 1));
        assert_eq!(raster.rgba, [255, 0, 0, 255].repeat(2));
    }

    #[test]
    fn handles_short_reads_when_detecting_compression() {
        struct ShortReader(Cursor<Vec<u8>>);
        impl Read for ShortReader {
            fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
                let count = out.len().min(1);
                self.0.read(&mut out[..count])
            }
        }
        let source = MODERN.preview();
        let compressed = zstd::stream::encode_all(Cursor::new(&source), 1).unwrap();
        for bytes in [source, compressed] {
            assert!(
                load_stream(ShortReader(Cursor::new(bytes)), 2, LIMITS)
                    .unwrap()
                    .is_some()
            );
        }
    }

    #[test]
    fn skips_scene_payload_and_handles_missing_preview() {
        let mut bytes = MODERN.header();
        bytes.extend(MODERN.block(b"SCEN", &[42; 100]));
        let preview = MODERN.preview();
        bytes.extend(&preview[17..]);
        assert!(decode(bytes, 2).unwrap().is_some());
        let mut bytes = MODERN.header();
        bytes.extend(MODERN.block(b"SCEN", &[42; 100]));
        bytes.extend(MODERN.block(b"ENDB", &[]));
        assert!(decode(bytes, 2).unwrap().is_none());
        let mut bytes = Format::Legacy {
            pointer: 4,
            little: false,
        }
        .header();
        bytes.extend(b"ENDB\0\0\0\0");
        assert!(decode(bytes, 2).unwrap().is_none());
    }

    #[test]
    fn gzip_and_concatenated_gzip_members() {
        let source = MODERN.preview();
        let mut compressed = Vec::new();
        for part in source.chunks(29) {
            let mut encoder =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            encoder.write_all(part).unwrap();
            compressed.extend(encoder.finish().unwrap());
        }
        let raster = decode(compressed, 2).unwrap().unwrap();
        assert_eq!(&raster.rgba[..4], &[255, 0, 0, 192]);
    }

    #[test]
    fn zstd_concatenated_frames_cross_header_and_pixel_boundaries() {
        let source = MODERN.preview();
        let mut compressed = Vec::new();
        // Start with a legal skippable frame, as well as an empty normal frame.
        compressed.extend(0x184d2a50_u32.to_le_bytes());
        compressed.extend(3_u32.to_le_bytes());
        compressed.extend([1, 2, 3]);
        compressed.extend(zstd::stream::encode_all(Cursor::new([]), 1).unwrap());
        for part in source.chunks(13) {
            compressed.extend(zstd::stream::encode_all(Cursor::new(part), 1).unwrap());
        }
        let raster = decode(compressed, 2).unwrap().unwrap();
        assert_eq!(&raster.rgba[..4], &[255, 0, 0, 192]);
    }

    #[test]
    fn rejects_truncation_at_every_prefix_boundary() {
        let bytes = MODERN.preview();
        let thumbnail_end = bytes.len() - 32;
        for end in 0..thumbnail_end {
            assert!(
                decode(bytes[..end].to_vec(), 2).is_err(),
                "accepted prefix of {end} bytes"
            );
        }
        assert!(
            decode(bytes[..thumbnail_end].to_vec(), 2)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn rejects_unsupported_and_malformed_headers() {
        for header in [
            b"BLENDER18-01v0500".as_slice(),
            b"BLENDER17-02v0500",
            b"BLENDER17-01V0500",
            b"BLENDER17-01v05xx",
            b"BLENDER_X300",
            b"not a blend file!",
        ] {
            assert!(decode(header.to_vec(), 2).is_err());
        }
        let mut bytes = MODERN.header();
        let mut block = MODERN.block(b"SCEN", &[]);
        block[16..24].copy_from_slice(&(-1_i64).to_le_bytes());
        bytes.extend(block);
        assert!(
            decode(bytes, 2)
                .unwrap_err()
                .to_string()
                .contains("negative")
        );
    }

    #[test]
    fn rejects_bad_test_dimensions_and_lengths() {
        for (width, height, extra) in [
            (0, 1, 4),
            (-1, 1, 4),
            (1, 0, 4),
            (4097, 1, 4),
            (i32::MAX, i32::MAX, 0),
            (2, 2, 15),
            (2, 2, 17),
        ] {
            let mut bytes = MODERN.header();
            let mut payload = MODERN.int(width).to_vec();
            payload.extend(MODERN.int(height));
            payload.resize(8 + extra, 0);
            bytes.extend(MODERN.block(b"TEST", &payload));
            assert!(decode(bytes, 2).is_err());
        }
        let mut bytes = MODERN.header();
        bytes.extend(MODERN.block(b"TEST", &[0; 7]));
        assert!(decode(bytes, 2).is_err());
    }

    #[test]
    fn bounds_source_decompressed_bytes_blocks_and_allocations() {
        let bytes = MODERN.preview();
        let compressed = zstd::stream::encode_all(Cursor::new(&bytes), 1).unwrap();
        for (source, limits, message) in [
            (
                bytes.clone(),
                Limits {
                    source_bytes: 20,
                    ..LIMITS
                },
                "source scan",
            ),
            (
                compressed,
                Limits {
                    scan_bytes: 45,
                    ..LIMITS
                },
                "decompressed scan",
            ),
            (
                bytes.clone(),
                Limits {
                    blocks: 1,
                    ..LIMITS
                },
                "block limit",
            ),
            (
                bytes.clone(),
                Limits {
                    image_bytes: 15,
                    ..LIMITS
                },
                "allocation limit",
            ),
        ] {
            let error = load_stream(Cursor::new(source), 2, limits).unwrap_err();
            assert!(error.to_string().contains(message), "{error:#}");
        }
        let mut bytes = MODERN.header();
        let mut block = MODERN.block(b"SCEN", &[]);
        block[16..24].copy_from_slice(&((SCAN_BYTES + 1) as i64).to_le_bytes());
        bytes.extend(block);
        assert!(
            decode(bytes, 2)
                .unwrap_err()
                .to_string()
                .contains("scan byte limit")
        );
    }

    #[test]
    fn rejects_corrupt_compression_and_excessive_zstd_window() {
        assert!(decode(vec![0x1f, 0x8b, 0, 0], 2).is_err());
        assert!(decode(vec![0x28, 0xb5, 0x2f, 0xfd, 0], 2).is_err());
        // Zstd frame descriptor: no content size; window descriptor 0x90 requests 256 MiB.
        let frame = vec![0x28, 0xb5, 0x2f, 0xfd, 0x00, 0x90, 0x01, 0x00, 0x00];
        assert!(decode(frame, 2).is_err());
    }

    #[test]
    fn path_api_leaves_original_unchanged() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let bytes = MODERN.preview();
        std::fs::write(file.path(), &bytes).unwrap();
        assert!(load_thumbnail(file.path(), 2).unwrap().is_some());
        assert_eq!(std::fs::read(file.path()).unwrap(), bytes);
    }
}
