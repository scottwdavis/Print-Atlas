//! Streaming STL and 3MF geometry loading. No slicer or web runtime is required.
use crate::types::{Model, Vertex};
use anyhow::{Context, Result, bail, ensure};
use quick_xml::{
    Reader,
    events::{BytesStart, Event},
};
use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
    fs::File,
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    path::Path,
};
use zip::ZipArchive;

const DEFAULT_RGB: [f32; 3] = [0.72, 0.76, 0.82];
const MAX_METADATA: u64 = 32 * 1024 * 1024;
const MAX_LINE: u64 = 8 * 1024 * 1024;
const IDENTITY: Matrix = [1., 0., 0., 0., 0., 1., 0., 0., 0., 0., 1., 0.];
type Matrix = [f64; 12];
type Point = [f64; 3];
type Triangle = [Point; 3];
type Rgb = [f32; 3];

#[derive(Default)]
struct Appearance {
    palette: Vec<[u8; 3]>,
    palette_seen: HashSet<[u8; 3]>,
    warnings: Vec<String>,
    warning_seen: HashSet<String>,
}
impl Appearance {
    fn warn(&mut self, text: impl Into<Cow<'static, str>>) {
        let text = text.into();
        if self.warning_seen.contains(text.as_ref()) {
            return;
        }
        let text = text.into_owned();
        self.warning_seen.insert(text.clone());
        self.warnings.push(text);
    }
    fn remember(&mut self, rgb: Rgb) {
        let c = rgb.map(|v| (v * 255.).round().clamp(0., 255.) as u8);
        if self.palette_seen.insert(c) {
            self.palette.push(c);
        }
    }
}
struct Bounds {
    min: Point,
    max: Point,
}
impl Bounds {
    fn new() -> Self {
        Self {
            min: [f64::INFINITY; 3],
            max: [f64::NEG_INFINITY; 3],
        }
    }
    fn include(&mut self, p: Point) -> Result<()> {
        for (k, value) in p.into_iter().enumerate() {
            ensure!(
                (value as f32).is_finite(),
                "Model contains non-finite coordinates or coordinates outside the Float32 display range."
            );
            self.min[k] = self.min[k].min(value);
            self.max[k] = self.max[k].max(value);
        }
        Ok(())
    }
}
fn finish(
    vertices: Vec<Vertex>,
    count: u64,
    bounds: Bounds,
    appearance: Appearance,
) -> Result<Model> {
    ensure!(
        count > 0 && !vertices.is_empty() && bounds.min[0].is_finite(),
        "No printable mesh triangles were found."
    );
    Ok(Model {
        vertices,
        triangle_count: count,
        bounds_min: bounds.min.map(|v| v as f32),
        bounds_max: bounds.max.map(|v| v as f32),
        palette: appearance.palette,
        warnings: appearance.warnings,
    })
}
fn allocate_vertices(total: u64) -> Result<Vec<Vertex>> {
    let count = total
        .checked_mul(3)
        .and_then(|count| usize::try_from(count).ok())
        .context("Model display vertex count exceeds addressable memory")?;
    let mut vertices = Vec::new();
    vertices
        .try_reserve_exact(count)
        .context("Not enough addressable memory for model display geometry")?;
    Ok(vertices)
}
fn number(text: &str, label: &str) -> Result<f64> {
    let value: f64 = text
        .trim()
        .parse()
        .with_context(|| format!("Invalid {label}: {text}"))?;
    ensure!(value.is_finite(), "Invalid {label}: {text}");
    Ok(value)
}
fn integer(text: &str, label: &str) -> Result<u32> {
    let value = number(text, label)?;
    ensure!(
        value >= 0. && value <= u32::MAX as f64 && value.fract() == 0.,
        "Invalid {label}."
    );
    Ok(value as u32)
}
fn parse_color(text: &str, appearance: &mut Appearance) -> Option<Rgb> {
    let text = text.trim();
    if !text.starts_with('#')
        || ![7, 9].contains(&text.len())
        || !text.as_bytes()[1..].iter().all(u8::is_ascii_hexdigit)
    {
        return None;
    }
    if text.len() == 9 && !text[7..].eq_ignore_ascii_case("ff") {
        appearance.warn("Transparent material colors are shown opaque.");
    }
    Some([
        u8::from_str_radix(&text[1..3], 16).ok()? as f32 / 255.,
        u8::from_str_radix(&text[3..5], 16).ok()? as f32 / 255.,
        u8::from_str_radix(&text[5..7], 16).ok()? as f32 / 255.,
    ])
}

/// Load every printable triangle, including painted 3MF subdivisions. Memory
/// grows with the complete expanded mesh; failed display-buffer allocations are
/// reported as errors rather than silently dropping triangles.
/// Bounds and triangle_count describe the original printable source mesh.
pub fn load_model_full(path: &Path) -> Result<Model> {
    match path
        .extension()
        .and_then(|v| v.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "stl" => load_stl(path),
        "3mf" => load_3mf(path),
        _ => bail!("Only STL and 3MF model files are supported."),
    }
}

fn read_ascii<R: BufRead>(
    mut reader: R,
    mut visit: impl FnMut(Triangle, u64) -> Result<()>,
) -> Result<u64> {
    let mut line = String::new();
    let mut in_facet = false;
    let mut saw_solid = false;
    let mut count = 0;
    let mut vertices = 0;
    let mut xyz = [[0.; 3]; 3];
    loop {
        line.clear();
        // Take caps a malicious line before allocation, rather than checking afterward.
        let bytes = reader
            .by_ref()
            .take(MAX_LINE + 1)
            .read_line(&mut line)
            .context("Invalid ASCII STL text")?;
        ensure!(
            bytes as u64 <= MAX_LINE,
            "ASCII STL contains an excessively long line."
        );
        if bytes == 0 {
            break;
        }
        let mut words = line.split_whitespace();
        while let Some(word) = words.next() {
            if word.eq_ignore_ascii_case("solid") {
                saw_solid = true;
            } else if word.eq_ignore_ascii_case("facet") {
                ensure!(
                    !in_facet,
                    "ASCII STL contains a nested or unterminated facet."
                );
                in_facet = true;
                vertices = 0;
            } else if word.eq_ignore_ascii_case("endfacet") {
                ensure!(
                    in_facet && vertices == 3,
                    "ASCII STL facet must contain exactly three vertices."
                );
                visit(xyz, count)?;
                count += 1;
                in_facet = false;
            } else if word.eq_ignore_ascii_case("vertex") {
                ensure!(
                    in_facet && vertices < 3,
                    "ASCII STL vertex is outside a valid facet."
                );
                for coordinate in &mut xyz[vertices] {
                    *coordinate = number(
                        words.next().context("Missing STL vertex coordinate")?,
                        "STL vertex coordinate",
                    )?;
                }
                vertices += 1;
            }
        }
    }
    ensure!(
        saw_solid && !in_facet && count > 0,
        "Invalid or empty ASCII STL."
    );
    Ok(count)
}
/// Source geometry information returned after visiting every STL facet.
#[derive(Debug, PartialEq)]
pub struct StlInfo {
    pub triangle_count: u64,
    pub bounds_min: [f32; 3],
    pub bounds_max: [f32; 3],
}

/// Visit all STL facets in source order without retaining geometry. Coordinates
/// and colors match `load_model_full`, including degenerate triangles. Callback
/// errors stop parsing immediately; a successful return validates the full file.
pub fn visit_stl(path: &Path, mut visit: impl FnMut([Vertex; 3]) -> Result<()>) -> Result<StlInfo> {
    StlReader::open(path)?.scan(|triangle, _| visit(triangle))
}

struct StlReader {
    file: File,
    size: u64,
    declared: u64,
    binary: bool,
    trailing: bool,
    object_color: Option<Rgb>,
}

impl StlReader {
    fn open(path: &Path) -> Result<Self> {
        let mut file = File::open(path)?;
        let size = file.metadata()?.len();
        let mut header = [0; 84];
        let header_len = size.min(84) as usize;
        file.read_exact(&mut header[..header_len])?;
        let declared = if header_len == 84 {
            u32::from_le_bytes(header[80..84].try_into().unwrap()) as u64
        } else {
            0
        };
        let expected = 84 + declared * 50;
        let prefix = String::from_utf8_lossy(&header[..header_len]);
        let ascii = prefix
            .split_whitespace()
            .next()
            .is_some_and(|w| w.eq_ignore_ascii_case("solid"));
        let binary = header_len == 84 && (expected == size || !ascii || header.contains(&0));
        ensure!(
            !binary || expected <= size,
            "Binary STL is truncated: header declares {declared} triangles."
        );
        let object_color = header[..80]
            .windows(6)
            .position(|v| v == b"COLOR=")
            .filter(|p| p + 10 <= 80)
            .map(|p| {
                [
                    header[p + 6] as f32 / 255.,
                    header[p + 7] as f32 / 255.,
                    header[p + 8] as f32 / 255.,
                ]
            });
        Ok(Self {
            file,
            size,
            declared,
            binary,
            trailing: binary && expected < size,
            object_color,
        })
    }

