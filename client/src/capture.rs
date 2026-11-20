//! Screen capture and cropping of the selection. Linux asks the screenshot
//! portal, or `grim` on wlroots compositors whose portal cannot; Windows
//! and macOS capture every monitor with xcap and put the images together.
//! Either way the result covers the bounding box of all monitors in
//! physical pixels.

use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    #[cfg(target_os = "linux")]
    #[error("screenshot portal: {0}")]
    Portal(#[from] ashpd::Error),
    #[cfg(any(not(target_os = "linux"), test))]
    #[error("screen capture: {0}")]
    Screen(String),
    #[cfg(target_os = "linux")]
    #[error("grim failed: {0}")]
    Grim(String),
    #[cfg(target_os = "linux")]
    #[error("the portal returned a URI that is no local file: {0}")]
    NotAFile(String),
    #[error("reading the screenshot failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("the screenshot is no PNG image: {0}")]
    Decode(#[from] png::DecodingError),
    #[error("writing the PNG failed: {0}")]
    Encode(#[from] png::EncodingError),
    #[error("unsupported PNG format {0:?}")]
    Format(png::ColorType),
    #[error("the selection lies outside the image")]
    Selection,
    #[error("the edited image is {0}x{1} px, expected {2}x{3} px")]
    Size(u32, u32, u32, u32),
}

#[cfg(target_os = "linux")]
impl CaptureError {
    /// The portal refused or failed, as opposed to reading the result.
    pub fn is_portal(&self) -> bool {
        matches!(self, Self::Portal(_))
    }

    /// The user closed a portal dialog.
    pub fn is_cancelled(&self) -> bool {
        matches!(
            self,
            Self::Portal(ashpd::Error::Response(
                ashpd::desktop::ResponseError::Cancelled
            ))
        )
    }
}

/// A captured screen: the PNG as the portal wrote it.
pub struct Capture {
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Captures all screens. Returns the capture and a note for the log.
/// Asks the portal first; on wlroots compositors whose portal cannot take
/// screenshots (Hyprland, for one), `grim` fills in.
#[cfg(target_os = "linux")]
pub async fn take() -> Result<(Capture, String), CaptureError> {
    let err = match portal().await {
        Ok(result) => return Ok(result),
        Err(err) => err,
    };
    if err.is_cancelled() || !crate::session::Desktop::current().may_use_grim() {
        return Err(err);
    }
    match grim().await {
        Ok(result) => Ok(result),
        Err(grim_err) => {
            crate::report::note(format!("grim after the portal failed: {grim_err}"));
            Err(err)
        }
    }
}

#[cfg(target_os = "linux")]
async fn portal() -> Result<(Capture, String), CaptureError> {
    use ashpd::desktop::screenshot::Screenshot;
    // The portal of GNOME shows no dialog with interactive=false.
    let response = Screenshot::request()
        .interactive(false)
        .modal(true)
        .send()
        .await?
        .response()?;
    let uri = response.uri().as_str();
    let path = file_path(uri).ok_or_else(|| CaptureError::NotAFile(uri.to_owned()))?;
    let png = tokio::fs::read(&path).await?;
    // The portal of GNOME stores every screenshot in the pictures folder;
    // the client keeps only what the user selects.
    let _ = tokio::fs::remove_file(&path).await;
    let (width, height) = dimensions(&png)?;
    let note = format!("portal, file {} (removed)", path.display());
    Ok((Capture { png, width, height }, note))
}

/// All outputs through `grim`, which writes the PNG to stdout. It covers
/// the bounding box of the layout at the highest output scale, like the
/// portal image.
#[cfg(target_os = "linux")]
async fn grim() -> Result<(Capture, String), CaptureError> {
    let output = tokio::process::Command::new("grim")
        .args(["-t", "png", "-"])
        .kill_on_drop(true)
        .output()
        .await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(CaptureError::Grim(format!(
            "{}: {}",
            output.status,
            stderr.trim()
        )));
    }
    let png = output.stdout;
    let (width, height) = dimensions(&png)?;
    Ok((Capture { png, width, height }, "grim".to_owned()))
}

/// Captures all screens. Returns the capture and a note for the log.
#[cfg(any(windows, target_os = "macos"))]
pub async fn take() -> Result<(Capture, String), CaptureError> {
    tokio::task::spawn_blocking(capture_monitors)
        .await
        .map_err(|e| CaptureError::Screen(e.to_string()))?
}

#[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
pub async fn take() -> Result<(Capture, String), CaptureError> {
    Err(CaptureError::Screen(
        "not supported on this system yet".into(),
    ))
}

#[cfg(windows)]
fn capture_monitors() -> Result<(Capture, String), CaptureError> {
    let screen = |e: xcap::XCapError| CaptureError::Screen(e.to_string());
    let mut shots = Vec::new();
    for monitor in xcap::Monitor::all().map_err(screen)? {
        let (x, y) = (monitor.x().map_err(screen)?, monitor.y().map_err(screen)?);
        let image = monitor.capture_image().map_err(screen)?;
        shots.push((x, y, image));
    }
    let parts: Vec<Part<'_>> = shots
        .iter()
        .map(|(x, y, image)| Part {
            x: *x,
            y: *y,
            width: image.width(),
            height: image.height(),
            rgba: image.as_raw(),
        })
        .collect();
    let note = format!("xcap, {} monitors", parts.len());
    Ok((compose(&parts)?, note))
}

/// macOS places monitors in points and captures each one at its own scale
/// factor, e.g. 2 for a Retina display next to an external one at 1. The
/// images are brought to the largest factor, so they share one pixel grid;
/// the overlays map their monitors the same way (see `source_rect`).
#[cfg(target_os = "macos")]
fn capture_monitors() -> Result<(Capture, String), CaptureError> {
    let screen = |e: xcap::XCapError| CaptureError::Screen(e.to_string());
    let mut shots = Vec::new();
    for monitor in xcap::Monitor::all().map_err(screen)? {
        let (x, y) = (monitor.x().map_err(screen)?, monitor.y().map_err(screen)?);
        let scale = f64::from(monitor.scale_factor().map_err(screen)?);
        let image = monitor.capture_image().map_err(screen)?;
        shots.push((x, y, scale, image));
    }
    let target = shots.iter().map(|s| s.2).fold(1.0, f64::max);
    let scaled: Vec<_> = shots
        .iter()
        .map(|(x, y, scale, image)| {
            let (rgba, width, height) = upscale(
                image.as_raw(),
                image.width(),
                image.height(),
                target / scale,
            );
            (
                to_pixels(*x, target),
                to_pixels(*y, target),
                rgba,
                width,
                height,
            )
        })
        .collect();
    let parts: Vec<Part<'_>> = scaled
        .iter()
        .map(|(x, y, rgba, width, height)| Part {
            x: *x,
            y: *y,
            width: *width,
            height: *height,
            rgba,
        })
        .collect();
    let note = format!("xcap, {} monitors, scale {target}", parts.len());
    Ok((compose(&parts)?, note))
}

/// A position in points as pixels at `scale`.
#[cfg(any(target_os = "macos", test))]
fn to_pixels(points: i32, scale: f64) -> i32 {
    (f64::from(points) * scale).round() as i32
}

/// Enlarges 8-bit RGBA rows by `factor` (at least 1, nearest neighbour).
/// Returns the image unchanged when there is nothing to enlarge.
#[cfg(any(target_os = "macos", test))]
fn upscale(
    rgba: &[u8],
    width: u32,
    height: u32,
    factor: f64,
) -> (std::borrow::Cow<'_, [u8]>, u32, u32) {
    let size = |v: u32| (f64::from(v) * factor).round() as u32;
    let (new_width, new_height) = (size(width), size(height));
    if factor <= 1.0 || (new_width, new_height) == (width, height) {
        return (std::borrow::Cow::Borrowed(rgba), width, height);
    }
    let mut out = Vec::with_capacity(new_width as usize * new_height as usize * 4);
    for y in 0..new_height {
        let src_y = ((f64::from(y) / factor) as u32).min(height - 1) as usize;
        for x in 0..new_width {
            let src_x = ((f64::from(x) / factor) as u32).min(width - 1) as usize;
            let start = (src_y * width as usize + src_x) * 4;
            out.extend_from_slice(&rgba[start..start + 4]);
        }
    }
    (std::borrow::Cow::Owned(out), new_width, new_height)
}

/// One monitor image at its position on the virtual desktop.
#[cfg(any(windows, target_os = "macos", test))]
pub struct Part<'a> {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    /// 8-bit RGBA rows without padding.
    pub rgba: &'a [u8],
}

/// Puts monitor images together into one PNG covering their bounding box.
/// Gaps between monitors stay black.
#[cfg(any(windows, target_os = "macos", test))]
pub fn compose(parts: &[Part<'_>]) -> Result<Capture, CaptureError> {
    let no_image = || CaptureError::Screen("no monitor image".into());
    let left = parts
        .iter()
        .map(|p| i64::from(p.x))
        .min()
        .ok_or_else(no_image)?;
    let top = parts
        .iter()
        .map(|p| i64::from(p.y))
        .min()
        .ok_or_else(no_image)?;
    let right = parts
        .iter()
        .map(|p| i64::from(p.x) + i64::from(p.width))
        .max()
        .ok_or_else(no_image)?;
    let bottom = parts
        .iter()
        .map(|p| i64::from(p.y) + i64::from(p.height))
        .max()
        .ok_or_else(no_image)?;
    let too_large = || CaptureError::Screen("the screens are too large".into());
    let width = u32::try_from(right - left).map_err(|_| too_large())?;
    let height = u32::try_from(bottom - top).map_err(|_| too_large())?;
    let stride = width as usize * 4;
    let mut pixels = vec![0u8; stride.checked_mul(height as usize).ok_or_else(too_large)?];
    // Opaque black, so gaps between monitors are not transparent.
    for alpha in pixels.iter_mut().skip(3).step_by(4) {
        *alpha = 255;
    }
    for part in parts {
        let row_len = part.width as usize * 4;
        if part.rgba.len() < row_len * part.height as usize {
            return Err(CaptureError::Screen("a monitor image is too short".into()));
        }
        // Both offsets are within the bounding box by construction.
        let x = (i64::from(part.x) - left) as usize;
        let y = (i64::from(part.y) - top) as usize;
        for row in 0..part.height as usize {
            let src = &part.rgba[row * row_len..(row + 1) * row_len];
            let start = (y + row) * stride + x * 4;
            pixels[start..start + row_len].copy_from_slice(src);
        }
    }
    let mut png = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut png, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        // Speed over size: the capture is only kept until the selection.
        encoder.set_compression(png::Compression::Fast);
        let mut writer = encoder.write_header()?;
        writer.write_image_data(&pixels)?;
    }
    Ok(Capture { png, width, height })
}

/// The local path of a `file://` URI, with percent escapes decoded.
#[cfg(target_os = "linux")]
fn file_path(uri: &str) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    // Only local files: `file:///path` or `file://localhost/path`.
    let rest = uri.strip_prefix("file://")?;
    let path = rest.strip_prefix("localhost").unwrap_or(rest);
    if !path.starts_with('/') {
        return None;
    }
    let mut bytes = Vec::with_capacity(path.len());
    let mut iter = path.bytes();
    while let Some(b) = iter.next() {
        if b == b'%' {
            let hex = [iter.next()?, iter.next()?];
            let value = u8::from_str_radix(std::str::from_utf8(&hex).ok()?, 16).ok()?;
            bytes.push(value);
        } else {
            bytes.push(b);
        }
    }
    if bytes.contains(&0) {
        return None;
    }
    Some(PathBuf::from(std::ffi::OsString::from_vec(bytes)))
}

