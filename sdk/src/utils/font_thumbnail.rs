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
use image::{ImageFormat, ImageOutputFormat, Pixel};
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
const THUMBNAIL_MIME_TYPE: &str = "image/png";
/// The text color for the thumbnail
const TEXT_COLOR: Color = Color::rgb(0, 0, 0);
/// The background color for the thumbnail
const BACKGROUND_COLOR: tiny_skia::Color = tiny_skia::Color::WHITE;

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
    /// Failed to find a point size that would accommodate the width and text
    #[error("Failed to find an appropriate font point size to fit the width")]
    FailedToFindAppropriateSize,
    /// Failed to create a Pixmap object
    #[error("Failed to create pixmap")]
    FailedToCreatePixmap,
    /// Failed to get a pixel from the image
    #[error("Failed to get pixel from image; x: {x}, y: {y}")]
    FailedToGetPixel { x: u32, y: u32 },
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

/// Various metrics that apply to the entire font.
///
/// For OpenType fonts, these mostly come from the `OS/2` table.
#[derive(Clone, Copy, Debug)]
pub struct FontMetrics {
    /// The number of font units per em.
    ///
    /// Font sizes are usually expressed in pixels per em; e.g. `12px` means 12
    /// pixels per em.
    pub units_per_em: u32,

    /// The maximum amount the font rises above the baseline, in font units.
    pub ascent: f32,

    /// The maximum amount the font descends below the baseline, in font units.
    ///
    /// NB: This is typically a negative value to match the definition of
    /// `sTypoDescender` in the `OS/2` table in the OpenType specification.
    /// If you are used to using Windows or Mac APIs, beware, as the sign
    /// is reversed from what those APIs return.
    pub descent: f32,
}

impl FontMetrics {
    /// Returns the height of the font in pixels.
    pub fn px_font_height(&self) -> f32 {
        (self.ascent + self.descent.abs()) / self.units_per_em as f32
    }
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

/// Get the font format from the extension
pub fn get_format_from_extension<T: AsRef<OsStr>>(ext: T) -> Option<ImageFormat> {
    match ext.as_ref().to_str() {
        Some("otf") | Some("ttf") => Some(ImageFormat::Png),
        _ => None,
    }
}

/// The MIME type map for the image format of the thumbnail
const MIME_TYPE_MAP: &[(&str, ImageFormat)] = &[
    ("application/font-sfnt", ImageFormat::Png),
    ("application/x-font-ttf", ImageFormat::Png),
    ("application/x-font-opentype", ImageFormat::Png),
    ("application/x-font-truetype", ImageFormat::Png),
    ("font/otf", ImageFormat::Png),
    ("font/sfnt", ImageFormat::Png),
    ("font/ttf", ImageFormat::Png),
    ("otf", ImageFormat::Png),
    ("sfnt", ImageFormat::Png),
    ("ttf", ImageFormat::Png),
];

/// Get the font format from the MIME type
pub fn get_format_from_mime_type(mime: &str) -> Option<ImageFormat> {
    MIME_TYPE_MAP
        .iter()
        .find_map(|(m, f)| if m == &mime { Some(f) } else { None })
        .copied()
}

/// Finds the point size that fits the width and creates a buffer with the text and has it
/// ready for rendering.
/// # Remarks
/// The `font_height` parameter should be in font units.
fn get_buffer_with_pt_size_fits_width(
    text: &str,
    attrs: Attrs,
    font_system: &mut FontSystem,
    font_size: f32,
    font_height: f32,
    width: f32,
    minimum_point_size: f32,
) -> Result<Buffer> {
    // Starting point size
    let mut font_size: f32 = font_size;
    // Generate the line height from the font height
    let mut line_height: f32 = (font_height * font_size).ceil();

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
        // Get the number of layout runs, we expect one if it fits on one line
        let count = borrowed_buffer.layout_runs().count();
        // If it is one, we have found the right size
        if count == 1 {
            let size = measure_text(text, attrs, &mut borrowed_buffer)?;
            // There instances where the measured width was 0, but maybe this is
            // caught now by counting the number of layout runs?
            if size.w > 0.0 && size.w <= width && size.h <= height {
                borrowed_buffer.set_size(size.w, size.h);
                return Ok(buffer);
            }
        }
        // Adjust and prepare to try again
        font_size -= POINT_SIZE_STEP;
        line_height = (font_height * font_size).ceil();

        // Update the buffer with the new font size
        borrowed_buffer.set_metrics(Metrics::new(font_size, line_height));
    }
    // At this point we have reached our minimum size, so setup to use it
    // which will result in text clipping, but that is fine
    font_size = minimum_point_size;
    line_height = (font_height * font_size).ceil();
    borrowed_buffer.set_size(width, line_height);
    borrowed_buffer.set_text(text, attrs, cosmic_text::Shaping::Advanced);
    let size = measure_text(text, attrs, &mut borrowed_buffer)?;
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
    measure_text_in_buffer(buffer)
}