    fn scan(
        &mut self,
        mut visit: impl FnMut([Vertex; 3], Option<Rgb>) -> Result<()>,
    ) -> Result<StlInfo> {
        let mut bounds = Bounds::new();
        let mut emit = |triangle: Triangle, color: Option<Rgb>| {
            for p in triangle {
                bounds.include(p)?;
            }
            visit(
                triangle.map(|p| Vertex {
                    position: p.map(|v| v as f32),
                    color: color.unwrap_or(DEFAULT_RGB),
                }),
                color,
            )
        };
        let count = if self.binary {
            self.file.seek(SeekFrom::Start(84))?;
            let mut reader = BufReader::with_capacity(50 * 4096, &mut self.file);
            let mut facet = [0u8; 50];
            for _ in 0..self.declared {
                reader
                    .read_exact(&mut facet)
                    .context("Binary STL changed or was truncated while reading")?;
                let attr = u16::from_le_bytes([facet[48], facet[49]]);
                let color = if let Some(color) = self.object_color {
                    Some(if attr & 0x8000 != 0 {
                        color
                    } else {
                        [
                            (attr & 31) as f32 / 31.,
                            ((attr >> 5) & 31) as f32 / 31.,
                            ((attr >> 10) & 31) as f32 / 31.,
                        ]
                    })
                } else if attr & 0x8000 != 0 {
                    Some([
                        ((attr >> 10) & 31) as f32 / 31.,
                        ((attr >> 5) & 31) as f32 / 31.,
                        (attr & 31) as f32 / 31.,
                    ])
                } else {
                    None
                };
                let triangle = std::array::from_fn(|vertex| {
                    std::array::from_fn(|k| {
                        let offset = 12 + vertex * 12 + k * 4;
                        f32::from_le_bytes(facet[offset..offset + 4].try_into().unwrap()) as f64
                    })
                });
                emit(triangle, color)?;
            }
            self.declared
        } else {
            self.file.seek(SeekFrom::Start(0))?;
            read_ascii(
                BufReader::with_capacity(128 * 1024, &mut self.file),
                |triangle, _| emit(triangle, None),
            )?
        };
        ensure!(
            self.file.metadata()?.len() == self.size,
            "STL changed while reading."
        );
        ensure!(count > 0, "No printable mesh triangles were found.");
        Ok(StlInfo {
            triangle_count: count,
            bounds_min: bounds.min.map(|v| v as f32),
            bounds_max: bounds.max.map(|v| v as f32),
        })
    }
}

fn load_stl(path: &Path) -> Result<Model> {
    let mut source = StlReader::open(path)?;
    let info = source.scan(|_, _| Ok(()))?;
    let mut vertices = allocate_vertices(info.triangle_count)?;
    let mut appearance = Appearance::default();
    if source.trailing {
        appearance.warn("Binary STL contains trailing data after its declared triangles.");
    }
    let second = source.scan(|triangle, color| {
        if let Some(color) = color {
            appearance.remember(color);
        }
        vertices.extend_from_slice(&triangle);
        Ok(())
    })?;
    ensure!(second == info, "STL changed while reading.");
    if !appearance.palette.is_empty() {
        appearance.warn("STL colors use the non-standard Magics/VisCAM convention; STL has no standard color encoding.");
    }
    Ok(Model {
        vertices,
        triangle_count: info.triangle_count,
        bounds_min: info.bounds_min,
        bounds_max: info.bounds_max,
        palette: appearance.palette,
        warnings: appearance.warnings,
    })
}

fn local(name: &[u8]) -> &[u8] {
    name.rsplit(|v| *v == b':').next().unwrap_or(name)
}
fn validate_attributes(tag: &BytesStart<'_>) -> Result<()> {
    for (index, value) in tag.attributes().enumerate() {
        let value = value?;
        let key = value.key.as_ref();
        if key == b"xmlns" || key.starts_with(b"xmlns:") {
            continue;
        }
        value.unescape_value()?;
        for prior in tag.attributes().take(index) {
            let prior = prior?;
            let prior_key = prior.key.as_ref();
            if prior_key == b"xmlns" || prior_key.starts_with(b"xmlns:") {
                continue;
            }
            ensure!(
                local(prior_key) != local(key),
                "Duplicate XML attribute: {}",
                String::from_utf8_lossy(local(key))
            );
        }
    }
    Ok(())
}
fn attr<'a>(tag: &'a BytesStart<'_>, key: &[u8]) -> Result<Option<Cow<'a, str>>> {
    for value in tag.attributes() {
        let value = value?;
        if local(value.key.as_ref()) == key {
            return Ok(Some(value.unescape_value()?));
        }
    }
    Ok(None)
}
fn required<'a>(tag: &'a BytesStart<'_>, key: &[u8]) -> Result<Cow<'a, str>> {
    attr(tag, key)?.with_context(|| format!("Missing {} attribute", String::from_utf8_lossy(key)))
}
fn uint_attr(tag: &BytesStart<'_>, key: &[u8]) -> Result<u32> {
    integer(&required(tag, key)?, &String::from_utf8_lossy(key))
}
fn optional_uint(tag: &BytesStart<'_>, key: &[u8]) -> Result<Option<u32>> {
    attr(tag, key)?
        .map(|v| integer(&v, "property index"))
        .transpose()
}

fn archive_path(reference: &str, from: &str) -> Result<String> {
    let bytes = reference.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            ensure!(i + 2 < bytes.len(), "Invalid encoded 3MF component path.");
            let digit = |c: u8| {
                (c as char)
                    .to_digit(16)
                    .context("Invalid encoded 3MF component path")
            };
            decoded.push((digit(bytes[i + 1])? * 16 + digit(bytes[i + 2])?) as u8);
            i += 3;
        } else {
            decoded.push(bytes[i]);
            i += 1;
        }
    }
    let decoded = String::from_utf8(decoded)
        .context("Invalid UTF-8 3MF component path")?
        .replace('\\', "/");
    ensure!(
        !decoded.contains(['\0', '?', '#', ':']),
        "External network/filesystem component references are not supported."
    );
    let mut segments: Vec<&str> = if decoded.starts_with('/') {
        Vec::new()
    } else {
        from.rsplit_once('/')
            .map(|(dir, _)| dir.split('/').collect())
            .unwrap_or_default()
    };
    for part in decoded.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                ensure!(
                    segments.pop().is_some(),
                    "3MF component path escapes the archive."
                );
            }
            _ => segments.push(part),
        }
    }
    Ok(segments.join("/"))
}
struct Archive {
    zip: ZipArchive<File>,
    entries: HashMap<String, usize>,
}
impl Archive {
    fn open(path: &Path) -> Result<Self> {
        let mut zip = ZipArchive::new(File::open(path)?)?;
        let mut entries = HashMap::new();
        for index in 0..zip.len() {
            let entry = zip.by_index(index)?;
            if entry.is_dir() {
                continue;
            }
            let name = archive_path(&format!("/{}", entry.name()), "")?;
            ensure!(
                entries.insert(name.clone(), index).is_none(),
                "Duplicate ZIP entry: {name}"
            );
        }
        Ok(Self { zip, entries })
    }
    fn bytes(&mut self, name: &str, cap: u64) -> Result<Option<Vec<u8>>> {
        let Some(index) = self.entries.get(name).copied() else {
            return Ok(None);
        };
        let mut entry = self.zip.by_index(index)?;
        ensure!(
            entry.size() <= cap,
            "3MF entry exceeds its safe size limit: {name}"
        );
        let mut data = Vec::with_capacity(entry.size() as usize);
        entry.by_ref().take(cap + 1).read_to_end(&mut data)?;
        ensure!(
            data.len() as u64 <= cap,
            "3MF entry exceeds its safe size limit: {name}"
        );
        Ok(Some(data))
    }
    fn text(&mut self, name: &str) -> Result<Option<String>> {
        self.bytes(name, MAX_METADATA)?
            .map(|data| {
                let mut text = String::from_utf8(data).context("3MF metadata is not UTF-8")?;
                if text.starts_with('\u{feff}') {
                    text.replace_range(..3, "");
                }
                Ok(text)
            })
            .transpose()
    }
}

/// Return all colored slicer plate images in numeric order, or one package preview.
/// Images remain encoded, allowing the preview loader to apply its dimension limits.
pub fn embedded_plates(path: &Path) -> Result<Vec<(String, Vec<u8>)>> {
    if !path
        .extension()
        .is_some_and(|v| v.eq_ignore_ascii_case("3mf"))
    {
        return Ok(Vec::new());
    }
    let mut archive = Archive::open(path)?;
    let mut plates = Vec::new();
    let mut fallback = Vec::new();
    for (name, index) in &archive.entries {
        let base = name.rsplit('/').next().unwrap_or(name).to_ascii_lowercase();
        if let Some(digits) = base
            .strip_prefix("plate_")
            .and_then(|v| v.strip_suffix(".png"))
        {
            if !digits.is_empty() && digits.bytes().all(|v| v.is_ascii_digit()) {
                plates.push((digits.parse::<u64>().unwrap_or(u64::MAX), name.clone()));
            }
        } else if [
            "thumbnail.png",
            "thumbnail.jpg",
            "thumbnail.jpeg",
            "preview.png",
            "preview.jpg",
            "preview.jpeg",
        ]
        .contains(&base.as_str())
        {
            fallback.push((*index, name.clone()));
        }
    }
    plates.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    fallback.sort_by_key(|v| v.0);
    let selected: Vec<String> = if plates.is_empty() {
        fallback.into_iter().take(1).map(|v| v.1).collect()
    } else {
        plates.into_iter().map(|v| v.1).collect()
    };
    let mut total = 0;
    let mut images = Vec::with_capacity(selected.len());
    for name in selected {
        let remaining = (96 * 1024 * 1024usize).saturating_sub(total);
        let data = archive
            .bytes(&name, (24 * 1024 * 1024).min(remaining) as u64)?
            .context("Missing embedded preview")?;
        total += data.len();
        if data.starts_with(b"\x89PNG\r\n\x1a\n") || data.starts_with(&[255, 216, 255]) {
            images.push((name.rsplit('/').next().unwrap_or(&name).to_owned(), data));
        }
    }
    Ok(images)
}

fn transform(text: Option<&str>) -> Result<Matrix> {
    let Some(text) = text.filter(|s| !s.is_empty()) else {
        return Ok(IDENTITY);
    };
    let mut values = [0.; 12];
    let mut words = text.split_whitespace();
    for value in &mut values {
        *value = number(
            words
                .next()
                .context("3MF transforms must contain 12 numbers")?,
            "3MF transform",
        )?;
    }
    ensure!(
        words.next().is_none(),
        "3MF transforms must contain 12 numbers."
    );
    Ok([
        values[0], values[3], values[6], values[9], values[1], values[4], values[7], values[10],
        values[2], values[5], values[8], values[11],
    ])
}
fn multiply(a: Matrix, b: Matrix) -> Matrix {
    let mut out = [0.; 12];
    for r in 0..3 {
        let k = r * 4;
        for c in 0..3 {
            out[k + c] = a[k] * b[c] + a[k + 1] * b[4 + c] + a[k + 2] * b[8 + c];
        }
        out[k + 3] = a[k] * b[3] + a[k + 1] * b[7] + a[k + 2] * b[11] + a[k + 3];
    }
    out
}
fn scale(value: f64) -> Matrix {
    [value, 0., 0., 0., 0., value, 0., 0., 0., 0., value, 0.]
}
fn point(m: Matrix, p: Point) -> Point {
    [
        m[0] * p[0] + m[1] * p[1] + m[2] * p[2] + m[3],
        m[4] * p[0] + m[5] * p[1] + m[6] * p[2] + m[7],
        m[8] * p[0] + m[9] * p[1] + m[10] * p[2] + m[11],
    ]
}

