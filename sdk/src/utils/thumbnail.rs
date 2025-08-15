// Copyright 2022 Adobe. All rights reserved.
// This file is licensed to you under the Apache License,
// Version 2.0 (http://www.apache.org/licenses/LICENSE-2.0)
// or the MIT license (http://opensource.org/licenses/MIT),
// at your option.

// Unless required by applicable law or agreed to in writing,
// this software is distributed on an "AS IS" BASIS, WITHOUT
// WARRANTIES OR REPRESENTATIONS OF ANY KIND, either express or
// implied. See the LICENSE-MIT and LICENSE-APACHE files for the
// specific language governing permissions and limitations under
// each license.

use std::io::{Read, Seek};

use image::{io::Reader, ImageFormat};

use crate::{Error, Result};

// max edge size allowed in pixels for thumbnail creation
const THUMBNAIL_LONGEST_EDGE: u32 = 1024;
const THUMBNAIL_JPEG_QUALITY: u8 = 80;

///  utility to generate a thumbnail from a file at path
/// returns Result (format, image_bits) if successful, otherwise Error
#[cfg(feature = "file_io")]
pub fn make_thumbnail(path: &std::path::Path) -> Result<(String, Vec<u8>)> {
    #[cfg(all(feature = "sfnt", feature = "add_font_thumbnails"))]
    {
        if path
            .extension()
            .map(font_thumbnail::is_supported_font_file)
            .is_some_and(|x| x)
        {
            return font_thumbnail::make_thumbnail(
                path,
                Some(cfg!(feature = "add_svg_font_thumbnails")),
            );
        }
    }
    let format = ImageFormat::from_path(path)?;

    let mut img = image::open(path)?;
    let longest_edge = THUMBNAIL_LONGEST_EDGE;

    // generate a thumbnail image scaled down and in jpeg format
    if img.width() > longest_edge || img.height() > longest_edge {
        img = img.thumbnail(longest_edge, longest_edge);
    }
    // for png files, use png thumbnails if there is an alpha channel
    // for other supported types try a jpeg thumbnail
    let (output_format, format) = match format {
        ImageFormat::Png if img.color().has_alpha() => (image::ImageOutputFormat::Png, "image/png"),
        _ => (
            image::ImageOutputFormat::Jpeg(THUMBNAIL_JPEG_QUALITY),
            "image/jpeg",
        ),
    };
    let thumbnail_bits = Vec::new();
    let mut cursor = std::io::Cursor::new(thumbnail_bits);
    img.write_to(&mut cursor, output_format)?;

    let format = format.to_owned();
    Ok((format, cursor.into_inner()))
}

///  utility to generate a thumbnail from a file at path
/// returns Result (format, image_bits) if successful, otherwise Error
pub fn make_thumbnail_from_stream<R: Read + Seek + ?Sized>(
    format: &str,
    stream: &mut R,
) -> Result<(String, Vec<u8>)> {
    #[cfg(all(feature = "sfnt", feature = "add_font_thumbnails"))]
    {
        if font_thumbnail::is_supported_font_file(format)
            || font_thumbnail::is_font_mime_type(format)
        {
            return font_thumbnail::make_thumbnail_from_stream(
                stream,
                Some(cfg!(feature = "add_svg_font_thumbnails")),
            );
        }
    }

    let format = ImageFormat::from_extension(format)
        .or_else(|| ImageFormat::from_mime_type(format))
        .ok_or(Error::UnsupportedType)?;

    let reader = Reader::with_format(std::io::BufReader::new(stream), format);
    let mut img = reader.decode()?;

    let longest_edge = THUMBNAIL_LONGEST_EDGE;

    // generate a thumbnail image scaled down and in jpeg format
    if img.width() > longest_edge || img.height() > longest_edge {
        img = img.thumbnail(longest_edge, longest_edge);
    }

    // for png files, use png thumbnails for transparency
    // for other supported types try a jpeg thumbnail
    let (output_format, format) = match format {
        ImageFormat::Png => (image::ImageOutputFormat::Png, "image/png"),
        _ => (
            image::ImageOutputFormat::Jpeg(THUMBNAIL_JPEG_QUALITY),
            "image/jpeg",
        ),
    };
    let thumbnail_bits = Vec::new();
    let mut cursor = std::io::Cursor::new(thumbnail_bits);
    img.write_to(&mut cursor, output_format)?;

    let format = format.to_owned();
    Ok((format, cursor.into_inner()))
}

#[cfg(all(feature = "sfnt", feature = "add_font_thumbnails"))]
#[path = "font_thumbnail.rs"]
mod font_thumbnail;

mod tests {
    /// This test is in place to test the main thumbnail creation, making sure it calls down
    /// into the font thumbnail creation logic. We have it here
    #[cfg(feature = "add_svg_font_thumbnails")]
    #[test]
    fn test_svg_creation_with_stream() {
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