/// Measure the text in the buffer to get the size of the bounding box required
///
/// # Remarks
/// The width may come back as `0.0` if the text is empty or the buffer width is
/// too small.
fn measure_text_in_buffer(buffer: &mut BorrowedWithFontSystem<Buffer>) -> Result<Size> {
    if buffer.size().0 < 0. || buffer.size().1 < 0. {
        return Err(FontThumbnailError::InvalidBufferSize);
    }
    let mut run_width: f32 = 0.0;
    let line_height = buffer.lines.len() as f32 * buffer.metrics().line_height;
    let layout_runs = buffer.layout_runs();
    for run in layout_runs {
        run_width = run_width.max(run.line_w);
    }
    Ok(Size {
        w: run_width.ceil(),
        h: line_height,
    })
}

impl From<Arc<Font>> for FontMetrics {
    fn from(font: Arc<Font>) -> Self {
        // Get the font metrics
        let font_metrics = font.as_swash().metrics(&[]);
        FontMetrics {
            units_per_em: font_metrics.units_per_em as u32,
            ascent: font_metrics.ascent,
            descent: font_metrics.descent,
        }
    }
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
        anti_alias: false,
        ..Default::default()
    }
}

/// Generates a PNG thumbnail from a font file
/// # Returns
/// Returns Result `(format, image_bits)` if successful, otherwise `Error`
#[cfg(feature = "file_io")]
pub fn make_thumbnail(
    path: &std::path::Path,
) -> std::result::Result<(String, Vec<u8>), crate::error::Error> {
    let mut font_data = std::fs::read(path)?;
    let mut font_data = std::io::Cursor::new(&mut font_data);
    make_thumbnail_from_stream(&mut font_data)
}

/// Make a PNG thumbnail from a stream, which should be font data bits.
/// # Returns
/// Returns Result `(format, image_bits)` if successful, otherwise `Error`
pub fn make_thumbnail_from_stream<R: Read + Seek + ?Sized>(
    stream: &mut R,
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
    let font_info = FontNameInfo::from(f.clone());
    let font_metrics = FontMetrics::from(f.clone());
    let full_name = font_info
        .full_name
        .ok_or(FontThumbnailError::NoFullNameFound)?;

    // Create a swash cache for the font system, to cache rendering
    let mut swash_cache = SwashCache::new();

    // Get the font height in pixels
    let font_height = font_metrics.px_font_height();
    // Find a buffer that fits the width
    let mut buffer = get_buffer_with_pt_size_fits_width(
        &full_name,
        attrs,
        &mut font_system,
        STARTING_POINT_SIZE,
        font_height,
        MAXIMUM_WIDTH as f32,
        MINIMUM_POINT_SIZE,
    )?;
    // Borrow the buffer with the font system, to make things easier to make
    // calls
    let mut buffer = buffer.borrow_with(&mut font_system);

    // Grab the actual width and height of the buffer for the image
    let (width, height) = buffer.size();

    // Create a new pixel map for the main text
    let mut img =
        Pixmap::new(width as u32, height as u32).ok_or(FontThumbnailError::FailedToCreatePixmap)?;
    // Draw the text into the pixel map
    buffer.draw(&mut swash_cache, TEXT_COLOR, |x, y, w, h, color| {
        if let Some(rect) = tiny_skia::Rect::from_xywh(x as f32, y as f32, w as f32, h as f32)
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
    Ok((THUMBNAIL_MIME_TYPE.to_string(), png_buffer))
}