#[derive(Default, Clone)]
struct PartInfo {
    extruder: Option<f64>,
    printable: bool,
}
#[derive(Default)]
struct ObjectInfo {
    extruder: Option<f64>,
    parts: HashMap<u32, PartInfo>,
    ranges: Vec<(u32, u32, PartInfo)>,
}
#[derive(Default)]
struct Project {
    filaments: Vec<Option<Rgb>>,
    objects: HashMap<u32, ObjectInfo>,
}
fn xml_document<'a>(text: &'a str, label: &str) -> Result<roxmltree::Document<'a>> {
    let bytes = text.as_bytes();
    ensure!(
        !bytes
            .windows(9)
            .any(|v| v.eq_ignore_ascii_case(b"<!DOCTYPE"))
            && !bytes
                .windows(8)
                .any(|v| v.eq_ignore_ascii_case(b"<!ENTITY")),
        "DTD/entity declarations are not allowed in {label}."
    );
    roxmltree::Document::parse(text).with_context(|| format!("Invalid XML in {label}"))
}
fn metadata<'a, 'input>(node: roxmltree::Node<'a, 'input>, key: &str) -> Option<&'a str> {
    node.children()
        .find(|n| n.has_tag_name("metadata") && n.attribute("key") == Some(key))
        .and_then(|n| n.attribute("value").or_else(|| n.text()))
}
fn assigned(text: Option<&str>) -> Option<f64> {
    text.map(|value| {
        let value = value.trim();
        if value.is_empty() {
            0.
        } else {
            value.parse().unwrap_or(f64::NAN)
        }
    })
}
fn project_metadata(archive: &mut Archive, appearance: &mut Appearance) -> Result<Project> {
    let mut project = Project::default();
    if let Some(text) = archive.text("Metadata/project_settings.config")? {
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
            if let Some(values) = json
                .get("filament_colour")
                .or_else(|| json.get("filament_color"))
            {
                if let Some(values) = values.as_array() {
                    project.filaments = values
                        .iter()
                        .map(|v| v.as_str().and_then(|s| parse_color(s, appearance)))
                        .collect();
                } else if let Some(values) = values.as_str() {
                    project.filaments = values
                        .split(';')
                        .map(|v| parse_color(v, appearance))
                        .collect();
                }
            }
            if project.filaments.iter().any(Option::is_none) {
                appearance.warn("Some project filament colors are invalid or missing; affected surfaces use the neutral display color.");
            }
            if json
                .get("filament_multi_colour")
                .is_some_and(|v| v.to_string().contains('#'))
            {
                appearance.warn("Multi-color filament appearance is unsupported; its configured base filament color is shown.");
            }
        } else {
            appearance.warn(
                "Project settings JSON is malformed; standard model colors remain available.",
            );
        }
    }
    let config = match archive.text("Metadata/model_settings.config")? {
        Some(text) => Some(text),
        None => archive.text("Metadata/Slic3r_PE_model.config")?,
    };
    if let Some(text) = config {
        let tree = xml_document(&text, "model settings")?;
        for obj in tree
            .root_element()
            .children()
            .filter(|n| n.has_tag_name("object"))
        {
            let id = integer(
                obj.attribute("id")
                    .context("Missing configured object id")?,
                "configured object id",
            )?;
            let mut info = ObjectInfo {
                extruder: assigned(metadata(obj, "extruder")),
                ..Default::default()
            };
            for part in obj
                .children()
                .filter(|n| n.has_tag_name("part") || n.has_tag_name("volume"))
            {
                let kind = part
                    .attribute("subtype")
                    .or_else(|| metadata(part, "part_type"))
                    .or_else(|| metadata(part, "volume_type"))
                    .unwrap_or("")
                    .to_ascii_lowercase();
                let printable = ![
                    "negative_part",
                    "modifier_part",
                    "support_enforcer",
                    "support_blocker",
                    "negative_volume",
                    "parameter_modifier",
                    "1",
                    "2",
                    "3",
                    "4",
                ]
                .contains(&kind.as_str())
                    && metadata(part, "modifier") != Some("1");
                let part_info = PartInfo {
                    extruder: assigned(metadata(part, "extruder")),
                    printable,
                };
                if let (Some(first), Some(last)) =
                    (part.attribute("firstid"), part.attribute("lastid"))
                {
                    info.ranges.push((
                        integer(first, "first triangle id")?,
                        integer(last, "last triangle id")?,
                        part_info,
                    ));
                } else if let Some(id) = part.attribute("id") {
                    info.parts.insert(integer(id, "part id")?, part_info);
                }
            }
            project.objects.insert(id, info);
        }
    }
    Ok(project)
}

#[derive(Default, Clone, Copy)]
struct Properties {
    pid: Option<u32>,
    indices: [Option<u32>; 3],
}
struct Component {
    id: u32,
    file: String,
    matrix: Matrix,
    printable: bool,
}
struct Object {
    kind: String,
    properties: Properties,
    vertices: Vec<Point>,
    triangles: Vec<[u32; 3]>,
    // Property blocks are allocated only for triangles that carry appearance.
    properties_blocks: HashMap<usize, Box<[Properties; 1024]>>,
    paints: HashMap<usize, String>,
    components: Vec<Component>,
}
impl Object {
    fn properties(&self, index: usize) -> Properties {
        self.properties_blocks
            .get(&(index / 1024))
            .map(|b| b[index % 1024])
            .unwrap_or_default()
    }
}
struct Resource {
    kind: String,
    colors: Vec<Option<Rgb>>,
}
struct Document {
    unit: f64,
    objects: HashMap<u32, Object>,
    properties: HashMap<u32, Resource>,
    build: Vec<Component>,
    references: HashSet<String>,
}

fn component(tag: &BytesStart<'_>, filename: &str) -> Result<Component> {
    Ok(Component {
        id: uint_attr(tag, b"objectid")?,
        file: match attr(tag, b"path")? {
            Some(path) => archive_path(&path, filename)?,
            None => filename.to_owned(),
        },
        matrix: transform(attr(tag, b"transform")?.as_deref())?,
        printable: !matches!(attr(tag, b"printable")?.as_deref(), Some("0" | "false")),
    })
}
#[derive(Default)]
struct ModelParseState {
    object_id: Option<u32>,
    resource_id: Option<u32>,
    saw_model: bool,
}

