# Print Atlas

A native **Rust** Windows application for organizing a 3D-printing library: STL and Bambu Studio 3MF models, images, PDFs, text documents, and related project files. The interface is built with egui/eframe. There is no Electron, Node.js, JavaScript frontend, WebView, account, or runtime network service.

## Install or run

Windows x64, Windows 10 version 1809 or newer, with a working OpenGL graphics driver.

- **Installer:** run `Print-Atlas-Setup.exe`. Installs for the current user without administrator access. Optional Start menu and desktop shortcuts are offered.
- **Portable:** extract the complete `Print-Atlas-2.0.3-windows-x64.zip`, then run `Print Atlas.exe`. Keep the accompanying `runtime` directory and third-party notices beside the executable.
- **This source checkout:** after packaging, run `release/Print Atlas 2.0.3.exe`. Manual shortcuts should target this versioned launcher.

The executable and installer are unsigned. Windows SmartScreen may show an unknown-publisher warning. Downloads include `SHA256SUMS.txt`; verify an artifact with `Get-FileHash -Algorithm SHA256`.

No Rust compiler, Node.js installation, browser runtime, or separate PDF application is required for normal use. PDFium is bundled with its complete upstream license notices; the application executable uses the static MSVC CRT.

## Browse and inspect

The left panel is a **file-and-folder tree**, not just a folder list. Expand a folder using its `>`/`v` control. Clicking a folder navigates its contents; clicking a file opens its preview and details **inline in the main workspace**. Expansion, collapse, file selection, folder navigation, and background scans keep the tree's existing scroll position. Changing the library root starts a new tree.

- Choose **All files**, **Favorites**, **Recently modified**, or a folder.
- Search file names, relative paths, and tags. Every search word must match. **Ctrl+K** returns to the browser and focuses search.
- Filter **3D models**, **Images**, **Documents** (including PDFs), **Other files**, STL, or 3MF.
- Choose **Small**, **Grid**, **Large**, or **List**. Layout, sorting, and subfolder preferences are remembered.
- Use **Include subfolders** in folder views to browse recursively.
- Click a card or a sidebar file to inspect it. Use **Back to files** to return to the same collection.
- Refresh with **F5** or the sidebar **Refresh** button. The library also reconciles in the background about every 20 seconds.

The tree and grid mount only visible rows. Thumbnail work runs on bounded background queues, with foreground inspections prioritized. Model thumbnails are cached on disk; a bounded in-memory texture cache is used for browsing. Cache keys include file path, size, modification time, and renderer version.

### Model previews

- ASCII and binary STL, including common binary color conventions.
- Bambu 3MF saved plate images, including multiple plates and original saved colors. Choose a plate from its dropdown.
- **Interactive 3D** uses a native Rust, depth-buffered renderer. Drag to orbit, scroll to zoom around the cursor, right-drag or Shift-drag to pan, and **Reset view** to restore the camera. Pan and zoom rerender geometry in the actual viewport rather than moving a cropped image; zoom supports 0.02×–1000×.
- Nested/external in-archive 3MF components, unit conversion, standard materials and vertex colors, object/part filament assignments, recursive Bambu triangle painting, and modifier exclusion.

Interactive inspection and generated mesh thumbnails retain all printable triangles, including painted 3MF subdivisions; no geometry is discarded for a triangle budget. STL thumbnails use two streaming passes into fixed-size image/depth buffers instead of retaining the complete mesh. Interactive geometry and the 3MF thumbnail fallback still require memory for the complete expanded mesh, so very large files may take longer or report an allocation error. Dimensions include all original triangles. Source models are never simplified or rewritten.

Textures, transparency, composite materials, and physical multi-color filament appearances are not fully reproduced in interactive 3D. Unsupported appearance is reported rather than invented. Saved Bambu plate renders remain the reference for exact slicer appearance. A sliced-only project can show saved plates without providing an interactive mesh.

### Blender saved thumbnails

`.blend` files appear in the sidebar, grids, and **3D models** filter. Print Atlas reads the embedded saved thumbnail directly for grid and inline image previews, including historical and Blender 5.x files, raw or gzip/zstd-compressed. It never launches Blender or evaluates scenes/scripts to generate previews. Originals remain untouched.

