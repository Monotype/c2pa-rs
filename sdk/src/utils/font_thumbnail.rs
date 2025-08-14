// Copyright 2024 Monotype Imaging Inc.

//cspell:ignore sfnt thiserror

use std::{
    ffi::OsStr,
    io::{Read, Seek},
};

use c2pa_font_handler::thumbnail::{
    BinarySearchContext, CosmicTextThumbnailGenerator, FontSystemConfig, PngThumbnailRenderer,
    Renderer, SvgThumbnailRenderer, ThumbnailGenerator,
};

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

/// The line height factor for the thumbnail
const LINE_HEIGHT_FACTOR: f32 = 1.075;
/// How much padding to use on the left and right sides of the text
const TOTAL_WIDTH_PADDING: f32 = 0.1;

/// Errors that can occur when creating a font thumbnail
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
enum FontThumbnailError {
    /// Error from the font thumbnail generator
    #[error(transparent)]
    C2paFontHandler(#[from] c2pa_font_handler::thumbnail::error::FontThumbnailError),
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

/// Get the SVG thumbnail renderer, but only if the feature is enabled
#[cfg(not(feature = "add_svg_font_thumbnails"))]
fn get_svg_renderer() -> Result<Box<dyn Renderer>> {
    Err(FontThumbnailError::SvgFeatureNotEnabled)
}

/// Get the SVG thumbnail renderer
#[cfg(feature = "add_svg_font_thumbnails")]
fn get_svg_renderer() -> Result<Box<dyn Renderer>> {
    Ok(Box::new(SvgThumbnailRenderer::default()))
}

/// Get the PN thumbnail renderer
fn get_png_renderer() -> Result<Box<dyn Renderer>> {
    Ok(Box::new(PngThumbnailRenderer::default()))
}

/// Gets the font system configuration to use with the thumbnail generator
#[inline]
fn get_font_system_config<'a>() -> FontSystemConfig<'a> {
    FontSystemConfig::builder()
        .default_locale(DEFAULT_LOCALE)
        .line_height_factor(LINE_HEIGHT_FACTOR)
        .maximum_width(MAXIMUM_WIDTH)
        .total_width_padding(TOTAL_WIDTH_PADDING)
        .search_strategy(
            c2pa_font_handler::thumbnail::FontSizeSearchStrategy::Binary(BinarySearchContext::new(
                STARTING_POINT_SIZE,
                POINT_SIZE_STEP,
                MINIMUM_POINT_SIZE,
            )),
        )
        .build()
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
    let renderer: Box<dyn Renderer> = if use_svg.unwrap_or(false) {
        get_svg_renderer()?
    } else {
        get_png_renderer()?
    };
    let generator =
        CosmicTextThumbnailGenerator::new_with_config(renderer, get_font_system_config());
    let thumbnail = generator
        .create_thumbnail_from_stream(stream, None)
        .map_err(FontThumbnailError::from)?;
    let (data, mime_type) = thumbnail.into_parts();
    Ok((mime_type, data))
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
    fn test_svg_creation() {
        use crate::utils::thumbnail::make_thumbnail_from_stream;

        // Use a test fixture for generating a thumbnail from a font
        let font_data = include_bytes!("../../tests/fixtures/font.otf");
        let mut stream = std::io::Cursor::new(font_data);
        // Make the thumbnail
        let result = make_thumbnail_from_stream("font/otf", &mut stream);
        assert!(result.is_ok());
        let (mime_type, image_data) = result.unwrap();
        // Assert the result is a valid SVG
        assert_eq!(mime_type, "image/svg+xml");
        // And the image data is NOT empty
        assert!(!image_data.is_empty());
        // Matter of fact, make sure it matches the expected output
        let expected_svg = include_bytes!("../../tests/fixtures/font.thumbnail.svg");
        let strip_all = |s: &str| s.split_whitespace().collect::<String>();
        assert_eq!(
            strip_all(&String::from_utf8_lossy(&image_data)),
            strip_all(&String::from_utf8_lossy(expected_svg))
        );
    }
}