fn open_tag(
    tag: &BytesStart<'_>,
    parent: &[u8],
    depth: usize,
    filename: &str,
    doc: &mut Document,
    state: &mut ModelParseState,
    appearance: &mut Appearance,
) -> Result<()> {
    let ModelParseState {
        object_id,
        resource_id,
        saw_model,
    } = state;
    let name = tag.local_name();
    let name = name.as_ref();
    if name == b"model" && depth == 0 {
        *saw_model = true;
        doc.unit = match attr(tag, b"unit")?.as_deref().unwrap_or("millimeter") {
            "micron" => 0.001,
            "millimeter" => 1.,
            "centimeter" => 10.,
            "inch" => 25.4,
            "foot" => 304.8,
            "meter" => 1000.,
            other => bail!("Unsupported 3MF unit: {other}"),
        };
    } else if name == b"object" && parent == b"resources" {
        let id = uint_attr(tag, b"id")?;
        ensure!(
            !doc.objects.contains_key(&id),
            "Duplicate 3MF object id {id}."
        );
        doc.objects.insert(
            id,
            Object {
                kind: attr(tag, b"type")?
                    .unwrap_or(Cow::Borrowed("model"))
                    .into_owned(),
                properties: Properties {
                    pid: optional_uint(tag, b"pid")?,
                    indices: [optional_uint(tag, b"pindex")?, None, None],
                },
                vertices: Vec::new(),
                triangles: Vec::new(),
                properties_blocks: HashMap::new(),
                paints: HashMap::new(),
                components: Vec::new(),
            },
        );
        *object_id = Some(id);
    } else if name == b"vertex" && parent == b"vertices" {
        if let Some(obj) = object_id.and_then(|id| doc.objects.get_mut(&id)) {
            obj.vertices.push([
                number(&required(tag, b"x")?, "vertex x")?,
                number(&required(tag, b"y")?, "vertex y")?,
                number(&required(tag, b"z")?, "vertex z")?,
            ]);
        }
    } else if name == b"triangle" && parent == b"triangles" {
        if let Some(obj) = object_id.and_then(|id| doc.objects.get_mut(&id)) {
            let index = obj.triangles.len();
            obj.triangles.push([
                uint_attr(tag, b"v1")?,
                uint_attr(tag, b"v2")?,
                uint_attr(tag, b"v3")?,
            ]);
            let prop = Properties {
                pid: optional_uint(tag, b"pid")?,
                indices: [
                    optional_uint(tag, b"p1")?,
                    optional_uint(tag, b"p2")?,
                    optional_uint(tag, b"p3")?,
                ],
            };
            ensure!(
                prop.pid != Some(u32::MAX) && !prop.indices.contains(&Some(u32::MAX)),
                "Triangle property index exceeds the supported range."
            );
            if prop.pid.is_some() || prop.indices.iter().any(Option::is_some) {
                obj.properties_blocks
                    .entry(index / 1024)
                    .or_insert_with(|| Box::new([Properties::default(); 1024]))[index % 1024] =
                    prop;
            }
            if let Some(paint) = attr(tag, b"paint_color")?.filter(|v| !v.is_empty()) {
                obj.paints.insert(index, paint.into_owned());
            }
            if attr(tag, b"slic3rpe")?.is_some() || attr(tag, b"mmu_segmentation")?.is_some() {
                appearance.warn("Unrecognized advanced painting attributes are not decoded; base material colors are shown.");
            }
        }
    } else if name == b"component" && parent == b"components" {
        if let Some(obj) = object_id.and_then(|id| doc.objects.get_mut(&id)) {
            let child = component(tag, filename)?;
            if child.file != filename {
                doc.references.insert(child.file.clone());
            }
            obj.components.push(child);
        }
    } else if name == b"item" && parent == b"build" {
        let child = component(tag, filename)?;
        if child.file != filename {
            doc.references.insert(child.file.clone());
        }
        doc.build.push(child);
    } else if parent == b"resources"
        && [
            b"colorgroup".as_slice(),
            b"basematerials",
            b"texture2dgroup",
            b"compositematerials",
            b"multiproperties",
        ]
        .contains(&name)
    {
        let id = uint_attr(tag, b"id")?;
        ensure!(
            !doc.properties.contains_key(&id),
            "Duplicate 3MF property resource id {id}."
        );
        let kind = std::str::from_utf8(name)?.to_owned();
        if kind != "colorgroup" && kind != "basematerials" {
            appearance.warn(format!("3MF {kind} appearance is unsupported; affected surfaces use their known base filament color or neutral display color."));
        }
        doc.properties.insert(
            id,
            Resource {
                kind,
                colors: Vec::new(),
            },
        );
        *resource_id = Some(id);
    } else if (name == b"color" || name == b"base")
        && let Some(resource) = resource_id.and_then(|id| doc.properties.get_mut(&id))
    {
        let value = attr(tag, b"color")?.or(attr(tag, b"displaycolor")?);
        let color = value.as_deref().and_then(|v| parse_color(v, appearance));
        if color.is_none() {
            appearance.warn(
                "A standard 3MF material color is invalid; affected surfaces use their base color.",
            );
        }
        resource.colors.push(color);
    }
    Ok(())
}
fn close_tag(name: &[u8], state: &mut ModelParseState) {
    if name == b"object" {
        state.object_id = None;
    }
    if [
        b"colorgroup".as_slice(),
        b"basematerials",
        b"texture2dgroup",
        b"compositematerials",
        b"multiproperties",
    ]
    .contains(&name)
    {
        state.resource_id = None;
    }
}
fn read_document<R: BufRead>(
    source: R,
    filename: &str,
    appearance: &mut Appearance,
) -> Result<Document> {
    let mut reader = Reader::from_reader(source);
    let mut buffer = Vec::with_capacity(4096);
    let mut stack: Vec<Vec<u8>> = Vec::new();
    let mut roots = 0;
    let mut state = ModelParseState::default();
    let mut doc = Document {
        unit: 1.,
        objects: HashMap::new(),
        properties: HashMap::new(),
        build: Vec::new(),
        references: HashSet::new(),
    };
    loop {
        buffer.clear();
        let event = reader
            .read_event_into(&mut buffer)
            .with_context(|| format!("Malformed XML in {filename}"))?;
        ensure!(
            event.len() <= 4 * 1024 * 1024,
            "3MF XML token exceeds the supported size."
        );
        let empty = matches!(&event, Event::Empty(_));
        match event {
            Event::Start(tag) | Event::Empty(tag) => {
                validate_attributes(&tag)?;
                if stack.is_empty() {
                    roots += 1;
                    ensure!(roots == 1, "Multiple XML document roots.");
                }
                open_tag(
                    &tag,
                    stack.last().map(|v| local(v)).unwrap_or_default(),
                    stack.len(),
                    filename,
                    &mut doc,
                    &mut state,
                    appearance,
                )?;
                if empty {
                    close_tag(tag.local_name().as_ref(), &mut state);
                } else {
                    stack.push(tag.name().as_ref().to_vec());
                    ensure!(stack.len() <= 256, "3MF XML nesting is excessively deep.");
                }
            }
            Event::End(tag) => {
                ensure!(
                    stack.pop().as_deref() == Some(tag.name().as_ref()),
                    "Mismatched XML closing tag."
                );
                close_tag(tag.local_name().as_ref(), &mut state);
            }
            Event::DocType(_) => bail!("XML declarations/DTDs are not supported in 3MF models."),
            Event::Eof => break,
            _ => {}
        }
    }
    ensure!(
        stack.is_empty() && roots == 1 && state.saw_model,
        "{filename} is a truncated or invalid 3MF model document."
    );
    for (id, object) in &doc.objects {
        ensure!(
            object
                .triangles
                .iter()
                .flatten()
                .all(|v| (*v as usize) < object.vertices.len()),
            "{filename}: object {id} references an out-of-range vertex."
        );
    }
    Ok(doc)
}

/// Decode Bambu's right-to-left nibble stream. Nodes and child vertices are kept
/// on a bounded-depth stack, never expanded into a retained tree.
fn painting(
    text: &str,
    geometry: Option<Triangle>,
    mut emit: impl FnMut(Triangle, u32) -> Result<()>,
) -> Result<u64> {
    ensure!(
        !text.is_empty() && text.len() <= 1_000_000 && text.bytes().all(|v| v.is_ascii_hexdigit()),
        "Invalid painting stream."
    );
    let mut bytes = text.bytes().rev();
    let nibble = |bytes: &mut std::iter::Rev<std::str::Bytes<'_>>| -> Result<u32> {
        Ok((bytes.next().context("Truncated painting stream")? as char)
            .to_digit(16)
            .unwrap())
    };
    // Three pending siblings per level plus the current node is the exact
    // maximum at the accepted depth. Fixed storage avoids one heap allocation
    // for every painted source triangle.
    let mut stack = [(None, 0u16); 3 * 128 + 1];
    stack[0] = (geometry, 0);
    let mut stack_len = 1;
    let mut leaves = 0;
    while stack_len > 0 {
        stack_len -= 1;
        let (vertices, depth) = stack[stack_len];
        let code = nibble(&mut bytes)?;
        let splits = code & 3;
        let side = (code >> 2) as usize;
        if splits == 0 {
            let mut state = side as u32;
            if side == 3 {
                loop {
                    let part = nibble(&mut bytes)?;
                    state += part;
                    ensure!(
                        state <= 4096,
                        "Painting filament state exceeds supported range."
                    );
                    if part != 15 {
                        break;
                    }
                }
            }
            leaves += 1;
            if let Some(vertices) = vertices {
                emit(vertices, state)?;
            }
        } else {
            ensure!(
                side <= 2 && (splits != 3 || side == 0) && depth < 128,
                "Unsupported or excessively deep painting subdivision."
            );
            let mut children = [None; 4];
            if let Some(v) = vertices {
                let a = v[side];
                let b = v[(side + 1) % 3];
                let c = v[(side + 2) % 3];
                let mid = |p: Point, q: Point| std::array::from_fn(|k| (p[k] + q[k]) / 2.);
                match splits {
                    1 => {
                        let bc = mid(b, c);
                        children[0] = Some([a, b, bc]);
                        children[1] = Some([bc, c, a]);
                    }
                    2 => {
                        let ab = mid(a, b);
                        let ca = mid(c, a);
                        children[0] = Some([a, ab, ca]);
                        children[1] = Some([ab, b, ca]);
                        children[2] = Some([b, c, ca]);
                    }
                    _ => {
                        let ab = mid(a, b);
                        let bc = mid(b, c);
                        let ca = mid(c, a);
                        children[0] = Some([a, ab, ca]);
                        children[1] = Some([ab, b, bc]);
                        children[2] = Some([bc, c, ca]);
                        children[3] = Some([ab, bc, ca]);
                    }
                }
            }
            // Highest child is popped first, matching Bambu's reverse serialization.
            for child in children.into_iter().take(splits as usize + 1) {
                stack[stack_len] = (child, depth + 1);
                stack_len += 1;
            }
        }
    }
    ensure!(
        bytes.next().is_none(),
        "Painting stream contains trailing nodes."
    );
    Ok(leaves)
}

struct Instance<'a> {
    doc: &'a Document,
    object: &'a Object,
    matrix: Matrix,
    context: Option<&'a ObjectInfo>,
    extruder: f64,
}
struct TraversalState<'a> {
    matrix: Matrix,
    context: Option<&'a ObjectInfo>,
    extruder: f64,
    part: Option<&'a PartInfo>,
    depth: usize,
}

