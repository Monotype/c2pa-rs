// Copyright 2023,2025 Monotype. All rights reserved.
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

use crate::error::Error;

/// Result type for font operations.
pub(crate) type Result<T> = core::result::Result<T, FontError>;

// Types for supporting fonts in any container.

/// Errors that can occur when working with fonts.
#[derive(Debug, thiserror::Error)]
pub enum FontError {
    /// Error related to a font operation.
    #[error(transparent)]
    C2paFontHandlerIoError(#[from] c2pa_font_handler::error::FontIoError),

    /// Error related to a font operation.
    #[error(transparent)]
    C2paFontHandlerSaveError(#[from] c2pa_font_handler::error::FontSaveError),

    /// Bad parameter
    #[error("Bad parameter: {0}")]
    BadParam(String),

    /// Bad head table, when the magic number is not recognized.
    #[error("Bad head table, unknown data format")]
    BadHeadTable,

    /// IO error while dealing with a potentially font-related file/stream.
    #[error(transparent)]
    IoError(#[from] std::io::Error),

    /// Invalid named table encountered.
    #[error("Invalid named table: {0}")]
    InvalidNamedTable(&'static str),

    /// The JUMBF data was not found.
    #[error("The JUMBF data was not found.")]
    JumbfNotFound,

    /// The font's 'C2PA' table contains invalid UTF-8 data.
    #[error("C2PA table manifest data is not valid UTF-8")]
    LoadC2PATableInvalidUtf8,

    /// The font's 'C2PA' table is truncated.
    #[error("C2PA table claimed sizes exceed actual")]
    LoadC2PATableTruncated,

    /// The font's 'head' table is truncated.
    #[error("head table claimed sizes exceed actual")]
    LoadHeadTableTruncated,

    /// The font's 'DSIG' table is truncated.
    #[error("DSIG table claimed sizes exceed actual")]
    LoadDSIGTableTruncated,

    #[error("Failed to save font: {0}")]
    SaveError(#[from] FontSaveError),

    /// Failed to create a string from a byte array.
    #[error("Failed to create a string from a byte array: {0}")]
    StringFromUtf8(#[from] std::string::FromUtf8Error),

    /// Invalid or unsupported font format
    #[error("Invalid or unsupported font format")]
    Unsupported,

    /// Failed to convert a byte array to a string as UTF-8.
    #[error(transparent)]
    Utf8Error(#[from] std::str::Utf8Error),
}

/// Errors that can occur when saving a font.
#[derive(Debug, thiserror::Error)]
pub enum FontSaveError {
    /// Failed to save a font file due to the fact the write operation
    /// failed.
    #[error("Failed to save font because the write operation failed; {0}")]
    FailedToWrite(std::io::Error),

    /// Failed to save a font file due to a state where the font's
    /// table directory has more than just the 'C2PA' table added.
    #[error("Too many tables were added to the font, only expected to add one for C2PA.")]
    TooManyTablesAdded,

    /// Failed to save a font file due to a state where the font's
    /// table directory has more than just the 'C2PA' table removed.
    #[error("Too many tables were removed from the font, only expected to remove one for C2PA.")]
    TooManyTablesRemoved,

    /// Failed to save a font file with an unexpected table found in the
    /// table directory.
    #[error("Unexpected table found in the table directory: {0}")]
    UnexpectedTable(String),

    // Failed to save a font file with a new table which wasn't C2PA.
    #[error("New table added which wasn't C2PA")]
    NonC2PATableAdded,
}

/// Helper method for wrapping a FontError into a crate level error.
pub(crate) fn wrap_font_err(e: FontError) -> Error {
    Error::FontError(e)
}

/// The 'head' table's checksumAdjustment value should be such that the whole-
/// font checksum comes out to this value.
#[allow(dead_code)]
pub(crate) const SFNT_EXPECTED_CHECKSUM: u32 = 0xb1b0afba;