This is only the image saved inside the file, often 128×128 pixels: it may be old, low resolution, or absent. Zoom/pan magnifies that image; there is no interactive Blender mesh or higher-resolution rendering. Files without an embedded thumbnail remain browsable and show **No embedded Blender thumbnail**.

Extraction stops after finding the image rather than loading the scene. Source reads and decompressed scanning are bounded to 128 MiB, zstd history to 64 MiB, and saved images to 4096 pixels per side / 64 MiB RGBA. A large file with an early thumbnail can still preview without reading its full contents. Malformed and unsupported headers report errors.


### Images, PDFs, and text

- PNG, JPEG, WebP, GIF, BMP, ICO, TIFF, AVIF, and SVG are decoded locally. GIF and AVIF previews use the first image/frame. SVG external resources are blocked. Images fit the available area; use **+**, **−**, **Fit**, and drag-to-pan.
- PDFs render inside the main workspace. Use previous/next, the page-number control, and zoom/fit. **Page text / select & copy** switches to selectable extracted text for the current page. Password-protected documents report a clear error and can be opened externally. PDF scripts, actions, links, forms, and attachments are not activated.
- TXT, Markdown, CSV/TSV, JSON, XML, YAML, configuration files, logs, G-code, common source files, and extensionless README/LICENSE files have read-only previews. UTF-8 and BOM-marked UTF-16LE/BE are supported. HTML and Markdown are shown literally, not executed or interpreted as web pages. Search does not scan document contents.
- Office files, videos, archives, and other regular files are listed and can open in their Windows-associated application; those formats do not have inline viewers.

Images/PDFs have a 64 MiB source limit. Raster image decoding is bounded at 32 megapixels / 256 MiB, and rendered output is dimension-limited. PDF pages and model canvases are bounded rather than rasterizing every page or holding unbounded display images. Text previews read the first 256 KiB, with a truncation notice. Oversized, corrupt, or unsupported files report errors; originals remain available for external opening.

## Organize safely

Star a card or check **Favorite** in its details. Add comma-separated tags and notes, then **Save tags & notes**. Pending edits are also saved before navigating away or closing the app; navigation is stopped if saving fails.

**Rename**, **Move**, and the sidebar **+** operate on actual files/folders. Ctrl-click cards to select multiple files, then **Move selected**. Existing destinations are never overwritten. There is intentionally no delete action. Use **Show in Explorer** or **Open in associated application** as needed.

Metadata follows files renamed or moved through Print Atlas. External renames/moves do not automatically migrate annotations. Batch move errors attempt to roll back completed transfers; concurrent external changes can prevent a full rollback. Keep regular backups of your library.

The scanner skips application/build/cache directories, `.git`, `node_modules`, `target`, `__MACOSX`, symbolic links, and Windows junctions/reparse directories. Only files inside the chosen library can be renamed/moved through the organizer.

## Existing users and local data

The Rust app continues using **`%APPDATA%/Print Atlas/library.json`**. Existing root selection, favorites, tags, notes, and preferences are migrated automatically. Windows casing, slash styles, and verbatim path prefixes are normalized without discarding unknown catalog fields or annotations for missing files. A malformed catalog is reported rather than silently overwritten.

Annotations are not embedded in STL, 3MF, image, or document files. Close the app before backing up `library.json`. Native model thumbnail PNGs are under `native-thumbnails`; they can be deleted while the app is closed to regenerate them. The disk cache is not automatically size-limited. Uninstalling removes installed program files, not this user-data directory or your model library.

## Build from source

Prerequisites:

1. Windows x64 and Rust **1.98 or newer** with the MSVC toolchain.
2. Visual Studio Build Tools with **Desktop development with C++** and a Windows SDK.
3. NASM for the native AVIF decoder. `scripts/package.ps1 -ProvisionToolsOnly` provisions pinned, checksum-verified NASM and Inno Setup into your per-user build-tools directory. Add the NASM directory it prints to your terminal's `PATH` for direct Cargo commands.
4. Bundled PDFium, obtained through the checksum-verified script below.

```powershell
powershell -ExecutionPolicy Bypass -File scripts/package.ps1 -ProvisionToolsOnly
powershell -ExecutionPolicy Bypass -File scripts/fetch-pdfium.ps1
cargo build --locked
cargo run --locked
```