struct Walker<'a> {
    docs: &'a HashMap<String, Document>,
    instances: Vec<Instance<'a>>,
    active: HashSet<(&'a str, u32)>,
}
impl<'a> Walker<'a> {
    fn walk(&mut self, file: &'a str, id: u32, state: TraversalState<'a>) -> Result<()> {
        let TraversalState {
            matrix,
            context,
            extruder: inherited,
            part,
            depth,
        } = state;
        ensure!(depth <= 128, "3MF component hierarchy is excessively deep.");
        ensure!(
            !self.active.contains(&(file, id)),
            "3MF contains a cyclic component reference."
        );
        let doc = self
            .docs
            .get(file)
            .with_context(|| format!("Missing 3MF component document {file}"))?;
        let object = doc
            .objects
            .get(&id)
            .with_context(|| format!("Missing 3MF object {id} in {file}"))?;
        let part = part.or_else(|| {
            if object.triangles.is_empty() {
                None
            } else {
                context.and_then(|v| v.parts.get(&id))
            }
        });
        if object.kind != "model" || part.is_some_and(|v| !v.printable) {
            return Ok(());
        }
        let extruder = part
            .and_then(|v| v.extruder)
            .filter(|v| *v != 0.)
            .unwrap_or(inherited);
        if !object.triangles.is_empty() {
            self.instances.push(Instance {
                doc,
                object,
                matrix,
                context,
                extruder,
            });
            ensure!(
                self.instances.len() <= 1_000_000,
                "3MF expands to too many mesh instances."
            );
        }
        self.active.insert((file, id));
        for child in &object.components {
            let child_doc = self
                .docs
                .get(&child.file)
                .with_context(|| format!("Missing 3MF component document {}", child.file))?;
            let child_matrix = multiply(
                multiply(matrix, child.matrix),
                scale(child_doc.unit / doc.unit),
            );
            self.walk(
                &child.file,
                child.id,
                TraversalState {
                    matrix: child_matrix,
                    context,
                    extruder,
                    part: context.and_then(|v| v.parts.get(&child.id)),
                    depth: depth + 1,
                },
            )?;
        }
        self.active.remove(&(file, id));
        Ok(())
    }
}
fn range_info<'a>(instance: &'a Instance<'_>, index: usize) -> Option<&'a PartInfo> {
    instance.context.and_then(|v| {
        v.ranges
            .iter()
            .find(|(first, last, _)| index as u64 >= *first as u64 && index as u64 <= *last as u64)
            .map(|v| &v.2)
    })
}
fn filament(project: &Project, slot: f64, appearance: &mut Appearance) -> Option<Rgb> {
    if slot == 0. {
        return None;
    }
    let color = if slot.is_finite()
        && slot >= 1.
        && slot.fract() == 0.
        && slot <= project.filaments.len() as f64
    {
        project.filaments[slot as usize - 1]
    } else {
        None
    };
    if let Some(color) = color {
        appearance.remember(color);
    } else {
        appearance.warn("Some assigned/painted filament slots have no usable project color; their surfaces use the neutral display color.");
    }
    color
}
fn standard_colors(
    doc: &Document,
    object: &Object,
    index: usize,
    appearance: &mut Appearance,
) -> Option<[Option<Rgb>; 3]> {
    let prop = object.properties(index);
    let pid = prop.pid.or(object.properties.pid)?;
    let Some(resource) = doc.properties.get(&pid) else {
        appearance.warn("A 3MF color/material property references a missing resource.");
        return None;
    };
    if resource.kind != "colorgroup" && resource.kind != "basematerials" {
        return None;
    }
    let first = prop.indices[0]
        .or(object.properties.indices[0])
        .unwrap_or(0);
    Some(std::array::from_fn(|v| {
        let index = prop.indices[v].unwrap_or(first);
        let color = resource.colors.get(index as usize).copied().flatten();
        if let Some(color) = color {
            appearance.remember(color);
        } else {
            appearance.warn(
                "A triangle references a missing material/color index; its base color is shown.",
            );
        }
        color
    }))
}
fn triangle(instance: &Instance<'_>, index: usize) -> Triangle {
    instance.object.triangles[index]
        .map(|v| point(instance.matrix, instance.object.vertices[v as usize]))
}
fn emit(vertices: &mut Vec<Vertex>, triangle: Triangle, colors: [Rgb; 3]) -> Result<()> {
    for k in 0..3 {
        ensure!(
            triangle[k].iter().all(|v| (*v as f32).is_finite()),
            "Model coordinates exceed Float32 display range."
        );
        vertices.push(Vertex {
            position: triangle[k].map(|v| v as f32),
            color: colors[k],
        });
    }
    Ok(())
}
fn load_3mf(path: &Path) -> Result<Model> {
    let mut archive = Archive::open(path)?;
    let mut appearance = Appearance::default();
    let project = project_metadata(&mut archive, &mut appearance)?;
    let mut root_file = "3D/3dmodel.model".to_owned();
    if let Some(text) = archive.text("_rels/.rels")? {
        let rels = xml_document(&text, "package relationships")?;
        if let Some(rel) = rels.root_element().children().find(|v| {
            v.has_tag_name("Relationship")
                && v.attribute("Type")
                    .is_some_and(|s| s.to_ascii_lowercase().ends_with("/3dmodel"))
        }) {
            ensure!(
                rel.attribute("TargetMode") != Some("External"),
                "The 3MF root model points outside the archive."
            );
            root_file = archive_path(
                rel.attribute("Target")
                    .context("Missing root model target")?,
                "",
            )?;
        }
    }
    if !archive.entries.contains_key(&root_file)
        && let Some(candidate) = archive
            .entries
            .keys()
            .find(|v| v.eq_ignore_ascii_case(&root_file))
    {
        root_file = candidate.clone();
    }
    let mut docs = HashMap::new();
    let mut pending = vec![root_file.clone()];
    while let Some(filename) = pending.pop() {
        if docs.contains_key(&filename) {
            continue;
        }
        let index = *archive
            .entries
            .get(&filename)
            .with_context(|| format!("3MF archive is missing {filename}"))?;
        let entry = archive.zip.by_index(index)?;
        let doc = read_document(
            BufReader::with_capacity(128 * 1024, entry),
            &filename,
            &mut appearance,
        )
        .with_context(|| filename.clone())?;
        for reference in &doc.references {
            if !docs.contains_key(reference) {
                pending.push(reference.clone());
            }
        }
        docs.insert(filename, doc);
        ensure!(
            docs.len() <= 10000,
            "3MF contains too many external model documents."
        );
    }
    let root = &docs[&root_file];
    ensure!(!root.build.is_empty(), "3MF has no build items.");
    let mut walker = Walker {
        docs: &docs,
        instances: Vec::new(),
        active: HashSet::new(),
    };
    for item in &root.build {
        if !item.printable {
            continue;
        }
        let context = project.objects.get(&item.id);
        let doc = docs.get(&item.file).context("Missing build document")?;
        let matrix = multiply(
            multiply(scale(root.unit), item.matrix),
            scale(doc.unit / root.unit),
        );
        let extruder = context
            .and_then(|v| v.extruder)
            .filter(|v| v.is_finite() && *v != 0.)
            .unwrap_or(if project.filaments.is_empty() { 0. } else { 1. });
        walker.walk(
            &item.file,
            item.id,
            TraversalState {
                matrix,
                context,
                extruder,
                part: None,
                depth: 0,
            },
        )?;
    }
    let mut bounds = Bounds::new();
    let mut original = 0u64;
    let mut expanded = 0u64;
    // Pointer identity is stable: documents are no longer mutated during traversal.
    let mut paint_counts: HashMap<(*const Object, usize), Option<u64>> = HashMap::new();
    for instance in &walker.instances {
        for index in 0..instance.object.triangles.len() {
            if range_info(instance, index).is_some_and(|v| !v.printable) {
                continue;
            }
            original = original
                .checked_add(1)
                .context("Model triangle count overflow")?;
            for p in triangle(instance, index) {
                bounds.include(p)?;
            }
            let count = if let Some(paint) = instance.object.paints.get(&index) {
                *paint_counts.entry((instance.object as *const Object, index)).or_insert_with(|| {
                    match painting(paint, None, |_, _| Ok(())) {
                        Ok(count) => Some(count),
                        Err(_) => { appearance.warn("Some advanced or malformed Bambu triangle painting could not be decoded faithfully; those triangles use only their known base material/filament color. Embedded plate previews retain the slicer appearance."); None }
                    }
                })
            } else {
                None
            };
            expanded = expanded
                .checked_add(count.unwrap_or(1))
                .context("Model triangle count overflow")?;
        }
    }
    let mut vertices = allocate_vertices(expanded)?;
    for instance in &walker.instances {
        for index in 0..instance.object.triangles.len() {
            let range = range_info(instance, index);
            if range.is_some_and(|v| !v.printable) {
                continue;
            }
            let slot = range
                .and_then(|v| v.extruder)
                .filter(|v| *v != 0.)
                .unwrap_or(instance.extruder);
            let base = filament(&project, slot, &mut appearance);
            let standard = standard_colors(instance.doc, instance.object, index, &mut appearance);
            let colors = std::array::from_fn(|v| {
                standard.and_then(|c| c[v]).or(base).unwrap_or(DEFAULT_RGB)
            });
            let points = triangle(instance, index);
            let paint = instance.object.paints.get(&index).filter(|_| {
                paint_counts
                    .get(&(instance.object as *const Object, index))
                    .is_some_and(Option::is_some)
            });
            if let Some(paint) = paint {
                painting(
                    paint,
                    Some([[1., 0., 0.], [0., 1., 0.], [0., 0., 1.]]),
                    |bary, state| {
                        let points = bary.map(|b| {
                            std::array::from_fn(|k| {
                                b[0] * points[0][k] + b[1] * points[1][k] + b[2] * points[2][k]
                            })
                        });
                        let colors = if state != 0 {
                            [filament(&project, state as f64, &mut appearance)
                                .unwrap_or(DEFAULT_RGB); 3]
                        } else {
                            bary.map(|b| {
                                std::array::from_fn(|k| {
                                    (b[0] * colors[0][k] as f64
                                        + b[1] * colors[1][k] as f64
                                        + b[2] * colors[2][k] as f64)
                                        as f32
                                })
                            })
                        };
                        emit(&mut vertices, points, colors)
                    },
                )?;
            } else {
                emit(&mut vertices, points, colors)?;
            }
        }
    }
    finish(vertices, original, bounds, appearance)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Cursor, Write},
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };
    use zip::{ZipWriter, write::SimpleFileOptions};

    static NEXT_FILE: AtomicU64 = AtomicU64::new(0);
    struct TestFile(PathBuf);
    impl TestFile {
        fn new(extension: &str) -> Self {
            Self(std::env::temp_dir().join(format!(
                "print-atlas-model-{}-{}.{}",
                std::process::id(),
                NEXT_FILE.fetch_add(1, Ordering::Relaxed),
                extension
            )))
        }
        fn bytes(extension: &str, bytes: &[u8]) -> Self {
            let file = Self::new(extension);
            File::create(&file.0).unwrap().write_all(bytes).unwrap();
            file
        }
    }
    impl Drop for TestFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    fn package(entries: &[(&str, &[u8])]) -> TestFile {
        let file = TestFile::new("3mf");
        let mut zip = ZipWriter::new(File::create(&file.0).unwrap());
        for (name, bytes) in entries {
            zip.start_file(
                *name,
                SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated),
            )
            .unwrap();
            zip.write_all(bytes).unwrap();
        }
        zip.finish().unwrap();
        file
    }
    fn mesh(id: u32, attrs: &str) -> String {
        format!(
            "<object id=\"{id}\"><mesh><vertices><vertex x=\"0\" y=\"0\" z=\"0\"/><vertex x=\"2\" y=\"0\" z=\"0\"/><vertex x=\"0\" y=\"2\" z=\"0\"/></vertices><triangles><triangle v1=\"0\" v2=\"1\" v3=\"2\" {attrs}/></triangles></mesh></object>"
        )
    }
    fn model_xml(resources: &str, build: &str) -> String {
        format!(
            "<model unit=\"millimeter\" xmlns:p=\"http://schemas.microsoft.com/3dmanufacturing/production/2015/06\"><resources>{resources}</resources><build>{build}</build></model>"
        )
    }
    fn ascii_facet(x: f32) -> String {
        format!(
            "facet normal 0 0 1\nouter loop\nvertex {x} 0 0\nvertex {x} 1 0\nvertex {x} 0 1\nendloop\nendfacet\n"
        )
    }
    fn binary_stl(triangles: &[(f32, u16)]) -> Vec<u8> {
        let mut bytes = vec![0; 84];
        bytes[80..84].copy_from_slice(&(triangles.len() as u32).to_le_bytes());
        for &(x, attr) in triangles {
            bytes.extend_from_slice(&[0; 12]);
            for p in [[x, 0., 0.], [x, 1., 0.], [x, 0., 1.]] {
                for coordinate in p {
                    bytes.extend_from_slice(&coordinate.to_le_bytes());
                }
            }
            bytes.extend_from_slice(&attr.to_le_bytes());
        }
        bytes
    }
    fn approx(a: f32, b: f32) {
        assert!((a - b).abs() < 0.0001, "{a} != {b}");
    }

    fn assert_stream_matches_full(path: &Path) {
        let full = load_model_full(path).unwrap();
        let mut index = 0;
        let info = visit_stl(path, |triangle| {
            for vertex in triangle {
                assert_eq!(vertex.position, full.vertices[index].position);
                assert_eq!(vertex.color, full.vertices[index].color);
                index += 1;
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(index, full.vertices.len());
        assert_eq!(info.triangle_count, full.triangle_count);
        assert_eq!(info.bounds_min, full.bounds_min);
        assert_eq!(info.bounds_max, full.bounds_max);
    }

    #[test]
    fn streaming_matches_full_ascii_binary_and_color_conventions() {
        let ascii = format!(
            "solid stream\n{}{}{}endsolid stream",
            ascii_facet(-8.),
            ascii_facet(35.),
            ascii_facet(0.125),
        );
        let plain = binary_stl(&[(-8., 0), (35., 0), (0.125, 0)]);
        let viscam = binary_stl(&[
            (-8., 0x8000 | (31 << 10)),
            (35., 0x8000 | (17 << 5) | 3),
            (0.125, 0),
        ]);
        let mut magics = binary_stl(&[(-8., 0x8000), (35., 31), (0.125, 17 << 10)]);
        magics[..10].copy_from_slice(b"COLOR=\x10\x80\xf0\xff");
        let mut solid_header = plain.clone();
        solid_header[..6].copy_from_slice(b"solid ");
        let mut trailing = viscam.clone();
        trailing.extend_from_slice(b"trailing data");
        for bytes in [
            ascii.as_bytes(),
            plain.as_slice(),
            viscam.as_slice(),
            magics.as_slice(),
            solid_header.as_slice(),
            trailing.as_slice(),
        ] {
            let file = TestFile::bytes("stl", bytes);
            assert_stream_matches_full(&file.0);
        }
    }

    #[test]
    fn streaming_retains_degenerate_facets_like_full_loader() {
        let ascii = b"solid point\nfacet normal 0 0 0\nvertex 0 0 0\nvertex 0 0 0\nvertex 0 0 0\nendfacet\nendsolid";
        let mut binary = binary_stl(&[(0., 0)]);
        binary[96..132].fill(0);
        for bytes in [ascii.as_slice(), binary.as_slice()] {
            let file = TestFile::bytes("stl", bytes);
            assert_stream_matches_full(&file.0);
            let info = visit_stl(&file.0, |triangle| {
                assert!(triangle.iter().all(|v| v.position == [0.; 3]));
                Ok(())
            })
            .unwrap();
            assert_eq!(info.triangle_count, 1);
            assert_eq!(info.bounds_min, [0.; 3]);
            assert_eq!(info.bounds_max, [0.; 3]);
        }
    }

    #[test]
    fn streaming_stops_immediately_and_preserves_callback_errors() {
        let ascii = format!(
            "solid callback\n{}{}endsolid callback",
            ascii_facet(1.),
            ascii_facet(2.),
        );
        let binary = binary_stl(&[(1., 0), (2., 0)]);
        for bytes in [ascii.as_bytes(), binary.as_slice()] {
            let file = TestFile::bytes("stl", bytes);
            let mut calls = 0;
            let error = visit_stl(&file.0, |_| {
                calls += 1;
                Err(std::io::Error::other("visitor stopped").into())
            })
            .unwrap_err();
            assert_eq!(calls, 1);
            assert_eq!(
                error.downcast_ref::<std::io::Error>().unwrap().to_string(),
                "visitor stopped"
            );
        }
    }

    #[test]
    fn streaming_rejects_invalid_facets_after_valid_geometry() {
        let ascii = format!(
            "solid invalid\n{}facet normal 0 0 1\nvertex 1e100 0 0\nvertex 0 1 0\nvertex 0 0 1\nendfacet\nendsolid",
            ascii_facet(0.),
        );
        let nonfinite = binary_stl(&[(0., 0), (f32::NAN, 0)]);
        let mut truncated = binary_stl(&[(0., 0), (1., 0)]);
        truncated.pop();
        for bytes in [ascii.as_bytes(), nonfinite.as_slice(), truncated.as_slice()] {
            let file = TestFile::bytes("stl", bytes);
            assert!(load_model_full(&file.0).is_err());
            let mut calls = 0;
            assert!(
                visit_stl(&file.0, |_| {
                    calls += 1;
                    Ok(())
                })
                .is_err()
            );
            assert!(calls <= 1);
        }
    }

    #[test]
    fn full_loader_preserves_tessellated_surface_in_stl_and_3mf() {
        const SIDE: u32 = 12;
        let mut expected = Vec::new();
        for y in 0..SIDE {
            for x in 0..SIDE {
                let (x, y) = (x as f32, y as f32);
                expected.extend_from_slice(&[
                    [x, y, 0.],
                    [x + 1., y, 0.],
                    [x + 1., y + 1., 0.],
                    [x, y, 0.],
                    [x + 1., y + 1., 0.],
                    [x, y + 1., 0.],
                ]);
            }
        }
        let count = expected.len() / 3;
        let mut ascii = String::from("solid surface\n");
        let mut binary = vec![0; 84];
        binary[80..84].copy_from_slice(&(count as u32).to_le_bytes());
        let mut xml_vertices = String::new();
        let mut xml_triangles = String::new();
        for (index, triangle) in expected.as_chunks::<3>().0.iter().enumerate() {
            ascii.push_str("facet normal 0 0 1\nouter loop\n");
            binary.extend_from_slice(&[0; 12]);
            for p in triangle {
                ascii.push_str(&format!("vertex {} {} {}\n", p[0], p[1], p[2]));
                xml_vertices.push_str(&format!(
                    "<vertex x=\"{}\" y=\"{}\" z=\"{}\"/>",
                    p[0], p[1], p[2]
                ));
                for coordinate in p {
                    binary.extend_from_slice(&coordinate.to_le_bytes());
                }
            }
            ascii.push_str("endloop\nendfacet\n");
            binary.extend_from_slice(&[0; 2]);
            let v = index * 3;
            xml_triangles.push_str(&format!(
                "<triangle v1=\"{v}\" v2=\"{}\" v3=\"{}\"/>",
                v + 1,
                v + 2
            ));
        }
        ascii.push_str("endsolid surface\n");
        let xml = model_xml(
            &format!(
                "<object id=\"1\"><mesh><vertices>{xml_vertices}</vertices><triangles>{xml_triangles}</triangles></mesh></object>"
            ),
            "<item objectid=\"1\"/>",
        );
        let files = [
            TestFile::bytes("stl", ascii.as_bytes()),
            TestFile::bytes("stl", &binary),
            package(&[("3D/3dmodel.model", xml.as_bytes())]),
        ];
        for file in files {
            let full = load_model_full(&file.0).unwrap();
            assert_eq!(full.triangle_count, count as u64);
            assert_eq!(
                full.vertices.iter().map(|v| v.position).collect::<Vec<_>>(),
                expected
            );
            if file.0.extension().is_some_and(|v| v == "stl") {
                assert_stream_matches_full(&file.0);
            }
        }
    }

    #[test]
    fn display_allocation_rejects_vertex_count_overflow() {
        assert!(allocate_vertices(u64::MAX).is_err());
    }

    #[test]
    fn ascii_preserves_all_source_bounds() {
        let text = format!(
            "solid sample\n{}{}{}endsolid sample",
            ascii_facet(-3.),
            ascii_facet(500.),
            ascii_facet(2.)
        );
        let file = TestFile::bytes("stl", text.as_bytes());
        let model = load_model_full(&file.0).unwrap();
        assert_eq!(model.triangle_count, 3);
        assert_eq!(model.vertices.len(), 9);
        assert_eq!(model.bounds_min, [-3., 0., 0.]);
        assert_eq!(model.bounds_max, [500., 1., 1.]);
    }
    #[test]
    fn ascii_rejects_broken_facets_and_nonfinite_positions() {
        for text in [
            "solid a\nfacet normal 0 0 1\nvertex 0 0 0\nendfacet\nendsolid",
            "solid a\nfacet normal 0 0 1\nfacet normal 0 0 1\n",
            "solid a\nvertex 0 0 0\n",
            "solid a\nfacet normal 0 0 1\nvertex NaN 0 0\nvertex 0 0 0\nvertex 0 1 0\nendfacet",
        ] {
            let file = TestFile::bytes("stl", text.as_bytes());
            assert!(load_model_full(&file.0).is_err());
            assert!(visit_stl(&file.0, |_| Ok(())).is_err());
        }
    }
    #[test]
    fn binary_bounds_and_palette_include_all_facets() {
        let file = TestFile::bytes(
            "stl",
            &binary_stl(&[(0., 0), (999., 0x8000 | (31 << 10)), (-5., 0)]),
        );
        let model = load_model_full(&file.0).unwrap();
        assert_eq!(model.triangle_count, 3);
        assert_eq!(model.vertices.len(), 9);
        assert_eq!(model.bounds_min, [-5., 0., 0.]);
        assert_eq!(model.bounds_max, [999., 1., 1.]);
        assert_eq!(model.palette, vec![[255, 0, 0]]);
        assert_eq!(model.vertices[0].color, DEFAULT_RGB);
    }
    #[test]
    fn binary_magics_color_order_and_object_color() {
        let mut bytes = binary_stl(&[(0., 0x8000), (1., 31)]);
        bytes[..10].copy_from_slice(b"COLOR=\x00\xff\x00\xff");
        let file = TestFile::bytes("stl", &bytes);
        let model = load_model_full(&file.0).unwrap();
        assert_eq!(model.vertices[0].color, [0., 1., 0.]);
        assert_eq!(model.vertices[3].color, [1., 0., 0.]);
    }
    #[test]
    fn binary_solid_header_still_uses_record_size() {
        let mut bytes = binary_stl(&[(0., 0)]);
        bytes[..6].copy_from_slice(b"solid ");
        let file = TestFile::bytes("stl", &bytes);
        assert_eq!(load_model_full(&file.0).unwrap().triangle_count, 1);
    }
    #[test]
    fn binary_truncation_nonfinite_and_trailing_data_are_explicit() {
        let mut truncated = binary_stl(&[(0., 0)]);
        truncated.pop();
        let file = TestFile::bytes("stl", &truncated);
        assert!(
            load_model_full(&file.0)
                .unwrap_err()
                .to_string()
                .contains("truncated")
        );
        let file = TestFile::bytes("stl", &binary_stl(&[(f32::INFINITY, 0)]));
        assert!(load_model_full(&file.0).is_err());
        let mut trailing = binary_stl(&[(0., 0)]);
        trailing.extend_from_slice(b"trailer");
        let file = TestFile::bytes("stl", &trailing);
        assert!(
            load_model_full(&file.0)
                .unwrap()
                .warnings
                .iter()
                .any(|v| v.contains("trailing data"))
        );
    }
    #[test]
    fn empty_models_are_rejected() {
        let file = TestFile::bytes("stl", &binary_stl(&[]));
        assert!(load_model_full(&file.0).is_err());
        assert!(visit_stl(&file.0, |_| Ok(())).is_err());
    }
    #[test]
    fn matrix_layout_rotation_translation_and_composition() {
        let rotation = transform(Some("0 1 0 -1 0 0 0 0 1 10 20 30")).unwrap();
        assert_eq!(point(rotation, [2., 3., 4.]), [7., 22., 34.]);
        assert_eq!(
            point(multiply(scale(10.), rotation), [2., 3., 4.]),
            [70., 220., 340.]
        );
        assert!(transform(Some("1 0 0")).is_err());
        assert!(transform(Some("1 0 0 0 1 0 0 0 1 NaN 0 0")).is_err());
    }
    #[test]
    fn archive_paths_decode_and_stay_inside_package() {
        assert_eq!(
            archive_path("../Objects/a%20b.model", "3D/Models/root.model").unwrap(),
            "3D/Objects/a b.model"
        );
        assert_eq!(
            archive_path("/3D/Objects/a.model", "3D/root.model").unwrap(),
            "3D/Objects/a.model"
        );
        for path in [
            "../../outside.model",
            "https://example.test/model",
            "C:\\model",
            "/../bad",
            "bad%ZZ",
            "%2e%2e/%2e%2e/bad",
            "file.model#id",
        ] {
            assert!(archive_path(path, "3D/root.model").is_err(), "{path}");
        }
    }
    #[test]
    fn standard_per_vertex_colors_and_transparency_warning() {
        let resources = format!(
            "<colorgroup id=\"8\"><color color=\"#ff0000\"/><color color=\"#00ff0080\"/><color color=\"#0000ff\"/></colorgroup>{}",
            mesh(1, "pid=\"8\" p1=\"0\" p2=\"1\" p3=\"2\"")
        );
        let xml = model_xml(&resources, "<item objectid=\"1\"/>");
        let file = package(&[("3D/3dmodel.model", xml.as_bytes())]);
        let model = load_model_full(&file.0).unwrap();
        assert_eq!(
            model.vertices.iter().map(|v| v.color).collect::<Vec<_>>(),
            vec![[1., 0., 0.], [0., 1., 0.], [0., 0., 1.]]
        );
        assert!(model.warnings.iter().any(|v| v.contains("opaque")));
    }
    #[test]
    fn standard_base_material_object_default_is_used() {
        let object = mesh(1, "").replace(
            "<object id=\"1\">",
            "<object id=\"1\" pid=\"7\" pindex=\"1\">",
        );
        let xml = model_xml(
            &format!(
                "<basematerials id=\"7\"><base name=\"a\" displaycolor=\"#ff0000\"/><base name=\"b\" displaycolor=\"#0000ff\"/></basematerials>{object}"
            ),
            "<item objectid=\"1\"/>",
        );
        let file = package(&[("3D/3dmodel.model", xml.as_bytes())]);
        let model = load_model_full(&file.0).unwrap();
        assert!(model.vertices.iter().all(|v| v.color == [0., 0., 1.]));
    }
    #[test]
    fn nested_external_models_combine_units_and_parent_translations() {
        let root = "<model unit=\"centimeter\" xmlns:p=\"urn:production\"><resources><object id=\"1\"><components><component objectid=\"2\" p:path=\"/3D/child.model\" transform=\"1 0 0 0 1 0 0 0 1 3 0 0\"/></components></object></resources><build><item objectid=\"1\" transform=\"1 0 0 0 1 0 0 0 1 2 0 0\"/></build></model>";
        let child = format!(
            "<model unit=\"inch\"><resources><object id=\"2\"><components><component objectid=\"3\" transform=\"1 0 0 0 1 0 0 0 1 1 0 0\"/></components></object>{}</resources></model>",
            mesh(3, "")
        );
        let file = package(&[
            ("3D/3dmodel.model", root.as_bytes()),
            ("3D/child.model", child.as_bytes()),
        ]);
        let model = load_model_full(&file.0).unwrap();
        approx(model.bounds_min[0], 75.4); // 20 + 30 + one inch.
        approx(model.bounds_max[0], 126.2);
        approx(model.bounds_max[1], 50.8);
        assert_eq!(model.triangle_count, 1);
    }
    #[test]
    fn root_relationship_and_external_build_apply_model_units() {
        let rels = "<Relationships><Relationship Type=\"http://schemas.microsoft.com/3dmanufacturing/2013/01/3dmodel\" Target=\"/Models/root.model\"/></Relationships>";
        let root = "<model unit=\"centimeter\" xmlns:p=\"urn:p\"><resources/><build><item objectid=\"3\" p:path=\"child.model\" transform=\"1 0 0 0 1 0 0 0 1 2 0 0\"/></build></model>";
        let child = format!(
            "<model unit=\"inch\"><resources>{}</resources></model>",
            mesh(3, "")
        );
        let file = package(&[
            ("_rels/.rels", rels.as_bytes()),
            ("Models/root.model", root.as_bytes()),
            ("Models/child.model", child.as_bytes()),
        ]);
        let model = load_model_full(&file.0).unwrap();
        approx(model.bounds_min[0], 20.);
        approx(model.bounds_max[0], 70.8);
    }
    #[test]
    fn nonprintable_parts_build_items_and_other_object_types_are_excluded() {
        let a = mesh(2, "");
        let b = mesh(3, "").replace("x=\"2\"", "x=\"2000\"");
        let c = mesh(4, "").replace("<object id=\"4\">", "<object id=\"4\" type=\"support\">");
        let resources = format!(
            "<object id=\"1\"><components><component objectid=\"2\"/><component objectid=\"3\"/><component objectid=\"4\"/></components></object>{a}{b}{c}"
        );
        let xml = model_xml(
            &resources,
            "<item objectid=\"1\"/><item objectid=\"3\" printable=\"false\"/>",
        );
        let config = "<config><object id=\"1\"><metadata key=\"extruder\" value=\"1\"/><part id=\"2\"><metadata key=\"extruder\" value=\"2\"/></part><part id=\"3\" subtype=\"negative_part\"/></object></config>";
        let settings = r##"{"filament_colour":["#ff0000","#00ff00"]}"##;
        let file = package(&[
            ("3D/3dmodel.model", xml.as_bytes()),
            ("Metadata/model_settings.config", config.as_bytes()),
            ("Metadata/project_settings.config", settings.as_bytes()),
        ]);
        let model = load_model_full(&file.0).unwrap();
        assert_eq!(model.triangle_count, 1);
        assert_eq!(model.bounds_max, [2., 2., 0.]);
        assert!(model.vertices.iter().all(|v| v.color == [0., 1., 0.]));
    }
    #[test]
    fn slic3r_volume_ranges_exclude_modifiers_and_assign_extruders() {
        let object = mesh(1, "").replace(
            "</triangles>",
            "<triangle v1=\"0\" v2=\"1\" v3=\"2\"/></triangles>",
        );
        let xml = model_xml(&object, "<item objectid=\"1\"/>");
        let config = "<config><object id=\"1\"><volume firstid=\"0\" lastid=\"0\"><metadata key=\"volume_type\" value=\"2\"/></volume><volume firstid=\"1\" lastid=\"1\"><metadata key=\"extruder\" value=\"2\"/></volume></object></config>";
        let settings = r##"{"filament_color":"#ff0000;#0000ff"}"##;
        let file = package(&[
            ("3D/3dmodel.model", xml.as_bytes()),
            ("Metadata/Slic3r_PE_model.config", config.as_bytes()),
            ("Metadata/project_settings.config", settings.as_bytes()),
        ]);
        let model = load_model_full(&file.0).unwrap();
        assert_eq!(model.triangle_count, 1);
        assert_eq!(model.vertices[0].color, [0., 0., 1.]);
    }
    #[test]
    fn painting_leaf_states_extensions_and_child_order_match_bambu() {
        let bary = [[1., 0., 0.], [0., 1., 0.], [0., 0., 1.]];
        let mut states = Vec::new();
        painting("c", None, |_, _| Ok(())).unwrap_err();
        painting("1c", Some(bary), |_, state| {
            states.push(state);
            Ok(())
        })
        .unwrap();
        assert_eq!(states, [4]);
        let mut children = Vec::new();
        assert_eq!(
            painting("481", Some(bary), |p, state| {
                children.push((p, state));
                Ok(())
            })
            .unwrap(),
            2
        );
        assert_eq!(children[0].1, 2);
        assert_eq!(children[1].1, 1);
        assert_eq!(children[0].0, [[0., 0.5, 0.5], [0., 0., 1.], [1., 0., 0.]]);
        assert_eq!(children[1].0, [[1., 0., 0.], [0., 1., 0.], [0., 0.5, 0.5]]);
        assert_eq!(painting("0002", None, |_, _| Ok(())).unwrap(), 3);
        assert_eq!(painting("00003", None, |_, _| Ok(())).unwrap(), 4);
        for malformed in ["", "xyz", "1", "00", "7", "b", "f", "c"] {
            assert!(
                painting(malformed, None, |_, _| Ok(())).is_err(),
                "{malformed}"
            );
        }
    }
    #[test]
    fn painting_subdivision_overrides_filaments_and_interpolates_unpainted_gradient() {
        let resources = format!(
            "<colorgroup id=\"8\"><color color=\"#ff0000\"/><color color=\"#00ff00\"/><color color=\"#0000ff\"/></colorgroup>{}",
            mesh(
                1,
                "pid=\"8\" p1=\"0\" p2=\"1\" p3=\"2\" paint_color=\"081\""
            )
        );
        let xml = model_xml(&resources, "<item objectid=\"1\"/>");
        let settings = r##"{"filament_colour":["#ffffff","#ffff00"]}"##;
        let file = package(&[
            ("3D/3dmodel.model", xml.as_bytes()),
            ("Metadata/project_settings.config", settings.as_bytes()),
        ]);
        let model = load_model_full(&file.0).unwrap();
        assert_eq!(model.triangle_count, 1);
        assert_eq!(model.vertices.len(), 6);
        assert!(model.vertices[..3].iter().all(|v| v.color == [1., 1., 0.]));
        assert_eq!(model.vertices[3].color, [1., 0., 0.]);
        assert_eq!(model.vertices[5].color, [0., 0.5, 0.5]);
        assert_eq!(model.bounds_max, [2., 2., 0.]);
        assert!(model.palette.contains(&[255, 255, 0]));
    }
    #[test]
    fn malformed_painting_warns_and_uses_only_known_base_filament() {
        let xml = model_xml(
            &mesh(1, "paint_color=\"bad-paint\""),
            "<item objectid=\"1\"/>",
        );
        let settings = r##"{"filament_colour":["#123456"]}"##;
        let file = package(&[
            ("3D/3dmodel.model", xml.as_bytes()),
            ("Metadata/project_settings.config", settings.as_bytes()),
        ]);
        let model = load_model_full(&file.0).unwrap();
        assert_eq!(model.vertices.len(), 3);
        assert!(
            model
                .warnings
                .iter()
                .any(|v| v.contains("could not be decoded faithfully"))
        );
        approx(model.vertices[0].color[0], 18. / 255.);
    }
    #[test]
    fn unknown_painted_slot_is_neutral_not_invented_color() {
        let xml = model_xml(&mesh(1, "paint_color=\"8\""), "<item objectid=\"1\"/>");
        let settings = r##"{"filament_colour":["#ff0000"]}"##;
        let file = package(&[
            ("3D/3dmodel.model", xml.as_bytes()),
            ("Metadata/project_settings.config", settings.as_bytes()),
        ]);
        let model = load_model_full(&file.0).unwrap();
        assert_eq!(model.vertices[0].color, DEFAULT_RGB);
        assert!(
            model
                .warnings
                .iter()
                .any(|v| v.contains("no usable project color"))
        );
    }
    #[test]
    fn unsupported_appearance_is_reported_without_inventing_color() {
        let resources = format!(
            "<texture2dgroup id=\"8\" texid=\"9\"/>{}",
            mesh(1, "pid=\"8\" p1=\"0\" mmu_segmentation=\"0\"")
        );
        let xml = model_xml(&resources, "<item objectid=\"1\"/>");
        let file = package(&[("3D/3dmodel.model", xml.as_bytes())]);
        let model = load_model_full(&file.0).unwrap();
        assert_eq!(model.vertices[0].color, DEFAULT_RGB);
        assert!(model.warnings.iter().any(|v| v.contains("texture2dgroup")));
        assert!(
            model
                .warnings
                .iter()
                .any(|v| v.contains("advanced painting"))
        );
    }
    #[test]
    fn invalid_xml_indices_duplicate_attributes_and_dtd_are_rejected() {
        let invalid_vertex = model_xml(
            &mesh(1, "").replace("v3=\"2\"", "v3=\"3\""),
            "<item objectid=\"1\"/>",
        );
        let duplicate_attr = model_xml(
            &mesh(1, "").replace("x=\"2\"", "x=\"2\" x=\"3\""),
            "<item objectid=\"1\"/>",
        );
        let namespace_duplicate = model_xml(
            &mesh(1, "").replace("x=\"2\"", "x=\"2\" p:x=\"3\""),
            "<item objectid=\"1\"/>",
        );
        for xml in [
            invalid_vertex.as_str(),
            duplicate_attr.as_str(),
            namespace_duplicate.as_str(),
            "<!DOCTYPE model><model/>",
            "<model><resources></model>",
            "<model/><model/>",
        ] {
            assert!(
                read_document(
                    Cursor::new(xml.as_bytes()),
                    "test.model",
                    &mut Appearance::default()
                )
                .is_err()
            );
        }
    }
    #[test]
    fn cycles_missing_objects_and_empty_build_fail_clearly() {
        for xml in [
            model_xml(
                "<object id=\"1\"><components><component objectid=\"1\"/></components></object>",
                "<item objectid=\"1\"/>",
            ),
            model_xml("", "<item objectid=\"1\"/>"),
            model_xml(&mesh(1, ""), ""),
        ] {
            let file = package(&[("3D/3dmodel.model", xml.as_bytes())]);
            assert!(load_model_full(&file.0).is_err());
        }
    }
    #[test]
    fn embedded_plates_numeric_order_and_standard_preview_fallback() {
        let png = b"\x89PNG\r\n\x1a\npayload";
        let jpg = b"\xff\xd8\xffpayload";
        let file = package(&[
            ("Metadata/plate_10.png", png),
            ("Metadata/plate_2.png", png),
            ("Metadata/thumbnail.png", png),
        ]);
        let images = embedded_plates(&file.0).unwrap();
        assert_eq!(
            images.iter().map(|v| v.0.as_str()).collect::<Vec<_>>(),
            ["plate_2.png", "plate_10.png"]
        );
        let file = package(&[("Metadata/preview.jpg", jpg)]);
        assert_eq!(
            embedded_plates(&file.0).unwrap()[0],
            ("preview.jpg".to_owned(), jpg.to_vec())
        );
        let file = package(&[("Metadata/plate_1.png", b"not an image")]);
        assert!(embedded_plates(&file.0).unwrap().is_empty());
    }
}

#[cfg(test)]
mod streaming_boundary_tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn ascii_keywords_and_numbers_cross_reader_chunks_and_last_line_has_no_newline() {
        let text = b"solid chunked\r\nfacet normal 0 0 1\r\nouter loop\r\nvertex -1.25e2 0 0\r\nvertex 0 3.5 0\r\nvertex 0 0 9\r\nendloop\r\nendfacet\r\nendsolid chunked";
        let mut bounds = Bounds::new();
        let count = read_ascii(
            BufReader::with_capacity(7, Cursor::new(text)),
            |triangle, _| {
                for point in triangle {
                    bounds.include(point)?;
                }
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(count, 1);
        assert_eq!(bounds.min, [-125., 0., 0.]);
        assert_eq!(bounds.max, [0., 3.5, 9.]);
    }

    #[test]
    fn xml_tags_attributes_and_entities_cross_reader_chunks() {
        let text = b"<?xml version=\"1.0\"?><model unit=\"millimeter\"><resources><object id=\"1\"><mesh><vertices><vertex x=\"&#49;\" y=\"0\" z=\"0\"/><vertex x=\"2\" y=\"0\" z=\"0\"/><vertex x=\"0\" y=\"2\" z=\"0\"/></vertices><triangles><triangle v1=\"0\" v2=\"1\" v3=\"2\"/></triangles></mesh></object></resources><build><item objectid=\"1\"/></build></model>";
        let doc = read_document(
            BufReader::with_capacity(5, Cursor::new(text)),
            "chunked.model",
            &mut Appearance::default(),
        )
        .unwrap();
        assert_eq!(doc.objects[&1].vertices[0], [1., 0., 0.]);
        assert_eq!(doc.objects[&1].triangles, [[0, 1, 2]]);
        assert_eq!(doc.build.len(), 1);
    }

    #[test]
    fn invalid_coordinates_are_detected_in_every_source_facet() {
        let mut bounds = Bounds::new();
        assert!(bounds.include([f32::MAX as f64 * 2., 0., 0.]).is_err());
        let text = b"solid bad\nfacet normal 0 0 1\nvertex 0 0 0\nvertex 1 0 0\nvertex 0 1 0\nendfacet\nfacet normal 0 0 1\nvertex 1e100 0 0\nvertex 1 0 0\nvertex 0 1 0\nendfacet\nendsolid";
        let result = read_ascii(Cursor::new(text), |triangle, _| {
            for point in triangle {
                bounds.include(point)?;
            }
            Ok(())
        });
        assert!(result.is_err());
    }

    #[test]
    fn recursive_painting_has_no_retained_tree_and_preserves_leaf_order() {
        // Root's highest child is itself split: visit its states 2 and 0,
        // followed by the root's unsplit lower child in state 1.
        let mut states = Vec::new();
        let count = painting(
            "40811",
            Some([[1., 0., 0.], [0., 1., 0.], [0., 0., 1.]]),
            |_, state| {
                states.push(state);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(count, 3);
        assert_eq!(states, [2, 0, 1]);
        let excessively_deep = format!("{}{}", "0".repeat(258), "3".repeat(129));
        assert!(painting(&excessively_deep, None, |_, _| Ok(())).is_err());
    }
}