#[cfg(any(target_os = "linux", test))]
fn dimensions(data: &[u8]) -> Result<(u32, u32), CaptureError> {
    let reader = png::Decoder::new(std::io::Cursor::new(data)).read_info()?;
    let info = reader.info();
    Ok((info.width, info.height))
}

/// A rectangle in image pixels.
#[derive(Debug, Clone, Copy, serde::Deserialize, serde::Serialize)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// Crops `rect` out of the PNG and returns it as 8-bit RGB(A) PNG.
pub fn crop(data: &[u8], rect: Rect) -> Result<Vec<u8>, CaptureError> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(data));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info()?;
    let mut buffer = vec![0; reader.output_buffer_size().ok_or(CaptureError::Selection)?];
    let frame = reader.next_frame(&mut buffer)?;
    let channels: usize = match frame.color_type {
        png::ColorType::Rgb => 3,
        png::ColorType::Rgba => 4,
        other => return Err(CaptureError::Format(other)),
    };
    let fits = rect.width > 0
        && rect.height > 0
        && rect
            .x
            .checked_add(rect.width)
            .is_some_and(|r| r <= frame.width)
        && rect
            .y
            .checked_add(rect.height)
            .is_some_and(|b| b <= frame.height);
    if !fits {
        return Err(CaptureError::Selection);
    }
    let (x, y, w, h) = (
        rect.x as usize,
        rect.y as usize,
        rect.width as usize,
        rect.height as usize,
    );
    let mut pixels = Vec::with_capacity(w * h * channels);
    for row in y..y + h {
        let start = row * frame.line_size + x * channels;
        pixels.extend_from_slice(&buffer[start..start + w * channels]);
    }
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, rect.width, rect.height);
        encoder.set_color(frame.color_type);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header()?;
        writer.write_image_data(&pixels)?;
    }
    Ok(out)
}

