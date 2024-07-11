// Copyright 2024 Monotype Imaging Inc.
use std::{
    ffi::OsStr,
    io::{Read, Seek},
    sync::Arc,
};

use cosmic_text::{
    fontdb::{Database, ID},
    Attrs, BorrowedWithFontSystem, Buffer, CacheKeyFlags, Color, Font, FontSystem, Metrics,
    SwashCache,
};
use image::{ImageOutputFormat, Pixel};
use tiny_skia::Pixmap;

/// The result type for the font thumbnail creation
type Result<T> = std::result::Result<T, FontThumbnailError>;

/// Default locale for the font system
const DEFAULT_LOCALE: &str = "en-US";
/// Starting point size for the font
const STARTING_POINT_SIZE: f32 = 512.0;
/// Step size for the point size
const POINT_SIZE_STEP: f32 = 8.0;
/// Minimum point size for a font
const MINIMUM_POINT_SIZE: f32 = 72.0;
/// Maximum width for the PNG
const MAXIMUM_WIDTH: u32 = 1024;

/// The name ID for the full name of the font from the name table
const FULL_NAME_ID: u16 = 4;
/// The name ID for the sample text of the font from the name table
const SAMPLE_TEXT_ID: u16 = 19;
/// The MIME type for the thumbnail
const THUMBNAIL_IMG_MIME_TYPE: &str = "image/png";
/// The MIME type for an SVG thumbnail
const THUMBNAIL_SVG_MIME_TYPE: &str = "image/svg+xml";
/// The text color for the thumbnail
const TEXT_COLOR: Color = Color::rgba(0, 0, 0, 0xff);
/// The background color for the thumbnail
const BACKGROUND_COLOR: tiny_skia::Color = tiny_skia::Color::WHITE;
/// The line height factor for the thumbnail
const LINE_HEIGHT_FACTOR: f32 = 1.075;
/// How much padding to use on the left and right sides of the text
const TOTAL_WIDTH_PADDING: f32 = 0.1;
/// The default SVG precision
const DEFAULT_SVG_PRECISION: u32 = 5;
/// The fill color for the SVG thumbnail
const SVG_GLYPH_FILL_COLOR: &str = "black";