Verification:

```powershell
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo run --locked -- --inspect "C:\path\to\model.3mf"
```

`--inspect FILE` prints diagnostic JSON for a real model/image/text/PDF preview without opening a window. The tests use isolated temporary libraries and cover metadata migration, collisions, rollback, links/junctions, geometry/color/painting/transform boundaries, image formats, PDF rendering/text, and raster occlusion/orbit behavior.

Build distributables:

```powershell
powershell -ExecutionPolicy Bypass -File scripts/package.ps1
```

This provisions verified build tools/runtime, performs a locked optimized Windows x64 build with static CRT, generates complete Rust dependency notices, and creates:

```text
release/Print Atlas 2.0.3.exe
release/runtime/
release/third-party-licenses/
release/Print-Atlas-Setup.exe
release/Print-Atlas-2.0.3-windows-x64.zip
release/SHA256SUMS.txt
```

Keep `Cargo.lock` committed. Generated binaries, caches, downloaded runtime DLLs, build tools, and user data do not belong in Git.

## GitHub repository and releases

The repository is [scottwdavis/Print-Atlas](https://github.com/scottwdavis/Print-Atlas), with `main` as the primary branch. The SSH remote is `git@github.com:scottwdavis/Print-Atlas.git`.

- `.github/workflows/ci.yml`: Windows formatting, strict Clippy, and locked tests on pushes and pull requests.
- `.github/workflows/release.yml`: pushing a version tag such as **`v2.0.3`** builds the installer, portable ZIP, and checksums and publishes them as GitHub release assets. The tag must match `Cargo.toml`'s version.
- Release publishing receives `contents: write` only in its release job. No repository owner/name or credentials are hardcoded.

Local verification does not claim a hosted GitHub Actions run. The hosted workflow executes after you push.

Project source licensing is provided in [LICENSE](LICENSE) (GNU General Public License, version 3). Third-party dependencies retain their own licenses: dependency attribution, license texts, source links, font notices, and PDFium notices are included in both distribution formats. The package is marked `publish = false` only to prevent accidental crates.io publication and to exclude project-owned source from third-party notice generation; GitHub source and binary releases are unaffected.

## Source layout

- `src/app.rs`: native tree/grid, inline workspace, bounded preview workers, annotations, and file actions.
- `src/library.rs`: scanning, Electron catalog migration, atomic persistence, and safe file operations.
- `src/model.rs`: streaming STL/3MF parsing, Bambu colors/painting, and embedded plate extraction.
- `src/preview.rs`: image/text/PDF decoding and bounds; `src/render.rs`: native 3D rasterization.
- `src/blend.rs`: bounded, read-only extraction of embedded Blender thumbnails; no scene evaluation. Format references: [Blender block headers](https://github.com/blender/blender/blob/main/scripts/modules/_blendfile_header.py) and [thumbnail layout](https://github.com/blender/blender/blob/main/source/blender/blenkernel/BKE_main.hh).
- `src/main.rs`, `src/types.rs`: startup/diagnostic CLI and shared data types.
- `scripts/`, `installer/`, `.github/workflows/`: verified runtime/build-tool acquisition, dependency notices, packaging, installation, and CI/releases.

## 2.0.3

Fixed perforated grid thumbnails by removing the remaining triangle-sampling path. STL thumbnails stream every facet into the rasterizer without retaining a full mesh; 3MF geometry fallback uses full detail. The thumbnail cache identity changed, so old sparse images regenerate automatically when browsed after restarting the updated app. No manual cache deletion is needed.

## 2.0.2

Added embedded `.blend` thumbnails to grids, the sidebar, and inline previews, with clear missing-thumbnail messages. Supports raw, gzip, and zstd containers without running Blender.

## 2.0.1

Fixed incomplete interactive surfaces by retaining complete meshes. Camera pan and cursor-anchored zoom now happen before rasterization at the viewport's aspect ratio, eliminating the cropped-image barrier and the previous 5× zoom limit. Camera and resize work coalesces to the latest requested view.

## 2.0.0

Native Rust replacement of the Electron application. Stable virtualized file-and-folder sidebar, single-click inline inspection instead of preview popups, preserved annotations, and installable Windows/GitHub release packaging. The old web implementation and its runtime dependencies have been removed.