/// Checks that the editor returned a PNG of the size it was given. The
/// editor runs in the webview, so its output is decoded before use.
pub fn check_edit(data: &[u8], width: u32, height: u32) -> Result<(), CaptureError> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(data));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info()?;
    let (w, h) = (reader.info().width, reader.info().height);
    if (w, h) != (width, height) {
        return Err(CaptureError::Size(w, h, width, height));
    }
    let mut buffer = vec![0; reader.output_buffer_size().ok_or(CaptureError::Selection)?];
    reader.next_frame(&mut buffer)?;
    Ok(())
}

/// Saves a finished image as `scrn-<unix millis>.png` in [`output_dir`].
pub fn save(png: &[u8]) -> Result<PathBuf, CaptureError> {
    let dir = output_dir();
    std::fs::create_dir_all(&dir)?;
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    let path = dir.join(format!("scrn-{millis}.png"));
    std::fs::write(&path, png)?;
    Ok(path)
}

pub fn output_dir() -> PathBuf {
    crate::paths::pictures_dir()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(width: u32, height: u32) -> Vec<u8> {
        let mut out = Vec::new();
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        let mut data = Vec::new();
        for y in 0..height {
            for x in 0..width {
                data.extend_from_slice(&[x as u8, y as u8, 7]);
            }
        }
        writer.write_image_data(&data).unwrap();
        drop(writer);
        out
    }

    #[test]
    fn monitors_are_put_together_on_the_virtual_desktop() {
        // A 2x1 monitor left of the primary one, and a 1x2 primary monitor
        // at (0, 0): the bounding box is 3x2 with a black gap below the
        // left monitor.
        let left = [10, 0, 0, 255, 20, 0, 0, 255];
        let primary = [0, 30, 0, 255, 0, 40, 0, 255];
        let parts = [
            Part {
                x: -2,
                y: 0,
                width: 2,
                height: 1,
                rgba: &left,
            },
            Part {
                x: 0,
                y: 0,
                width: 1,
                height: 2,
                rgba: &primary,
            },
        ];
        let capture = compose(&parts).unwrap();
        assert_eq!((capture.width, capture.height), (3, 2));
        let mut decoder = png::Decoder::new(std::io::Cursor::new(&capture.png[..]));
        decoder.set_transformations(png::Transformations::normalize_to_color8());
        let mut reader = decoder.read_info().unwrap();
        let mut buffer = vec![0; reader.output_buffer_size().unwrap()];
        reader.next_frame(&mut buffer).unwrap();
        let px = |x: usize, y: usize| &buffer[(y * 3 + x) * 4..(y * 3 + x) * 4 + 4];
        assert_eq!(px(0, 0), [10, 0, 0, 255]);
        assert_eq!(px(1, 0), [20, 0, 0, 255]);
        assert_eq!(px(2, 0), [0, 30, 0, 255]);
        assert_eq!(px(2, 1), [0, 40, 0, 255]);
        assert_eq!(px(0, 1), [0, 0, 0, 255]);
    }

    #[test]
    fn mixed_scale_monitors_do_not_overlap() {
        // A Retina display (2x2 points, scale 2) left of an external one
        // (1x1 point at x = 2, scale 1). In pixels at scale 2 the external
        // image starts at x = 4 and is enlarged to 2x2.
        let retina = [[1u8, 0, 0, 255]; 16].concat();
        let external = [0u8, 2, 0, 255];
        let (big, width, height) = upscale(&external, 1, 1, 2.0);
        assert_eq!((width, height), (2, 2));
        let parts = [
            Part {
                x: to_pixels(0, 2.0),
                y: 0,
                width: 4,
                height: 4,
                rgba: &retina,
            },
            Part {
                x: to_pixels(2, 2.0),
                y: 0,
                width,
                height,
                rgba: &big,
            },
        ];
        let capture = compose(&parts).unwrap();
        assert_eq!((capture.width, capture.height), (6, 4));
        let mut decoder = png::Decoder::new(std::io::Cursor::new(&capture.png[..]));
        decoder.set_transformations(png::Transformations::normalize_to_color8());
        let mut reader = decoder.read_info().unwrap();
        let mut buffer = vec![0; reader.output_buffer_size().unwrap()];
        reader.next_frame(&mut buffer).unwrap();
        let px = |x: usize, y: usize| &buffer[(y * 6 + x) * 4..(y * 6 + x) * 4 + 4];
        assert_eq!(px(3, 0), [1, 0, 0, 255]);
        assert_eq!(px(4, 0), [0, 2, 0, 255]);
        assert_eq!(px(5, 1), [0, 2, 0, 255]);
        assert_eq!(px(4, 2), [0, 0, 0, 255]);
    }

    #[test]
    fn upscale_keeps_images_at_the_target_scale() {
        let rgba = [7u8, 8, 9, 255];
        let (same, width, height) = upscale(&rgba, 1, 1, 1.0);
        assert!(matches!(same, std::borrow::Cow::Borrowed(_)));
        assert_eq!((width, height), (1, 1));
    }

    #[test]
    fn compose_rejects_missing_or_short_images() {
        assert!(compose(&[]).is_err());
        let short = [0u8; 4];
        let parts = [Part {
            x: 0,
            y: 0,
            width: 2,
            height: 1,
            rgba: &short,
        }];
        assert!(compose(&parts).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn file_uris_become_paths() {
        assert_eq!(
            file_path("file:///home/a/Bilder/Bildschirmfoto%20vom%202026-11-06%20%C3%A4.png"),
            Some(PathBuf::from(
                "/home/a/Bilder/Bildschirmfoto vom 2026-11-06 ä.png"
            ))
        );
        assert_eq!(
            file_path("file://localhost/tmp/x.png"),
            Some(PathBuf::from("/tmp/x.png"))
        );
        for bad in [
            "https://x/y.png",
            "file://host/x.png",
            "file:///a%2",
            "file:///a%zz",
            "file:///a%00b",
        ] {
            assert_eq!(file_path(bad), None, "{bad}");
        }
    }

    #[test]
    fn crops_the_selected_pixels() {
        let source = image(10, 8);
        let rect = Rect {
            x: 2,
            y: 3,
            width: 4,
            height: 2,
        };
        let out = crop(&source, rect).unwrap();
        let mut reader = png::Decoder::new(std::io::Cursor::new(&out[..]))
            .read_info()
            .unwrap();
        let mut buffer = vec![0; reader.output_buffer_size().unwrap()];
        let frame = reader.next_frame(&mut buffer).unwrap();
        assert_eq!((frame.width, frame.height), (4, 2));
        assert_eq!(&buffer[..3], &[2, 3, 7]);
        let last = &buffer[frame.line_size + 3 * 3..frame.line_size + 4 * 3];
        assert_eq!(last, &[5, 4, 7]);
        assert_eq!(dimensions(&out).unwrap(), (4, 2));
    }

    #[test]
    fn edits_must_keep_the_size() {
        let edit = image(6, 4);
        assert!(check_edit(&edit, 6, 4).is_ok());
        assert!(matches!(
            check_edit(&edit, 6, 5),
            Err(CaptureError::Size(6, 4, 6, 5))
        ));
        assert!(matches!(
            check_edit(b"not a png", 6, 4),
            Err(CaptureError::Decode(_))
        ));
        // A truncated PNG has a valid header but no complete image data.
        assert!(check_edit(&edit[..edit.len() - 20], 6, 4).is_err());
    }

    #[test]
    fn selections_outside_the_image_are_rejected() {
        let source = image(10, 8);
        for rect in [
            Rect {
                x: 8,
                y: 0,
                width: 3,
                height: 1,
            },
            Rect {
                x: 0,
                y: 0,
                width: 0,
                height: 1,
            },
            Rect {
                x: u32::MAX,
                y: 0,
                width: 2,
                height: 1,
            },
        ] {
            assert!(matches!(crop(&source, rect), Err(CaptureError::Selection)));
        }
    }
}