/// Errors that can occur when creating a font thumbnail
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
enum FontThumbnailError {
    /// Error from the image crate
    #[error(transparent)]
    ImageError(#[from] image::ImageError),
    /// error from IO operations
    #[error(transparent)]
    IoError(#[from] std::io::Error),
    /// A font was not found
    #[error("No font found")]
    NoFontFound,
    /// No full name found in the font
    #[error("No full name found")]
    NoFullNameFound,
    /// Error when the buffer size is invalid
    #[error("The buffer size is invalid")]
    InvalidBufferSize,
    #[cfg(feature = "add_svg_font_thumbnails")]
    /// Could not create a Rect from the given values
    #[error("Invalid values for Rect")]
    InvalidRect,
    /// Failed to find a point size that would accommodate the width and text
    #[error("Failed to find an appropriate font point size to fit the width")]
    FailedToFindAppropriateSize,
    /// Failed to create a Pixmap object
    #[error("Failed to create pixmap")]
    FailedToCreatePixmap,
    /// Failed to get a pixel from the image
    #[error("Failed to get pixel from image; x: {x}, y: {y}")]
    FailedToGetPixel { x: u32, y: u32 },
    #[cfg(not(feature = "add_svg_font_thumbnails"))]
    /// The SVG feature is not enabled
    #[error("The SVG feature is not enabled")]
    SvgFeatureNotEnabled,
}

impl From<FontThumbnailError> for crate::Error {
    fn from(e: FontThumbnailError) -> Self {
        crate::Error::OtherError(e.to_string().into())
    }
}

/// Size of the bounding box
#[derive(Debug, Default, Clone)]
struct Size {
    /// Width of the bounding box
    w: f32,
    /// Height of the bounding box
    h: f32,
}

/// Information about the font
struct FontNameInfo {
    /// Full name of the font
    full_name: Option<String>,
    /// Sample text for the font
    #[allow(unused)]
    sample_text: Option<String>,
}

/// Information about a loaded font
struct LoadedFont<'a> {
    /// ID of the font
    id: ID,
    /// Attributes of the font
    attrs: Attrs<'a>,
}

/// The MIME type map for the image format of the thumbnail
const SUPPORTED_MIME_TYPES: &[&str] = &[
    "application/font-sfnt",
    "application/x-font-ttf",
    "application/x-font-opentype",
    "application/x-font-truetype",
    "font/otf",
    "font/sfnt",
    "font/ttf",
    "otf",
    "sfnt",
    "ttf",
];

/// Checks if the file is a supported font type
pub fn is_supported_font_file<T: AsRef<OsStr>>(ext: T) -> bool {
    matches!(
        ext.as_ref().to_ascii_lowercase().to_str(),
        Some("otf") | Some("ttf")
    )
}

/// Checks of the mime type is a valid supported font mime type
pub fn is_font_mime_type(mime: &str) -> bool {
    SUPPORTED_MIME_TYPES.iter().any(|m| m == &mime)
}

/// Finds the point size that fits the width and creates a buffer with the text and has it
/// ready for rendering.
///
/// # Remarks
/// The `line_height_fn` is a function that takes the font size and should be used to
/// calculate the line height.
fn get_buffer_with_pt_size_fits_width<T: Fn(f32) -> f32>(
    text: &str,
    attrs: Attrs,
    font_system: &mut FontSystem,
    font_size: f32,
    width: f32,
    minimum_point_size: f32,
    line_height_fn: T,
) -> Result<Buffer> {
    // Starting point size
    let mut font_size: f32 = font_size;
    // Generate the line height from the font height
    let mut line_height: f32 = line_height_fn(font_size);

    // Make sure there is a enough room for line wrapping to account for the
    // width being too small
    let height = line_height * 2.5;

    // Create a buffer for measuring the text
    let mut buffer = Buffer::new(font_system, Metrics::new(font_size, line_height));
    let mut borrowed_buffer = buffer.borrow_with(font_system);

    // Loop until we find the right size to fit within the maximum width
    while font_size > minimum_point_size {
        borrowed_buffer.set_size(width, height);
        borrowed_buffer.set_wrap(cosmic_text::Wrap::Glyph);
        borrowed_buffer.set_text(text, attrs, cosmic_text::Shaping::Advanced);
        borrowed_buffer.shape_until_scroll(true);
        // Get the number of layout runs, we expect one if it fits on one line
        let count = borrowed_buffer.layout_runs().count();
        // If it is one, we have found the right size
        if count == 1 {
            let size = measure_text(text, attrs, &mut borrowed_buffer)?;
            // There instances where the measured width was 0, but maybe this is
            // caught now by counting the number of layout runs?
            if size.w > 0.0 && size.w <= width {
                borrowed_buffer.set_size(size.w, size.h);
                return Ok(buffer);
            }
        }
        // Adjust and prepare to try again
        font_size -= POINT_SIZE_STEP;
        line_height = line_height_fn(font_size);

        // Update the buffer with the new font size
        borrowed_buffer.set_metrics(Metrics::new(font_size, line_height));
    }
    // At this point we have reached our minimum size, so setup to use it
    // which will result in text clipping, but that is fine
    font_size = minimum_point_size;
    line_height = line_height_fn(font_size);
    borrowed_buffer.set_size(width, line_height);
    borrowed_buffer.set_metrics(Metrics::new(font_size, line_height));
    borrowed_buffer.shape_until_scroll(true);
    // get the text replacing the last 3 characters with ellipsis
    let text = if text.len() > 3 {
        format!("{}...", text.split_at(text.len() - 3).0)
    } else {
        text.to_string()
    };
    borrowed_buffer.set_text(&text, attrs, cosmic_text::Shaping::Advanced);
    let size = measure_text(&text, attrs, &mut borrowed_buffer)?;
    // We still run the chance of an invalid size returned, so take that into
    // account
    if size.w > 0.0 && size.w <= width && size.h <= height {
        borrowed_buffer.set_size(size.w, size.h);
        return Ok(buffer);
    }
    Err(FontThumbnailError::FailedToFindAppropriateSize)
}

/// Load font data into the font database, returning the ID of the last font
fn load_font_data<'a>(font_db: &mut Database, data: Vec<u8>) -> Result<LoadedFont<'a>> {
    font_db.load_font_data(data);
    // Find the last font face loaded
    let face = font_db
        .faces()
        .last()
        .ok_or(FontThumbnailError::NoFontFound)?;
    let weight = face.weight;
    let style = face.style;
    let stretch = face.stretch;
    let attrs: Attrs = Attrs {
        color_opt: None,
        family: cosmic_text::Family::Serif,
        stretch,
        style,
        weight,
        metadata: 0,
        cache_key_flags: CacheKeyFlags::empty(),
    };
    Ok(LoadedFont { id: face.id, attrs })
}

/// Measure the text to get the size of the bounding box required
///
/// # Remarks
/// The width may come back as `0.0` if the text is empty or the buffer width is
/// too small.
fn measure_text(
    text: &str,
    attrs: Attrs,
    buffer: &mut BorrowedWithFontSystem<Buffer>,
) -> Result<Size> {
    buffer.set_text(text, attrs, cosmic_text::Shaping::Advanced);
    buffer.shape_until_scroll(true);
    measure_text_in_buffer(buffer)
}

/// Measure the text in the buffer to get the size of the bounding box required
///
/// # Remarks
/// The width may come back as `0.0` if the text is empty or the buffer width is
/// too small.
fn measure_text_in_buffer(buffer: &mut BorrowedWithFontSystem<Buffer>) -> Result<Size> {
    // Error if the buffer is not a valid/sane looking size
    if buffer.size().0 < 0. || buffer.size().1 < 0. {
        return Err(FontThumbnailError::InvalidBufferSize);
    }
    // Find the maximum width of the layout lines and keep track of the total number
    // of lines
    let (width, total_lines) = buffer
        .layout_runs()
        .fold((0.0, 0usize), |(width, total_lines), run| {
            (run.line_w.max(width), total_lines + 1)
        });
    Ok(Size {
        w: width,
        h: total_lines as f32 * buffer.metrics().line_height,
    })
}

impl From<Arc<Font>> for FontNameInfo {
    fn from(font: Arc<Font>) -> Self {
        let face = font.rustybuzz();
        let mut full_name = None;
        let mut sample_text = None;
        for name in face.names() {
            match name.name_id {
                FULL_NAME_ID => full_name = name.to_string(),
                SAMPLE_TEXT_ID => sample_text = name.to_string(),
                _ => {}
            }
        }
        FontNameInfo {
            full_name,
            sample_text,
        }
    }
}

/// Get a Skia paint for the color, which is a solid color shader, and not using
/// anti-aliasing
fn get_skia_paint_for_color<'a>(color: Color) -> tiny_skia::Paint<'a> {
    tiny_skia::Paint {
        shader: tiny_skia::Shader::SolidColor({
            let (r, g, b, a) = color.as_rgba_tuple();
            tiny_skia::Color::from_rgba8(r, g, b, a)
        }),
        blend_mode: tiny_skia::BlendMode::Color,
        anti_alias: true,
        ..Default::default()
    }
}

/// Determines the maximum viewable bounding box from a the tiny_skia rect object
#[cfg(feature = "add_svg_font_thumbnails")]
trait MaxBoundingBox: Sized {
    /// Finds the maximum bounding box from the given rect
    fn max(&self, other: Self) -> core::result::Result<Self, crate::Error>;
}

#[cfg(feature = "add_svg_font_thumbnails")]
impl MaxBoundingBox for tiny_skia::Rect {
    fn max(&self, other: Self) -> core::result::Result<Self, crate::Error> {
        // If the width and height are 0, then we can just return the other rect
        if self.width() == 0.0 && self.height() == 0.0 {
            Ok(other)
        } else if other.width() == 0.0 && other.height() == 0.0 {
            Ok(*self)
        } else {
            let left = self.left().min(other.left());
            let top = self.top().min(other.top());
            let right = self.right().max(other.right());
            let bottom = self.bottom().max(other.bottom());
            tiny_skia::Rect::from_ltrb(left, top, right, bottom)
                .ok_or(FontThumbnailError::InvalidRect.into())
        }
    }
}

#[cfg(feature = "add_svg_font_thumbnails")]
trait PrecisionRound {
    fn round_to(&self, precision: u32) -> Self;
}

#[cfg(feature = "add_svg_font_thumbnails")]
impl PrecisionRound for f32 {
    fn round_to(&self, precision: u32) -> Self {
        let factor = 10u32.pow(precision) as f32;
        (self * factor).round() / factor
    }
}

#[cfg(feature = "add_svg_font_thumbnails")]
impl PrecisionRound for (f32, f32) {
    fn round_to(&self, precision: u32) -> Self {
        (self.0.round_to(precision), self.1.round_to(precision))
    }
}

/// Generates a PNG thumbnail from a font file
/// # Returns
/// Returns Result `(format, image_bits)` if successful, otherwise `Error`
#[cfg(feature = "file_io")]
pub fn make_thumbnail(
    path: &std::path::Path,
    use_svg: Option<bool>,
) -> std::result::Result<(String, Vec<u8>), crate::error::Error> {
    let mut font_data = std::fs::read(path)?;
    let mut font_data = std::io::Cursor::new(&mut font_data);
    make_thumbnail_from_stream(&mut font_data, use_svg)
}

/// Make a PNG thumbnail from a stream, which should be font data bits.
/// # Returns
/// Returns Result `(format, image_bits)` if successful, otherwise `Error`
pub fn make_thumbnail_from_stream<R: Read + Seek + ?Sized>(
    stream: &mut R,
    use_svg: Option<bool>,
) -> std::result::Result<(String, Vec<u8>), crate::error::Error> {
    let font_data = std::io::Read::bytes(stream).collect::<std::io::Result<Vec<u8>>>()?;
    // Create a local font database, which only contains the font we loaded
    let mut font_db = Database::new();
    // Load the given font file into the font database, getting the ID of the
    // font to use with the font system
    let LoadedFont { id: font_id, attrs } = load_font_data(&mut font_db, font_data)?;

    // And build a font system from this local database
    let mut font_system = cosmic_text::FontSystem::new_with_locale_and_db(
        DEFAULT_LOCALE.to_string(),
        font_db.clone(),
    );
    // Get reference to the font from the font system
    let f = font_system
        .get_font(font_id)
        .ok_or(FontThumbnailError::NoFontFound)?;
    // Grab the potential italic angle of the font to calculate the width
    // of the slant later
    let angle = f.rustybuzz().italic_angle();
    let font_info = FontNameInfo::from(f.clone());
    let full_name = font_info
        .full_name
        .ok_or(FontThumbnailError::NoFullNameFound)?;

    // Create a swash cache for the font system, to cache rendering
    let mut swash_cache = SwashCache::new();

    let ascender = f.rustybuzz().ascender() as i32;
    let descender = f.rustybuzz().descender() as i32;
    let max_height: f32 = (ascender - descender) as f32 / f.rustybuzz().units_per_em() as f32;

    // Find a buffer that fits the width
    let mut buffer = get_buffer_with_pt_size_fits_width(
        &full_name,
        attrs,
        &mut font_system,
        STARTING_POINT_SIZE,
        MAXIMUM_WIDTH as f32 * (1.0 - TOTAL_WIDTH_PADDING),
        MINIMUM_POINT_SIZE,
        |x| (max_height * LINE_HEIGHT_FACTOR * x).ceil(),
    )?;

    if use_svg.unwrap_or(false) {
        let svg_buffer = make_svg(&mut buffer, &mut font_system, &mut swash_cache)?;
        Ok((THUMBNAIL_SVG_MIME_TYPE.to_string(), svg_buffer))
    } else {
        let png_buffer = make_png(&mut buffer, &mut font_system, &mut swash_cache, angle)?;
        Ok((THUMBNAIL_IMG_MIME_TYPE.to_string(), png_buffer))
    }
}

/// Make a PNG thumbnail from the buffer
pub fn make_png(
    text_buffer: &mut Buffer,
    font_system: &mut FontSystem,
    swash_cache: &mut SwashCache,
    angle: Option<f32>,
) -> std::result::Result<Vec<u8>, crate::error::Error> {
    // Got some reason, the `swash` library used by `cosmic-text` puts pixels at negative
    // x values, so we need to find the amount we need to offset the image by
    // to include all pixels.
    let layout_run = text_buffer
        .layout_runs()
        .next()
        .ok_or(FontThumbnailError::InvalidBufferSize)?;
    // Grab the cache key for the first glyph, so we can get the image information from
    // the swash cache
    let x = layout_run
        .glyphs
        .first()
        .ok_or(FontThumbnailError::InvalidBufferSize)?
        .physical((0., 0.), 1.0)
        .cache_key;
    let image = swash_cache
        .get_image(font_system, x)
        .as_ref()
        .ok_or(FontThumbnailError::FailedToCreatePixmap)?;
    // Only offset if the left is negative
    let offset = if image.placement.left < 0 {
        image.placement.left.abs()
    } else {
        0
    };

    // Borrow the buffer with the font system, to make things easier to make
    // calls
    let mut buffer = text_buffer.borrow_with(font_system);

    // Grab the actual width and height of the buffer for the image
    let (width, height) = buffer.size();

    // Calculate the width taken up by the italic angle
    let width_italic_buffer = match angle {
        // If we have an angle get the tangent of the angle
        Some(angle) => height * ((std::f32::consts::PI / 180.0) * angle).tan(),
        // Otherwise, use 0
        _ => 0.0,
    }
    .abs();

    // The total width will be our specified width + the width from the italic angle.
    // QUESTION: Should not the `cosmic-text` library really already include this in the
    //           width calculation? Are we doing something wrong?
    let width = width + width_italic_buffer + offset as f32;

    // Create a new pixel map for the main text
    let mut img =
        Pixmap::new(width as u32, height as u32).ok_or(FontThumbnailError::FailedToCreatePixmap)?;
    // Draw the text into the pixel map
    buffer.draw(swash_cache, TEXT_COLOR, |x, y, w, h, color| {
        if let Some(rect) =
            tiny_skia::Rect::from_xywh((x + offset) as f32, y as f32, w as f32, h as f32)
                .map(Some)
                .unwrap_or_default()
        {
            img.fill_rect(
                rect,
                &get_skia_paint_for_color(color),
                tiny_skia::Transform::identity(),
                None,
            );
        } else {
            println!(
                "WARN: Failed to create rect: x: {}, y: {}, w: {}, h: {}",
                x, y, w, h
            );
        }
    });

    // Create a pixel map for the total image
    let mut final_img =
        Pixmap::new(width as u32, height as u32).ok_or(FontThumbnailError::FailedToCreatePixmap)?;
    // Fill the image in with black to start
    final_img.fill(BACKGROUND_COLOR);
    // Draw the main text into the final image
    final_img.draw_pixmap(
        0,
        0,
        img.as_ref(),
        &tiny_skia::PixmapPaint::default(),
        tiny_skia::Transform::identity(),
        None,
    );
    // Now use the `image` crate to save the final image as a PNG as grayscale,
    // because as of now, `tiny-skia` does not support saving as PNG with
    // grayscale
    let mut total_img = image::GrayImage::new(width as u32, height as u32);
    for x in 0..total_img.width() {
        for y in 0..total_img.height() {
            if let Some(pixel) = final_img.pixel(x, y) {
                let rgb = [pixel.red(), pixel.green(), pixel.blue(), pixel.alpha()];
                let rgb: image::Rgba<u8> = *image::Rgba::from_slice(&rgb);
                total_img.put_pixel(x, y, rgb.to_luma());
            } else {
                return Err(FontThumbnailError::FailedToGetPixel { x, y }.into());
            }
        }
    }
    let mut png_buffer = Vec::new();
    let mut png_cursor = std::io::Cursor::new(&mut png_buffer);
    total_img.write_to(&mut png_cursor, ImageOutputFormat::Png)?;
    Ok(png_buffer)
}

#[cfg(not(feature = "add_svg_font_thumbnails"))]
pub fn make_svg(
    _text_buffer: &mut Buffer,
    _font_system: &mut FontSystem,
    _swash_cache: &mut SwashCache,
) -> std::result::Result<Vec<u8>, crate::error::Error> {
    Err(FontThumbnailError::SvgFeatureNotEnabled.into())
}
/// Make an SVG thumbnail from the buffer
#[cfg(feature = "add_svg_font_thumbnails")]
pub fn make_svg(
    text_buffer: &mut Buffer,
    font_system: &mut FontSystem,
    swash_cache: &mut SwashCache,
) -> std::result::Result<Vec<u8>, crate::error::Error> {
    use svg::Node;
    let mut svg_doc = svg::Document::new();
    // Start with a zero width/height box
    let mut bounding_box: tiny_skia::Rect =
        tiny_skia::Rect::from_xywh(0.0, 0.0, 0.0, 0.0).ok_or(FontThumbnailError::InvalidRect)?;
    for layout_run in text_buffer.layout_runs() {
        let mut group = svg::node::element::Group::new();
        for glyph in layout_run.glyphs {
            let mut data = svg::node::element::path::Data::new();
            // Get the x/y offsets
            let (x_offset, y_offset) = (glyph.x + glyph.x_offset, glyph.y + glyph.y_offset);
            // We will need the physical glyph to get the outline commands
            let physical_glyph = glyph.physical((0., 0.), 1.0);
            let cache_key = physical_glyph.cache_key;
            let outline_commands = swash_cache.get_outline_commands(font_system, cache_key);
            // Go through each command and build the path
            if let Some(commands) = outline_commands {
                for command in commands {
                    match command {
                        cosmic_text::Command::MoveTo(p1) => {
                            let rounded_data = (p1.x, p1.y).round_to(DEFAULT_SVG_PRECISION);
                            data = data.move_to(rounded_data);
                        }
                        cosmic_text::Command::LineTo(p1) => {
                            let rounded_data = (p1.x, p1.y).round_to(DEFAULT_SVG_PRECISION);
                            data = data.line_to(rounded_data);
                        }
                        cosmic_text::Command::CurveTo(p1, p2, p3) => {
                            let p1_rounded_data = (p1.x, p1.y).round_to(DEFAULT_SVG_PRECISION);
                            let p2_rounded_data = (p2.x, p2.y).round_to(DEFAULT_SVG_PRECISION);
                            let p3_rounded_data = (p3.x, p3.y).round_to(DEFAULT_SVG_PRECISION);
                            data = data.cubic_curve_to((
                                p1_rounded_data,
                                p2_rounded_data,
                                p3_rounded_data,
                            ));
                        }
                        cosmic_text::Command::QuadTo(p1, p2) => {
                            let p1_rounded_data = (p1.x, p1.y).round_to(DEFAULT_SVG_PRECISION);
                            let p2_rounded_data = (p2.x, p2.y).round_to(DEFAULT_SVG_PRECISION);
                            data = data.quadratic_curve_to((p1_rounded_data, p2_rounded_data));
                        }
                        cosmic_text::Command::Close => {
                            data = data.close();
                        }
                    }
                }
            }
            // Don't add empty data paths
            if !data.is_empty() {
                let path = svg::node::element::Path::new()
                    .set("fill", SVG_GLYPH_FILL_COLOR)
                    .set(
                        "transform",
                        format!("translate({}, {})", x_offset, y_offset),
                    )
                    .set("d", data.clone());
                group = group.add(path);
            }
        }
        // We will need to create a temporary document to get the bounding box
        // of the entire group
        let tmp_doc = svg::Document::new().add(group.clone());
        let tree =
            resvg::usvg::Tree::from_str(&tmp_doc.to_string(), &resvg::usvg::Options::default())
                .map_err(|_e| FontThumbnailError::FailedToCreatePixmap)?;
        bounding_box = bounding_box.max(tree.root().abs_bounding_box())?;

        // Setup the y translate
        let y_translate = if bounding_box.y() < 0.0 {
            // We need the height of the bounding box, minus the absolute value of the y
            // origin. The reason for the 2nd part is that the y origin is negative, so
            // so we need the data to be positive
            bounding_box.height() - (2.0 * bounding_box.y().abs())
        } else {
            bounding_box.height()
        };
        group.assign(
            "transform",
            format!("translate(0, {}) scale(1, -1)", y_translate),
        );
        svg_doc.append(group);
    }
    svg_doc = svg_doc.set(
        "viewBox",
        (
            bounding_box.x().floor(),
            bounding_box.y().floor(),
            bounding_box.width().ceil(),
            bounding_box.height().ceil(),
        ),
    );

    let mut svg_buffer = Vec::new();
    let svg_cursor = std::io::Cursor::new(&mut svg_buffer);
    svg::write(svg_cursor, &svg_doc)?;
    Ok(svg_buffer)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn test_is_supported_font_file() {
        assert!(is_supported_font_file("ttf"));
        assert!(is_supported_font_file("TTF"));
        assert!(is_supported_font_file("otf"));
        assert!(is_supported_font_file("OTF"));
        assert!(!is_supported_font_file("woff"));
    }

    #[test]
    fn test_is_font_mime_type() {
        assert!(is_font_mime_type("application/font-sfnt"));
        assert!(is_font_mime_type("application/x-font-ttf"));
        assert!(is_font_mime_type("application/x-font-opentype"));
        assert!(is_font_mime_type("application/x-font-truetype"));
        assert!(is_font_mime_type("font/otf"));
        assert!(is_font_mime_type("font/sfnt"));
        assert!(is_font_mime_type("font/ttf"));
        assert!(is_font_mime_type("otf"));
        assert!(is_font_mime_type("sfnt"));
        assert!(is_font_mime_type("ttf"));
        assert!(!is_font_mime_type("woff"));
    }

    #[cfg(feature = "add_svg_font_thumbnails")]
    #[test]
    fn test_max_rect() {
        let rect1 = tiny_skia::Rect::from_ltrb(0.0, 0.0, 10.0, 10.0).unwrap();
        let rect2 = tiny_skia::Rect::from_ltrb(5.0, 5.0, 10.0, 10.0).unwrap();
        let max = rect1.max(rect2).unwrap();
        assert_eq!(max.x(), 0.0);
        assert_eq!(max.y(), 0.0);
        assert_eq!(max.width(), 10.0);
        assert_eq!(max.height(), 10.0);

        let empty_rect = tiny_skia::Rect::from_ltrb(0.0, 0.0, 0.0, 0.0).unwrap();
        let max = rect2.max(empty_rect).unwrap();
        assert_eq!(max.x(), 5.0);
        assert_eq!(max.y(), 5.0);
        assert_eq!(max.width(), 5.0);
        assert_eq!(max.height(), 5.0);
        let max = empty_rect.max(rect2).unwrap();
        assert_eq!(max.x(), 5.0);
        assert_eq!(max.y(), 5.0);
        assert_eq!(max.width(), 5.0);
        assert_eq!(max.height(), 5.0);

        let neg_rect = tiny_skia::Rect::from_ltrb(-5.0, -5.0, 10.0, 10.0).unwrap();
        let max = rect2.max(neg_rect).unwrap();
        assert_eq!(max.x(), -5.0);
        assert_eq!(max.y(), -5.0);
        assert_eq!(max.width(), 15.0);
        assert_eq!(max.height(), 15.0);
    }
}
