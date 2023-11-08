// Copyright 2023 Monotype. All rights reserved.
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
use std::{
    collections::BTreeMap,
    convert::TryFrom,
    fs::File,
    io::{BufReader, Cursor, Read, Seek, SeekFrom, Write},
    mem::size_of,
    path::*,
};

use byteorder::{BigEndian, ReadBytesExt, WriteBytesExt};
use log::trace;
use serde_bytes::ByteBuf;
use tempfile::TempDir;
use uuid::Uuid;

use crate::{
    assertions::BoxMap,
    asset_io::{
        AssetBoxHash, AssetIO, CAIRead, CAIReadWrite, CAIReader, CAIWriter, HashBlockObjectType,
        HashObjectPositions, RemoteRefEmbed, RemoteRefEmbedType,
    },
    error::{Error, Result},
};

/// This module is a temporary implementation of a very basic support for XMP in
/// fonts. Ideally the reading/writing of the following should be independent of
/// XMP support:
///
/// - `InstanceID` - Unique identifier of the instance
/// - `DocumentID` - Unique identifier of the document
/// - `Provenance` - Url/Uri of the provenance
///
/// But as the authoring of this module the Rust SDK still assumes the remote
/// manifest reference comes from the XMP when building the `Ingredient` (as
/// seen in the ingredient module). The instance/document ID also are read from
/// the XMP when building the ingredient, so until this type of logic has been
/// abstracted this is meant as a temporary hack to provide a working
/// implementation for remote manifests.
///
/// ### Remarks
///
/// This module depends on the `feature = "xmp_write"` to be enabled.
#[cfg(feature = "xmp_write")]
mod font_xmp_support {
    use xmp_toolkit::{FromStrOptions, XmpError, XmpErrorType, XmpMeta};

    use super::*;

    /// Creates a default `XmpMeta` object for fonts
    ///
    /// ### Arguments
    ///
    /// - `document_id` - optional unique identifier for the document
    /// - `instance_id` - optional unique identifier for the instance
    ///
    /// ### Remarks
    ///
    /// Default/random values will be used for the document/instance IDs as
    /// needed.
    fn default_font_xmp_meta(
        document_id: Option<String>,
        instance_id: Option<String>,
    ) -> Result<XmpMeta> {
        // But we could build up a default document/instance ID for a font and
        // use it to seed the data. Doing this would only make sense if creating
        // and writing the data to the font
        let xmp_mm_namespace = "http://ns.adobe.com/xap/1.0/mm/";
        // If there was no reference in the stream, then we build up
        // a default XMP data
        let mut xmp_meta = XmpMeta::new().map_err(xmp_write_err)?;

        // Add a document ID
        xmp_meta
            .set_property(
                xmp_mm_namespace,
                "DocumentID",
                // Use the supplied document ID or default to one if needed
                &document_id.unwrap_or(WoffIO::default_document_id()).into(),
            )
            .map_err(xmp_write_err)?;

        // Add an instance ID
        xmp_meta
            .set_property(
                xmp_mm_namespace,
                "InstanceID",
                // Use the supplied instance ID or default to one if needed
                &instance_id.unwrap_or(WoffIO::default_instance_id()).into(),
            )
            .map_err(xmp_write_err)?;

        Ok(xmp_meta)
    }

    /// Builds a `XmpMeta` element from the data within the source stream
    ///
    /// ### Parameters
    ///
    /// - `source` - Source stream to read data from to build the `XmpMeta` object
    ///
    /// ### Returns
    ///
    /// A new `XmpMeta` object, either based on information that already exists in
    /// the stream or using defaults
    ///
    /// ### Remarks
    /// The use of this function really shouldn't be needed, but currently the SDK
    /// is tightly coupled to the use of XMP with assets.
    pub fn build_xmp_from_stream<TSource>(source: &mut TSource) -> Result<XmpMeta>
    where
        TSource: Read + Seek + ?Sized,
    {
        match read_reference_from_stream(source)? {
            // For now we pretend the reference read from the stream is really XMP
            // data
            Some(xmp) => {
                // If we did have reference data in the stream, we assume it is
                // really XMP data, and will read as such
                XmpMeta::from_str_with_options(xmp.as_str(), FromStrOptions::default())
                    .map_err(xmp_write_err)
            }
            // Mention there is no data representing XMP found
            None => Err(Error::NotFound),
        }
    }

    /// Maps the errors from the xmp_toolkit crate
    ///
    /// ### Parameters
    ///
    /// - `err` - The `XmpError` to map to an internal error type
    ///
    /// ### Remarks
    /// This is nearly a copy/paste from `embedded_xmp` crate, we should clean this
    /// up at some point
    fn xmp_write_err(err: XmpError) -> crate::Error {
        match err.error_type {
            // convert to OS permission error code so we can detect it correctly upstream
            XmpErrorType::FilePermission => Error::IoError(std::io::Error::from_raw_os_error(13)),
            XmpErrorType::NoFile => Error::NotFound,
            XmpErrorType::NoFileHandler => Error::UnsupportedType,
            _ => Error::XmpWriteError,
        }
    }

    /// Adds a C2PA manifest reference as XMP data to a font file
    ///
    /// ### Parameters
    /// - `font_path` - Path to the font file to add the reference to
    /// - `manifest_uri` - A C2PA manifest URI (JUMBF or URL based)
    ///
    /// ### Remarks
    /// This method is considered a stop-gap for now until the official SDK
    /// offers a more generic method to indicate a document ID, instance ID,
    /// and a reference to the a remote manifest.
    pub fn add_reference_as_xmp_to_font(font_path: &Path, manifest_uri: &str) -> Result<()> {
        process_file_with_streams(font_path, move |input_stream, temp_file| {
            // Write the manifest URI to the stream
            add_reference_as_xmp_to_stream(input_stream, temp_file.get_mut_file(), manifest_uri)
        })
    }

    /// Adds a C2PA manifest reference as XMP data to the stream
    ///
    /// ### Parameters
    /// - `source` - Source stream to read from
    /// - `destination` - Destination stream to write the reference to
    /// - `reference` - A C2PA manifest URI (JUMBF or URL based)
    ///
    /// ### Remarks
    /// This method is considered a stop-gap for now until the official SDK
    /// offers a more generic method to indicate a document ID, instance ID,
    /// and a reference to the a remote manifest.
    #[allow(dead_code)]
    pub fn add_reference_as_xmp_to_stream<TSource, TDest>(
        source: &mut TSource,
        destination: &mut TDest,
        manifest_uri: &str,
    ) -> Result<()>
    where
        TSource: Read + Seek + ?Sized,
        TDest: Write + ?Sized,
    {
        // We must register the namespace for dcterms, to be able to set the
        // provenance
        XmpMeta::register_namespace("http://purl.org/dc/terms/", "dcterms")
            .map_err(xmp_write_err)?;
        // Build a simple XMP meta element from the current source stream
        let mut xmp_meta = match build_xmp_from_stream(source) {
            // Use the data already available
            Ok(meta) => meta,
            // If data was not found for building out the XMP, we will default
            // to some good starting points
            Err(Error::NotFound) => default_font_xmp_meta(None, None)?,
            // At this point, the font is considered to be invalid possibly
            Err(error) => return Err(error),
        };
        // Reset the source stream to the beginning
        source.seek(SeekFrom::Start(0))?;
        // We don't really care if there was a provenance before, since we are
        // writing a new one we will either be adding or overwriting what
        // was there.
        xmp_meta
            .set_property(
                "http://purl.org/dc/terms/",
                "provenance",
                &manifest_uri.into(),
            )
            .map_err(xmp_write_err)?;
        // Finally write the XMP data as a string to the stream
        add_reference_to_stream(source, destination, &xmp_meta.to_string())?;

        Ok(())
    }
}

struct TempFile {
    // The temp_dir must be referenced during the duration of the use of the
    // temporary file, as soon as it is dropped the temporary directory and the
    // contents thereof are deleted
    #[allow(dead_code)]
    temp_dir: TempDir,
    path: Box<Path>,
    file: File,
}

impl TempFile {
    /// Creates a new temporary file within the `env::temp_dir()` directory,
    /// which should be deleted once the object is dropped.
    ///
    /// ## Arguments
    ///
    /// * `base_name` - Base name to use for the temporary file name
    pub fn new(base_name: &Path) -> Result<Self> {
        let temp_dir = TempDir::new()?;
        let temp_dir_path = temp_dir.path();
        let path = temp_dir_path.join(
            base_name
                .file_name()
                .ok_or_else(|| Error::BadParam("Invalid file name".to_string()))?,
        );
        let file = File::create(&path)?;
        Ok(Self {
            temp_dir,
            path: path.into(),
            file,
        })
    }

    /// Get the path of the temporary file
    pub fn get_path(&self) -> &Path {
        self.path.as_ref()
    }

    /// Get a mutable reference to the temporary file
    pub fn get_mut_file(&mut self) -> &mut File {
        &mut self.file
    }
}

/// Supported extension and mime-types
static SUPPORTED_TYPES: [&str; 4] = [
    "application/font-woff",
    "application/x-font-woff",
    "font/woff",
    "woff",
];

/// Four-character tag which names a font table.
#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
struct TableTag {
    data: [u8; 4],
}

impl TableTag {
    /// Construct a new TableTag with the given value.
    pub fn new(source_data: [u8; 4]) -> Self {
        Self { data: source_data }
    }

    /// Read a new TableTag from the given source.
    pub fn new_from_reader<T: Read + Seek + ?Sized>(reader: &mut T) -> Result<Self> {
        Ok(Self::new([
            reader.read_u8()?,
            reader.read_u8()?,
            reader.read_u8()?,
            reader.read_u8()?,
        ]))
    }

    /// Serialized this tag data to the given writer.
    pub fn write<TDest: Write + ?Sized>(&self, destination: &mut TDest) -> Result<()> {
        destination.write_all(&self.data[..])?;
        Ok(())
    }
}

impl std::fmt::Display for TableTag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}{}{}{}",
            self.data[0], self.data[1], self.data[2], self.data[3]
        )
    }
}

impl std::fmt::Debug for TableTag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}{}{}{}",
            self.data[0], self.data[1], self.data[2], self.data[3]
        )
    }
}

/// 32-bit font-format identification magic number.
///
/// Embedded OpenType and MicroType Express formats cannot be detected with a
/// simple magic-number sniff. Conceivably, EOT could be dealt with as a
/// variation on SFNT, but MTX needs more exotic handling.
enum Magic {
    /// 'OTTO' - OpenType
    OpenType = 0x4f54544f,
    /// FIXED 1.0 - TrueType (or possibly v1.0 Embedded OpenType)
    TrueType = 0x00010000,
    /// 'wOFF' - WOFF 1.0
    Woff = 0x774f4646,
    /// 'wOF2' - WOFF 2.0
    Woff2 = 0x774f4632,
}

/// Tags for the font tables we care about.

/// Tag for the 'C2PA' table.
const C2PA_TABLE_TAG: TableTag = TableTag { data: *b"C2PA" };

/// Tag for the 'head' table in a font.
const HEAD_TABLE_TAG: TableTag = TableTag { data: *b"head" };

/// WOFF 1.0 WOFFHeader
const WOFF_HEADER_CHUNK_NAME: TableTag = TableTag {
    data: *b"\x00\x00\x00w",
};
// Then SFNT header could be *b"\x00\x00\x00w", and all the "header" chunks
// will automatically BTreeMap to start-of-file.

/// Pseudo-tag for the table directory.
const WOFF_DIRECTORY_CHUNK_NAME: TableTag = TableTag {
    data: *b"\x00\x00\x01D",
}; // Sorts to just-after HEADER tag.

/// WOFF 1.0 / 2.0 trailing XML metadata
const WOFF_METADATA_CHUNK_NAME: TableTag = TableTag {
    data: *b"\x7F\x7F\x7FM",
}; // Sorts to the penultimate position.

/// WOFF 1.0 / 2.0 trailing private data
const WOFF_PRIVATE_CHUNK_NAME: TableTag = TableTag {
    data: *b"\x7F\x7F\x7FP",
}; // Sorts to the very end.

/// Used to attempt conversion from u32 to a Magic value.
impl TryFrom<u32> for Magic {
    type Error = crate::error::Error;

    /// Tries to convert from u32 to a valid font version.
    fn try_from(v: u32) -> core::result::Result<Self, Self::Error> {
        match v {
            tt if tt == Magic::TrueType as u32 => Ok(Magic::TrueType),
            ot if ot == Magic::OpenType as u32 => Ok(Magic::OpenType),
            w1 if w1 == Magic::Woff as u32 => Ok(Magic::Woff),
            w2 if w2 == Magic::Woff2 as u32 => Ok(Magic::Woff2),
            _unknown => Err(Error::FontUnknownMagic),
        }
    }
}

/// 'C2PA' font table - in storage
#[derive(Debug)]
#[repr(C, packed(4))] // As defined by the C2PA spec.
#[allow(non_snake_case)] // As named by the C2PA spec.
struct TableC2PARaw {
    majorVersion: u16,
    minorVersion: u16,
    activeManifestUriOffset: u32,
    activeManifestUriLength: u16,
    reserved: u16,
    manifestStoreOffset: u32,
    manifestStoreLength: u32,
}
impl TableC2PARaw {
    fn write<TDest: Write + ?Sized>(&mut self, destination: &mut TDest) -> Result<()> {
        destination.write_u16::<BigEndian>(self.majorVersion)?;
        destination.write_u16::<BigEndian>(self.minorVersion)?;
        destination.write_u32::<BigEndian>(self.activeManifestUriOffset)?;
        destination.write_u16::<BigEndian>(self.activeManifestUriLength)?;
        destination.write_u32::<BigEndian>(self.manifestStoreOffset)?;
        destination.write_u32::<BigEndian>(self.manifestStoreLength)?;
        Ok(())
    }
}

/// 'C2PA' font table - after loading from storage
#[derive(Clone, Debug)]
struct TableC2PA {
    /// Major version of the C2PA table record
    major_version: u16,
    /// Minor version of the C2PA table record
    minor_version: u16,
    /// Optional URI to an active manifest
    active_manifest_uri: Option<String>,
    /// Optional embedded manifest store
    manifest_store: Option<Vec<u8>>,
}

impl TableC2PA {
    /// Creates a new C2PA table with the given values.
    pub fn new(active_manifest_uri: Option<String>, manifest_store: Option<Vec<u8>>) -> Self {
        Self {
            active_manifest_uri,
            manifest_store,
            ..TableC2PA::default()
        }
    }

    /// Create the checksum for this table
    fn checksum(&self) -> Result<u32> {
        // Serialize self to a throwaway stream
        let mut stream = Cursor::new(Vec::new());
        match self.write(&mut stream) {
            Ok(()) => (),
            Err(error) => return Err(error),
        }
        // Compute checksum of stream
        stream.seek(SeekFrom::Start(0)).unwrap();
        let mut cksum: u32 = 0;
        while stream.get_ref().len() > 4 {
            let ckword: u32 = stream.read_u32::<BigEndian>()?;
            cksum += ckword;
        }
        if stream.get_ref().len() > 0 {
            let mut ckfrag: u32 = 0;
            let mut factor: u32 = 256 * 256 * 256;
            while stream.get_ref().len() > 0 {
                let ckbyte = stream.read_u8()?;
                ckfrag += ckbyte as u32 * factor;
                factor /= 256;
            }
            cksum += ckfrag;
        }
        return Ok(cksum);
    }

    /// Creates a new C2PA table from the given stream.
    pub fn new_from_reader<T: Read + Seek + ?Sized>(
        reader: &mut T,
        offset: u64,
        size: usize,
    ) -> core::result::Result<TableC2PA, Error> {
        reader.seek(SeekFrom::Start(offset))?;
        if size < size_of::<TableC2PARaw>() {
            Err(Error::FontLoadError)?
        } else {
            todo!("Ookay, do this...")
            // Old implementation, for reference...
            //
            //impl Deserialize for TableC2PA {
            //    fn from_bytes(c: &mut ReaderContext) -> Result<Self, DeserializationError> {
            //        let mut active_manifest_uri: Option<String> = None;
            //        let mut manifest_store: Option<Box<[u8]>> = None;
            //        // Save the pointer of the current reader context, before we read the
            //        // internal record for obtaining the offset from the beginning of the
            //        // table to the data as to specification.
            //        c.push();
            //
            //        // Read the components of the C2PA header
            //        let internal_record: C2PARecordInternal = c.de()?;
            //
            //        if internal_record.activeManifestUriOffset > 0 {
            //            // Offset to the active manifest URI
            //            c.ptr = c.top_of_table() + internal_record.activeManifestUriOffset as usize;
            //            // Reading in the active URI as bytes
            //            let uri_as_bytes: Vec<u8> =
            //                c.de_counted(internal_record.activeManifestUriLength as usize)?;
            //            // And converting to a string read as UTF-8 encoding
            //            active_manifest_uri = Some(
            //                str::from_utf8(&uri_as_bytes)
            //                    .map_err(|_| {
            //                        DeserializationError("Failed to read UTF-8 string from bytes".to_string())
            //                    })?
            //                    .to_string(),
            //            );
            //        }
            //
            //        if internal_record.manifestStoreOffset > 0 {
            //            // Reset the offset to the C2PA manifest store
            //            c.ptr = c.top_of_table() + internal_record.manifestStoreOffset as usize;
            //            // Read the store as bytes
            //            let store_as_bytes: Option<Vec<u8>> =
            //                Some(c.de_counted(internal_record.manifestStoreLength as usize)?);
            //            // And then convert to a string as UTF-8 bytes
            //            manifest_store = store_as_bytes.map(|d| d.into_boxed_slice());
            //        }
            //
            //        // Restore the state of the reader
            //        c.pop();
            //
            //        // Return our record
            //        Ok(C2PA {
            //            majorVersion: internal_record.majorVersion,
            //            minorVersion: internal_record.minorVersion,
            //            active_manifest_uri: active_manifest_uri,
            //            manifestStore: manifest_store,
            //        })
            //    }
            //}
        }
    }

    /// Get the manifest store data if available
    pub fn get_manifest_store(&self) -> Option<&[u8]> {
        self.manifest_store.as_deref()
    }

    /// Serialize this C2PA table to the given writer.
    fn write<TDest: Write + ?Sized>(&self, destination: &mut TDest) -> Result<()> {
        // Set up the structured data
        let mut raw_table = TableC2PARaw {
            majorVersion: self.major_version,
            minorVersion: self.minor_version,
            activeManifestUriOffset: 0,
            reserved: 0,
            activeManifestUriLength: 0,
            manifestStoreOffset: 0,
            manifestStoreLength: 0,
        };
        // If a remote URI is present, prepare to store it.
        if let Some(uri_string) = self.active_manifest_uri.as_ref() {
            raw_table.activeManifestUriOffset = size_of::<TableC2PARaw>() as u32;
            raw_table.activeManifestUriLength = uri_string.len() as u16;
        }
        // If a local store is present, prepare to store it.
        if let Some(manifest_store) = self.manifest_store.as_ref() {
            raw_table.manifestStoreOffset =
                raw_table.activeManifestUriOffset + raw_table.activeManifestUriLength as u32;
            raw_table.manifestStoreLength = manifest_store.len() as u32;
        }
        // Write the table data
        raw_table.write(destination)?;
        // Write the remote manifest URI, if present.
        if let Some(uri_string) = self.active_manifest_uri.as_ref() {
            destination.write_all(uri_string.as_bytes())?;
        }
        // Write out the local manifest store, if present.
        if let Some(manifest_store) = self.manifest_store.as_ref() {
            destination.write_all(manifest_store)?;
        }
        // Done
        Ok(())
    }
}

impl Default for TableC2PA {
    fn default() -> Self {
        Self {
            major_version: 0,
            minor_version: 1,
            active_manifest_uri: Default::default(),
            manifest_store: Default::default(),
        }
    }
}

/// 'head' font table. For now, there is no need for a 'raw' variant, since only
/// byte-swapping is needed.
#[derive(Debug)]
#[repr(C, packed(4))]
// As defined by Open Font Format / OpenType (though we don't as yet directly support exotics like FIXED).
#[allow(non_snake_case)] // As named by Open Font Format / OpenType.
struct TableHead {
    majorVersion: u16,
    minorVersion: u16,
    fontRevision: u32,
    checksumAdjustment: u32,
    magicNumber: u32,
    flags: u16,
    unitsPerEm: u16,
    created: i64,
    modified: i64,
    xMin: i16,
    yMin: i16,
    xMax: i16,
    yMax: i16,
    macStyle: u16,
    lowestRecPPEM: u16,
    fontDirectionHint: i16,
    indexToLocFormat: i16,
    glyphDataFormat: i16,
}

impl TableHead {
    /// Creates a `head` table from the given stream.
    pub fn _new_from_reader<T: Read + Seek + ?Sized>(
        reader: &mut T,
        offset: u64,
        size: usize,
    ) -> core::result::Result<TableHead, Error> {
        reader.seek(SeekFrom::Start(offset))?;
        if size != size_of::<TableHead>() {
            Err(Error::FontLoadError)?
        } else {
            Ok(Self {
                majorVersion: reader.read_u16::<BigEndian>()?,
                minorVersion: reader.read_u16::<BigEndian>()?,
                fontRevision: reader.read_u32::<BigEndian>()?,
                checksumAdjustment: reader.read_u32::<BigEndian>()?,
                magicNumber: reader.read_u32::<BigEndian>()?,
                flags: reader.read_u16::<BigEndian>()?,
                unitsPerEm: reader.read_u16::<BigEndian>()?,
                created: reader.read_i64::<BigEndian>()?,
                modified: reader.read_i64::<BigEndian>()?,
                xMin: reader.read_i16::<BigEndian>()?,
                yMin: reader.read_i16::<BigEndian>()?,
                xMax: reader.read_i16::<BigEndian>()?,
                yMax: reader.read_i16::<BigEndian>()?,
                macStyle: reader.read_u16::<BigEndian>()?,
                lowestRecPPEM: reader.read_u16::<BigEndian>()?,
                fontDirectionHint: reader.read_i16::<BigEndian>()?,
                indexToLocFormat: reader.read_i16::<BigEndian>()?,
                glyphDataFormat: reader.read_i16::<BigEndian>()?,
            })
        }
    }

    /// Serialize this head table to the given writer.
    fn _write<TDest: Write + ?Sized>(&mut self, destination: &mut TDest) -> Result<()> {
        destination.write_u16::<BigEndian>(self.majorVersion)?;
        destination.write_u16::<BigEndian>(self.minorVersion)?;
        destination.write_u32::<BigEndian>(self.fontRevision)?;
        destination.write_u32::<BigEndian>(self.checksumAdjustment)?;
        destination.write_u32::<BigEndian>(self.magicNumber)?;
        destination.write_u16::<BigEndian>(self.flags)?;
        destination.write_u16::<BigEndian>(self.unitsPerEm)?;
        destination.write_i64::<BigEndian>(self.created)?;
        destination.write_i64::<BigEndian>(self.modified)?;
        destination.write_u16::<BigEndian>(self.unitsPerEm)?;
        destination.write_i16::<BigEndian>(self.xMin)?;
        destination.write_i16::<BigEndian>(self.yMin)?;
        destination.write_i16::<BigEndian>(self.xMax)?;
        destination.write_i16::<BigEndian>(self.yMax)?;
        destination.write_u16::<BigEndian>(self.macStyle)?;
        destination.write_u16::<BigEndian>(self.lowestRecPPEM)?;
        destination.write_i16::<BigEndian>(self.fontDirectionHint)?;
        destination.write_i16::<BigEndian>(self.indexToLocFormat)?;
        destination.write_i16::<BigEndian>(self.glyphDataFormat)?;
        Ok(())
    }
}

/// Generic font table with unknown contents.
#[derive(Debug)]
struct TableUnspecified {
    data: Vec<u8>,
}

/// Any font table.
impl TableUnspecified {
    /// Creates an unspecified table from the given stream.
    pub fn new_from_reader<T: Read + Seek + ?Sized>(
        reader: &mut T,
        offset: u64,
        size: usize,
    ) -> core::result::Result<TableUnspecified, Error> {
        let mut raw_table_data: Vec<u8> = vec![0; size];
        reader.seek(SeekFrom::Start(offset))?;
        reader.read_exact(&mut raw_table_data)?;
        Ok(Self {
            data: raw_table_data,
        })
    }

    /// Serialized this table data to the given writer.
    fn write<TDest: Write + ?Sized>(&self, destination: &mut TDest) -> Result<()> {
        Ok(destination.write_all(&self.data[..])?)
    }
}

/// Possible tables
#[derive(Debug)]
enum Table {
    /// 'C2PA' table
    C2PA(TableC2PA),
    /// 'head' table
    //Head(TableHead),
    /// any other table
    Unspecified(TableUnspecified),
}

/// Implementation of a WOFF 1.0 font
struct WoffFont {
    header: WoffHeader,
    directory: WoffDirectory,
    /// All the Tables in this font, keyed by TableTag.
    // TBD - WOFF2 - For this format, the Table Directory entries are not
    // sorted by tag; rather, their order determines the physical order of the
    // tables themselves. Therefore, a BTreeMap keyed by tag is not appropriate;
    // we need to generalize the concept of table order across font container
    // types. Design considerations include:
    // - Any motivation to _not_ ensure that SFNTs/WOFF1s are sorted correctly;
    //   if our goal is simply to describe the provenance _of an already-existing asset_,
    //   then perhaps we need to permit incorrectly-ordered SFNTs/WOFF1s? In that
    //   case, then perhaps the selection of BTreeMap versus Vec is done at
    //   construction time, ensuring the desired behavior?
    // - Otherwise, we could just use BTreeMap for SFNT/WOFF1 and Vec for WOFF2
    // - Other matters?
    tables: BTreeMap<TableTag, Table>,
    meta: Option<TableUnspecified>,
    private: Option<TableUnspecified>,
}

impl WoffFont {
    /// Reads in a WOFF 1 font file from the given stream.
    fn new_from_reader<T: Read + Seek + ?Sized>(
        reader: &mut T,
    ) -> core::result::Result<WoffFont, Error> {
        // Read in the WOFFHeader & record its chunk.
        // We expect to be called with the stream positioned just past the
        // magic number.
        let woff_hdr = WoffHeader::new_from_reader(reader)?;

        // After the header should be the directory.
        let woff_dir = WoffDirectory::new_from_reader(reader, woff_hdr.numTables as usize)?;

        // With that, we can construct the tables
        let mut woff_tables = BTreeMap::new();

        for entry in woff_dir.entries.iter() {
            // Try to parse the next dir entry
            let offset: u64 = entry.offset as u64;
            let size: usize = entry.compLength as usize;
            // Create a table instance for it.
            let table: Table = {
                match entry.tag {
                    C2PA_TABLE_TAG => {
                        Table::C2PA(TableC2PA::new_from_reader(reader, offset, size)?)
                    }
                    HEAD_TABLE_TAG => {
                        // Soon, Table::Head(TableHead::new_from_reader(reader, offset, size)?)
                        Table::Unspecified(TableUnspecified::new_from_reader(reader, offset, size)?)
                    }
                    _ => {
                        Table::Unspecified(TableUnspecified::new_from_reader(reader, offset, size)?)
                    }
                }
            };
            // Tell it to get in the van
            woff_tables.insert(entry.tag, table);
        }

        // Discover XML metadata
        let /*mut*/ woff_meta = Option::None;
        //if woff_hdr.metaOffset > 0 && woff_hdr.metaLength > 0 {
        //

        // If private data is present, store it as a table.
        let /*mut*/ woff_private = Option::None;
        //if woff_hdr.privOffset > 0  && woff_hdr.privLength > 0 {
        //}

        Ok(WoffFont {
            header: woff_hdr,
            directory: woff_dir,
            tables: woff_tables,
            meta: woff_meta,
            private: woff_private,
        })
    }

    /// Writes out this font file.
    fn write<TDest: Write + ?Sized>(&self, destination: &mut TDest) -> Result<()> {
        // First the header.
        self.header.write(destination)?;
        // Then the directory.
        self.directory.write(destination)?;
        // Then the tables, in physical order.
        for entry in self.directory.physical_order().iter() {
            // TBD - current-offset sanity-checking:
            //  1. Did we go backwards (despite the request for physical_order)?
            //  2. Did we go more than 3 bytes forward (file has excess padding)?
            // destination.seek(SeekFrom::Start(entry.offset as u64))?;
            // Note that dest stream is not seekable.
            // Write out the (real and fake) tables.
            match &self.tables[&entry.tag] {
                Table::C2PA(c2pa_table) => c2pa_table.write(destination)?,
                //Table::Head(head_table) => head_table.write(destination)?,
                Table::Unspecified(un_table) => un_table.write(destination)?,
            }
        }
        // Then the XML meta, if present.
        match &self.meta {
            Some(woff_meta) => {
                woff_meta.write(destination)?;
            }
            None => (),
        };
        // Then the private data, if present.
        match &self.private {
            Some(woff_private) => {
                woff_private.write(destination)?;
            }
            None => (),
        };
        // If we made it here, it all worked.
        Ok(())
    }
}

/// TBD: All the serialization structures so far have been defined using native
/// Rust types; should we go all-out in the other direction, and establish a
/// layer of "font" types (FWORD, FIXED, etc.)?

/// SFNT header, from the OpenType spec.
#[derive(Copy, Clone, Debug)]
#[repr(C, packed(4))] // As defined by the OpenType spec.
#[allow(non_snake_case)] // As defined by the OpenType spec.
struct SfntHeader {
    _sfntVersion: u32,
    _numTables: u16,
    _searchRange: u16,
    _entrySelector: u16,
    _rangeShift: u16,
}

/// SFNT Table Directory Entry, from the OpenType spec.
#[derive(Copy, Clone, Debug)]
#[repr(C, packed(4))] // As defined by the OpenType spec.
#[allow(non_snake_case)] // As defined by the OpenType spec.
struct SfntTableDirEntry {
    _tag: TableTag,
    _checksum: u32,
    _offset: u32,
    _length: u32,
}

/// WOFF 1.0 file header, from the WOFF spec.
///
/// TBD: Should this be treated as a "Table", perhaps with a magic tag value that
/// always sorts first, for operational reasons?
#[derive(Copy, Clone, Debug)]
#[repr(C, packed(4))] // As defined by the WOFF spec. (though we don't as yet directly support exotics like FIXED)
#[allow(non_snake_case)] // As named by the WOFF spec.
struct WoffHeader {
    signature: u32,
    flavor: u32,
    length: u32,
    numTables: u16,
    reserved: u16,
    totalSfntSize: u32,
    majorVersion: u16,
    minorVersion: u16,
    metaOffset: u32,
    metaLength: u32,
    metaOrigLength: u32,
    privOffset: u32,
    privLength: u32,
}

impl WoffHeader {
    pub fn new_from_reader<T: Read + Seek + ?Sized>(reader: &mut T) -> Result<Self> {
        Ok(Self {
            signature: reader.read_u32::<BigEndian>()?,
            flavor: reader.read_u32::<BigEndian>()?,
            length: reader.read_u32::<BigEndian>()?,
            numTables: reader.read_u16::<BigEndian>()?,
            reserved: reader.read_u16::<BigEndian>()?,
            totalSfntSize: reader.read_u32::<BigEndian>()?,
            majorVersion: reader.read_u16::<BigEndian>()?,
            minorVersion: reader.read_u16::<BigEndian>()?,
            metaOffset: reader.read_u32::<BigEndian>()?,
            metaLength: reader.read_u32::<BigEndian>()?,
            metaOrigLength: reader.read_u32::<BigEndian>()?,
            privOffset: reader.read_u32::<BigEndian>()?,
            privLength: reader.read_u32::<BigEndian>()?,
        })
    }

    fn write<TDest: Write + ?Sized>(&self, destination: &mut TDest) -> Result<()> {
        destination.write_u32::<BigEndian>(self.signature)?;
        destination.write_u32::<BigEndian>(self.flavor)?;
        destination.write_u32::<BigEndian>(self.length)?;
        destination.write_u16::<BigEndian>(self.numTables)?;
        destination.write_u16::<BigEndian>(self.reserved)?;
        destination.write_u32::<BigEndian>(self.totalSfntSize)?;
        destination.write_u16::<BigEndian>(self.majorVersion)?;
        destination.write_u16::<BigEndian>(self.minorVersion)?;
        destination.write_u32::<BigEndian>(self.metaOffset)?;
        destination.write_u32::<BigEndian>(self.metaLength)?;
        destination.write_u32::<BigEndian>(self.metaOrigLength)?;
        destination.write_u32::<BigEndian>(self.privOffset)?;
        destination.write_u32::<BigEndian>(self.privLength)?;
        Ok(())
    }
}
impl Default for WoffHeader {
    fn default() -> Self {
        Self {
            signature: Magic::Woff as u32,
            flavor: 0,
            length: 0,
            numTables: 0,
            reserved: 0,
            totalSfntSize: 44,
            majorVersion: 1,
            minorVersion: 0,
            metaOffset: 0,
            metaLength: 0,
            metaOrigLength: 0,
            privOffset: 0,
            privLength: 0,
        }
    }
}

/// WOFF 1.0 Table Directory Entry, from the WOFF spec.
#[derive(Copy, Clone, Debug)]
#[repr(C, packed(4))] // As defined by the WOFF spec. (though we don't as yet directly support exotics like FIXED)
#[allow(non_snake_case)] // As named by the WOFF spec.
struct WoffTableDirEntry {
    tag: TableTag,
    offset: u32,
    compLength: u32,
    origLength: u32,
    origChecksum: u32,
}
impl WoffTableDirEntry {
    pub fn new_from_reader<T: Read + Seek + ?Sized>(reader: &mut T) -> Result<Self> {
        Ok(Self {
            tag: TableTag::new_from_reader(reader)?,
            offset: reader.read_u32::<BigEndian>()?,
            compLength: reader.read_u32::<BigEndian>()?,
            origLength: reader.read_u32::<BigEndian>()?,
            origChecksum: reader.read_u32::<BigEndian>()?,
        })
    }

    /// Serialize this directory entry to the given writer.
    fn write<TDest: Write + ?Sized>(&self, destination: &mut TDest) -> Result<()> {
        self.tag.write(destination)?;
        destination.write_u32::<BigEndian>(self.offset)?;
        destination.write_u32::<BigEndian>(self.compLength)?;
        destination.write_u32::<BigEndian>(self.origLength)?;
        destination.write_u32::<BigEndian>(self.origChecksum)?;
        Ok(())
    }
}

/// WOFF 1.0 Directory is just an array of entries. Undoubtedly there exists a
/// more-oxidized way of just using Vec directly for this...
#[derive(Debug)]
struct WoffDirectory {
    entries: Vec<WoffTableDirEntry>,
}
impl WoffDirectory {
    pub fn new() -> Result<Self> {
        Ok(Self {
            entries: Vec::new(),
        })
    }

    pub fn new_from_reader<T: Read + Seek + ?Sized>(
        reader: &mut T,
        entry_count: usize,
    ) -> Result<Self> {
        let mut the_directory = WoffDirectory::new()?;
        for _entry in 0..entry_count {
            the_directory
                .entries
                .push(WoffTableDirEntry::new_from_reader(reader)?);
        }
        Ok(the_directory)
    }

    /// Serialize this directory entry to the given writer.
    fn write<TDest: Write + ?Sized>(&self, destination: &mut TDest) -> Result<()> {
        for entry in self.entries.iter() {
            entry.write(destination)?;
        }
        Ok(())
    }

    /// Returns an array which contains the indices of this directory's entries,
    /// arranged in their physical (offset+size) order.
    fn physical_order(&self) -> Vec<WoffTableDirEntry> {
        let mut physically_ordered_entries = self.entries.clone();
        physically_ordered_entries.sort_by_key(|e| e.offset);
        physically_ordered_entries
    }
}

/// Identifies types of regions within a font file. Chunks with lesser enum
/// values precede those with greater enum values; order within a given group
/// of chunks (such as a series of `Table` chunks) must be preserved by some
/// other mechanism.
#[derive(Debug, Eq, PartialEq)]
pub enum ChunkType {
    /// Whole-container header.
    Header,
    /// Table directory entry or entries.
    Directory,
    /// Table data
    Table,
    /// WOFF metadata
    WoffMeta,
    /// WOFF private data
    WoffPrivate,
}

/// Represents regions within a font file that may be of interest when it
/// comes to hashing data for C2PA.
#[derive(Debug, Eq, PartialEq)]
pub struct ChunkPosition {
    /// Offset to the start of the chunk
    pub offset: u64,
    /// Length of the chunk
    pub length: u32,
    /// Tag of the chunk
    pub name: [u8; 4],
    /// Type of chunk
    pub chunk_type: ChunkType,
}

//impl PartialEq for ChunkPosition {
//    fn eq(&self, other: &Self) -> bool {
//        self.offset == other.offset
//            && self.length == other.length
//            && self.name == other.name
//            && self.chunk_type == other.chunk_type
//    }
//}

/// Custom trait for reading chunks of data from a scalable font (SFNT).
pub trait ChunkReader {
    type Error;
    /// Gets a collection of positions of chunks within the font.
    ///
    /// ## Arguments
    /// * `reader` - Source stream to read data from
    ///
    /// ## Returns
    /// A collection of positions/offsets and length to omit from hashing.
    fn get_chunk_positions<T: Read + Seek + ?Sized>(
        &self,
        reader: &mut T,
    ) -> core::result::Result<Vec<ChunkPosition>, Self::Error>;
}

/// Reads in chunks for a WOFF 1.0 file
impl ChunkReader for WoffIO {
    type Error = crate::error::Error;

    fn get_chunk_positions<T: Read + Seek + ?Sized>(
        &self,
        reader: &mut T,
    ) -> core::result::Result<Vec<ChunkPosition>, Self::Error> {
        reader.rewind()?;
        // WOFFHeader
        let woff_hdr = WoffHeader::new_from_reader(reader)?;
        // Verify the font has a valid version in it before assuming the rest is
        // valid (NOTE: we don't actually do anything with it, just as a safety
        // check).
        //
        // TBD - Push this into WoffHeader::new_from_reader
        let _font_magic: Magic =
            <u32 as std::convert::TryInto<Magic>>::try_into(woff_hdr.signature)
                .map_err(|_err| Error::UnsupportedFontError)?;
        // Add the position of the header.
        let mut positions: Vec<ChunkPosition> = Vec::new();
        positions.push(ChunkPosition {
            offset: 0,
            length: size_of::<WoffHeader>() as u32,
            name: WOFF_HEADER_CHUNK_NAME.data,
            chunk_type: ChunkType::Header,
        });

        // Advance to the start of the table entries
        reader.seek(SeekFrom::Start(size_of::<WoffHeader>() as u64))?;

        // Read in the directory, and add its chunk
        let woff_dir = WoffDirectory::new_from_reader(reader, woff_hdr.numTables as usize)?;
        positions.push(ChunkPosition {
            offset: reader.stream_position()?,
            length: woff_hdr.numTables as u32 * size_of::<WoffTableDirEntry>() as u32,
            name: WOFF_DIRECTORY_CHUNK_NAME.data,
            chunk_type: ChunkType::Directory,
        });

        // Add a chunk for each table, in directory order
        for entry in woff_dir.physical_order().iter() {
            positions.push(ChunkPosition {
                offset: entry.offset as u64,
                length: entry.origLength, // TBD compression support
                name: entry.tag.data,
                chunk_type: ChunkType::Table,
            });
        }

        // Check for XML metadata trailer
        if woff_hdr.metaLength > 0 {
            positions.push(ChunkPosition {
                offset: woff_hdr.metaOffset as u64,
                length: woff_hdr.metaLength,
                name: WOFF_METADATA_CHUNK_NAME.data,
                chunk_type: ChunkType::WoffMeta,
            });
        }

        // Check for private data trailer
        if woff_hdr.privLength > 0 {
            positions.push(ChunkPosition {
                offset: woff_hdr.privOffset as u64,
                length: woff_hdr.privLength,
                name: WOFF_PRIVATE_CHUNK_NAME.data,
                chunk_type: ChunkType::WoffPrivate,
            });
        }

        // Do not iterate if the log level is not set to at least trace
        if log::max_level().cmp(&log::LevelFilter::Trace).is_ge() {
            for position in positions.iter().as_ref() {
                trace!("Position for C2PA in font: {:?}", &position);
            }
        }

        Ok(positions)
    }
}

/// Adds C2PA manifest store data to a font file
///
/// ## Arguments
///
/// * `font_path` - Path to a font file
/// * `manifest_store_data` - C2PA manifest store data to add to the font file
fn add_c2pa_to_font(font_path: &Path, manifest_store_data: &[u8]) -> Result<()> {
    process_file_with_streams(font_path, move |input_stream, temp_file| {
        // Add the C2PA data to the temp file
        add_c2pa_to_stream(input_stream, temp_file.get_mut_file(), manifest_store_data)
    })
}

/// Adds C2PA manifest store data to a font stream
///
/// ## Arguments
///
/// * `source` - Source stream to read initial data from
/// * `destination` - Destination stream to write C2PA manifest store data
/// * `manifest_store_data` - C2PA manifest store data to add to the font stream
fn add_c2pa_to_stream<TSource, TDest>(
    source: &mut TSource,
    destination: &mut TDest,
    manifest_store_data: &[u8],
) -> Result<()>
where
    TSource: Read + Seek + ?Sized,
    TDest: Write + ?Sized,
{
    source.rewind()?;
    let mut font = WoffFont::new_from_reader(source).map_err(|_| Error::FontLoadError)?;
    // Install the provide active_manifest_uri in this font's C2PA table, adding
    // that table if needed.
    match font.tables.get_mut(&C2PA_TABLE_TAG) {
        // If there isn't one, create it.
        None => {
            font.tables.insert(
                C2PA_TABLE_TAG,
                Table::C2PA(TableC2PA::new(None, Some(manifest_store_data.to_vec()))),
            );
        }
        // If there is, replace its `active_manifest_uri` value with the
        // provided one.
        Some(ostensible_c2pa_table) => {
            match ostensible_c2pa_table {
                Table::C2PA(c2pa_table) => {
                    c2pa_table.manifest_store = Some(manifest_store_data.to_vec());
                }
                _ => {
                    todo!("A non-C2PA table was found with the C2PA tag. We should report this as if it were an error, which it most certainly is.");
                }
            };
        }
    };
    font.write(destination).map_err(|_| Error::FontSaveError)?;
    Ok(())
}

/// Adds the manifest URI reference to the font at the given path.
///
/// ## Arguments
///
/// * `font_path` - Path to a font file
/// * `manifest_uri` - Reference URI to a manifest store
#[allow(dead_code)]
fn add_reference_to_font(font_path: &Path, manifest_uri: &str) -> Result<()> {
    process_file_with_streams(font_path, move |input_stream, temp_file| {
        // Write the manifest URI to the stream
        add_reference_to_stream(input_stream, temp_file.get_mut_file(), manifest_uri)
    })
}

/// Adds the specified reference to the font.
///
/// ## Arguments
///
/// * `source` - Source stream to read initial data from
/// * `destination` - Destination stream to write data with new reference
/// * `manifest_uri` - Reference URI to a manifest store
fn add_reference_to_stream<TSource, TDest>(
    source: &mut TSource,
    destination: &mut TDest,
    manifest_uri: &str,
) -> Result<()>
where
    TSource: Read + Seek + ?Sized,
    TDest: Write + ?Sized,
{
    source.rewind()?;
    let mut font = WoffFont::new_from_reader(source).map_err(|_| Error::FontLoadError)?;
    // Install the provide active_manifest_uri in this font's C2PA table, adding
    // that table if needed.
    match font.tables.get_mut(&C2PA_TABLE_TAG) {
        // If there isn't one, create it.
        None => {
            font.tables.insert(
                C2PA_TABLE_TAG,
                Table::C2PA(TableC2PA::new(Some(manifest_uri.to_string()), None)),
            );
        }
        // If there is, replace its `active_manifest_uri` value with the
        // provided one.
        Some(ostensible_c2pa_table) => {
            match ostensible_c2pa_table {
                Table::C2PA(c2pa_table) => {
                    c2pa_table.active_manifest_uri = Some(manifest_uri.to_string());
                }
                _ => {
                    todo!("A non-C2PA table was found with the C2PA tag. We should report this as if it were an error, which it most certainly is.");
                }
            };
        }
    };
    font.write(destination).map_err(|_| Error::FontSaveError)?;
    Ok(())
}

/// Adds the required chunks to the stream for supporting C2PA, if the chunks are
/// already present nothing is done.
///
/// ### Parameters
/// - `input_stream` - Source stream to read initial data from
/// - `output_stream` - Destination stream to write data with the added required
///                     chunks
///
/// ### Remarks
/// Neither streams are rewound before and/or after the operation, so it is up
/// to the caller.
///
/// ### Returns
/// A Result indicating success or failure
fn add_required_chunks_to_stream<TReader, TWriter>(
    input_stream: &mut TReader,
    output_stream: &mut TWriter,
) -> Result<()>
where
    TReader: Read + Seek + ?Sized,
    TWriter: Read + Seek + ?Sized + Write,
{
    // Read the font from the input stream
    let mut font = WoffFont::new_from_reader(input_stream).map_err(|_| Error::FontLoadError)?;
    // If the C2PA table does not exist, then we will add an empty one.
    if font.tables.get(&C2PA_TABLE_TAG).is_none() {
        // This table will succeed it.
        let c2pa_table = TableC2PA::new(None, None);
        let c2pa_entry = match font.directory.physical_order().last() {
            Some(last_phys_entry) => WoffTableDirEntry {
                tag: C2PA_TABLE_TAG,
                offset: (last_phys_entry.offset + last_phys_entry.compLength + 3) % 4,
                compLength: size_of::<TableC2PARaw>() as u32,
                origLength: size_of::<TableC2PARaw>() as u32,
                origChecksum: c2pa_table.checksum()?,
            },
            None => WoffTableDirEntry {
                tag: C2PA_TABLE_TAG,
                offset: (size_of::<WoffHeader>()
                    + font.header.numTables as usize * size_of::<WoffTableDirEntry>())
                    as u32,
                compLength: size_of::<TableC2PARaw>() as u32,
                origLength: size_of::<TableC2PARaw>() as u32,
                origChecksum: c2pa_table.checksum()?,
            },
        };
        // Store the new directory entry & table.
        font.directory.entries.push(c2pa_entry);
        font.tables.insert(C2PA_TABLE_TAG, Table::C2PA(c2pa_table));
        // Count the table, grow the total size, grow, the "SFNT size"
        font.header.numTables += 1;
        // (TBD compression - conflating comp/uncomp sizes here.)
        font.header.length += size_of::<WoffTableDirEntry>() as u32 + c2pa_entry.compLength;
        let sfnt_hdr = size_of::<SfntHeader>();
        let sfnt_dir = size_of::<SfntTableDirEntry>();
        println!("{}, {}", sfnt_hdr, sfnt_dir);
        font.header.totalSfntSize += size_of::<SfntTableDirEntry>() as u32 + c2pa_entry.compLength;
        // Bump the XML meta and private data blocks ahead if needed.
        if font.header.metaOffset > 0 {
            font.header.metaOffset += c2pa_entry.compLength;
        }
        if font.header.privOffset > 0 {
            font.header.privOffset += c2pa_entry.compLength;
        }
    }
    // Write the font to the output stream
    font.write(output_stream)
        .map_err(|_| Error::FontSaveError)?;
    Ok(())
}

/// Opens a BufReader for the given file path
///
/// ## Arguments
///
/// * `file_path` - Valid path to a file to open in a buffer reader
///
/// ## Returns
///
/// A BufReader<File> object
fn open_bufreader_for_file(file_path: &Path) -> Result<BufReader<File>> {
    let file = File::open(file_path)?;
    Ok(BufReader::new(file))
}

/// Processes a font file using a streams to process.
///
/// ## Arguments
///
/// * `font_path` - Path to the font file to process
/// * `callback` - Method to process the stream
fn process_file_with_streams(
    font_path: &Path,
    callback: impl Fn(&mut BufReader<File>, &mut TempFile) -> Result<()>,
) -> Result<()> {
    // Open the font source for a buffer read
    let mut font_buffer = open_bufreader_for_file(font_path)?;
    // Open a temporary file, which will be deleted after destroyed
    let mut temp_file = TempFile::new(font_path)?;
    callback(&mut font_buffer, &mut temp_file)?;
    // Finally copy the temporary file, replacing the original file
    std::fs::copy(temp_file.get_path(), font_path)?;
    Ok(())
}

/// Reads the C2PA manifest store reference from the font file.
///
/// ## Arguments
///
/// * `font_path` - File path to the font file to read reference from.
///
/// ## Returns
/// If a reference is available, it will be returned.
#[allow(dead_code)]
fn read_reference_from_font(font_path: &Path) -> Result<Option<String>> {
    // open the font source
    let mut font_stream = open_bufreader_for_file(font_path)?;
    read_reference_from_stream(&mut font_stream)
}

/// Reads the C2PA manifest store reference from the stream.
///
/// ## Arguments
///
/// * `source` - Source font stream to read reference from.
///
/// ## Returns
/// If a reference is available, it will be returned.
#[allow(dead_code)]
fn read_reference_from_stream<TSource>(source: &mut TSource) -> Result<Option<String>>
where
    TSource: Read + Seek + ?Sized,
{
    match read_c2pa_from_stream(source) {
        Ok(c2pa_data) => Ok(c2pa_data.active_manifest_uri.to_owned()),
        Err(Error::JumbfNotFound) => Ok(None),
        Err(_) => Err(Error::DeserializationError),
    }
}

/// Remove the `C2PA` font table from the font file.
///
/// ## Arguments
///
/// * `font_path` - path to the font file to remove C2PA from
fn remove_c2pa_from_font(font_path: &Path) -> Result<()> {
    process_file_with_streams(font_path, move |input_stream, temp_file| {
        // Remove the C2PA manifest store from the stream
        remove_c2pa_from_stream(input_stream, temp_file.get_mut_file())
    })
}

/// Remove the `C2PA` font table from the font data stream, writing to the
/// destination.
///
/// ## Arguments
///
/// * `source` - Source data stream containing font data
/// * `destination` - Destination data stream to write new font data with the
///                   C2PA table removed
fn remove_c2pa_from_stream<TSource, TDest>(
    source: &mut TSource,
    destination: &mut TDest,
) -> Result<()>
where
    TSource: Read + Seek + ?Sized,
    TDest: Write + ?Sized,
{
    source.rewind()?;
    // Load the font from the stream
    let mut font = WoffFont::new_from_reader(source).map_err(|_| Error::FontLoadError)?;
    // Remove the table from the collection
    font.tables.remove(&C2PA_TABLE_TAG);
    // And write it to the destination stream
    font.write(destination).map_err(|_| Error::FontSaveError)?;

    Ok(())
}

/// Removes the reference to the active manifest from the source stream, writing
/// to the destination.
///
/// ## Arguments
///
/// * `source` - Source data stream containing font data
/// * `destination` - Destination data stream to write new font data with the
///                   active manifest reference removed
///
/// ## Returns
///
/// The active manifest URI reference that was removed, if there was one
#[allow(dead_code)]
fn remove_reference_from_stream<TSource, TDest>(
    source: &mut TSource,
    destination: &mut TDest,
) -> Result<Option<String>>
where
    TSource: Read + Seek + ?Sized,
    TDest: Write + ?Sized,
{
    source.rewind()?;
    let mut font = WoffFont::new_from_reader(source).map_err(|_| Error::FontLoadError)?;
    let old_manifest_uri_maybe = match font.tables.get_mut(&C2PA_TABLE_TAG) {
        // If there isn't one, how pleasant, there will be so much less to do.
        None => None,
        // If there is, and it has Some `active_manifest_uri`, then mutate that
        // to None, and return the former value.
        Some(ostensible_c2pa_table) => {
            match ostensible_c2pa_table {
                Table::C2PA(c2pa_table) => {
                    if c2pa_table.active_manifest_uri.is_none() {
                        None
                    } else {
                        // TBD this cannot really be the idiomatic way, can it?
                        let old_manifest_uri = c2pa_table.active_manifest_uri.clone();
                        c2pa_table.active_manifest_uri = None;
                        old_manifest_uri
                    }
                }
                _ => {
                    todo!("A non-C2PA table was found with the C2PA tag. We should report this as if it were an error, which it most certainly is.");
                }
            }
        }
    };
    font.write(destination).map_err(|_| Error::FontSaveError)?;
    Ok(old_manifest_uri_maybe)
}

/// Gets a collection of positions of hash objects, which are to be excluded from the hashing.
///
/// ## Arguments
///
/// * `reader` - Reader object used to read object locations from
///
/// ## Returns
///
/// A collection of positions/offsets and length to omit from hashing.
fn get_object_locations_from_stream<T>(
    woff_io: &WoffIO,
    reader: &mut T,
) -> Result<Vec<HashObjectPositions>>
where
    T: Read + Seek + ?Sized,
{
    // The SDK doesn't necessarily promise the input stream is rewound, so do so
    // now to make sure we can parse the font.
    reader.rewind()?;
    // We must take into account a font that may not have a C2PA table in it at
    // this point, adding any required chunks needed for C2PA to work correctly.
    let output_vec: Vec<u8> = Vec::new();
    let mut output_stream = Cursor::new(output_vec);
    add_required_chunks_to_stream(reader, &mut output_stream)?;
    output_stream.rewind()?;

    // Build up the positions we will hand back to the caller
    let mut positions: Vec<HashObjectPositions> = Vec::new();

    // Which will be built up from the different chunks from the file
    let chunk_positions = woff_io.get_chunk_positions(&mut output_stream)?;
    for chunk_position in chunk_positions {
        let mut position_objs = match chunk_position.chunk_type {
            // The table directory, other than the table records array will be
            // added as "other"
            ChunkType::Header => vec![HashObjectPositions {
                offset: chunk_position.offset as usize,
                length: chunk_position.length as usize,
                htype: HashBlockObjectType::Other,
            }],
            // For the table record entries, we will specialize the C2PA table
            // record and all others will be added as is
            ChunkType::Directory => {
                if chunk_position.name == C2PA_TABLE_TAG.data {
                    vec![HashObjectPositions {
                        offset: chunk_position.offset as usize,
                        length: chunk_position.length as usize,
                        htype: HashBlockObjectType::Cai,
                    }]
                } else {
                    vec![HashObjectPositions {
                        offset: chunk_position.offset as usize,
                        length: chunk_position.length as usize,
                        htype: HashBlockObjectType::Other,
                    }]
                }
            }
            // Similarly for the actual table data, we need to specialize C2PA
            // and in this case the `head` table as well, to ignore the checksum
            // adjustment.
            //
            // ("Other" data gets treated the same as an uninteresting table.)
            ChunkType::Table | ChunkType::WoffMeta | ChunkType::WoffPrivate => {
                let mut table_positions = Vec::<HashObjectPositions>::new();
                // We must split out the head table to ignore the checksum
                // adjustment, because it changes after the C2PA table is
                // written to the font
                if chunk_position.name == HEAD_TABLE_TAG.data {
                    let head_offset = &chunk_position.offset;
                    let head_length = &chunk_position.length;
                    // Include the major/minor/revision version numbers
                    table_positions.push(HashObjectPositions {
                        offset: *head_offset as usize,
                        length: 8,
                        htype: HashBlockObjectType::Other,
                    });
                    // Indicate the checksumAdjustment value as CAI
                    table_positions.push(HashObjectPositions {
                        offset: (head_offset + 8) as usize,
                        length: 4,
                        htype: HashBlockObjectType::Cai,
                    });
                    // And the remainder of the table as other
                    table_positions.push(HashObjectPositions {
                        offset: (head_offset + 12) as usize,
                        length: (head_length - 12) as usize,
                        htype: HashBlockObjectType::Other,
                    });
                } else if chunk_position.name == C2PA_TABLE_TAG.data {
                    table_positions.push(HashObjectPositions {
                        offset: chunk_position.offset as usize,
                        length: chunk_position.length as usize,
                        htype: HashBlockObjectType::Cai,
                    });
                } else {
                    table_positions.push(HashObjectPositions {
                        offset: chunk_position.offset as usize,
                        length: chunk_position.length as usize,
                        htype: HashBlockObjectType::Other,
                    });
                }
                table_positions
            }
        };
        positions.append(&mut position_objs);
    }
    Ok(positions)
}

/// Reads the `C2PA` font table from the data stream
///
/// ## Arguments
///
/// * `reader` - data stream reader to read font data from
///
/// ## Returns
///
/// A result containing the `C2PA` font table data
fn read_c2pa_from_stream<T: Read + Seek + ?Sized>(reader: &mut T) -> Result<TableC2PA> {
    let woff = WoffFont::new_from_reader(reader).map_err(|_| Error::FontLoadError)?;
    let c2pa_table: Option<TableC2PA> = match woff.tables.get(&C2PA_TABLE_TAG) {
        None => None,
        Some(ostensible_c2pa_table) => match ostensible_c2pa_table {
            Table::C2PA(bonafied_c2pa_table) => Some(bonafied_c2pa_table.clone()),
            _ => {
                todo!("A non-C2PA table was found with the C2PA tag. We should report this as if it were an error, which it most certainly is.");
            }
        },
    };
    c2pa_table.ok_or(Error::JumbfNotFound)
}

/// Main WOFF IO feature.
pub struct WoffIO {}

impl WoffIO {
    #[allow(dead_code)]
    pub fn default_document_id() -> String {
        format!("fontsoftware:did:{}", Uuid::new_v4())
    }

    #[allow(dead_code)]
    pub fn default_instance_id() -> String {
        format!("fontsoftware:iid:{}", Uuid::new_v4())
    }
}

/// WOFF implementation of the CAILoader trait.
impl CAIReader for WoffIO {
    fn read_cai(&self, asset_reader: &mut dyn CAIRead) -> Result<Vec<u8>> {
        let c2pa_table = read_c2pa_from_stream(asset_reader)?;
        match c2pa_table.get_manifest_store() {
            Some(manifest_store) => Ok(manifest_store.to_vec()),
            _ => Err(Error::JumbfNotFound),
        }
    }

    fn read_xmp(&self, asset_reader: &mut dyn CAIRead) -> Option<String> {
        // Fonts have no XMP data.
        // BUT, for now we will pretend it does and read from the reference
        match read_reference_from_stream(asset_reader) {
            Ok(reference) => reference,
            Err(_) => None,
        }
    }
}

/// WOFF/TTF implementations for the CAIWriter trait.
impl CAIWriter for WoffIO {
    fn write_cai(
        &self,
        input_stream: &mut dyn CAIRead,
        output_stream: &mut dyn CAIReadWrite,
        store_bytes: &[u8],
    ) -> Result<()> {
        add_c2pa_to_stream(input_stream, output_stream, store_bytes)
    }

    fn get_object_locations_from_stream(
        &self,
        input_stream: &mut dyn CAIRead,
    ) -> Result<Vec<HashObjectPositions>> {
        get_object_locations_from_stream(self, input_stream)
    }

    fn remove_cai_store_from_stream(
        &self,
        input_stream: &mut dyn CAIRead,
        output_stream: &mut dyn CAIReadWrite,
    ) -> Result<()> {
        remove_c2pa_from_stream(input_stream, output_stream)
    }
}

/// WOFF/TTF implementations for the AssetIO trait.
impl AssetIO for WoffIO {
    fn new(_asset_type: &str) -> Self
    where
        Self: Sized,
    {
        WoffIO {}
    }

    fn get_handler(&self, asset_type: &str) -> Box<dyn AssetIO> {
        Box::new(WoffIO::new(asset_type))
    }

    fn get_reader(&self) -> &dyn CAIReader {
        self
    }

    fn get_writer(&self, asset_type: &str) -> Option<Box<dyn CAIWriter>> {
        Some(Box::new(WoffIO::new(asset_type)))
    }

    fn remote_ref_writer_ref(&self) -> Option<&dyn RemoteRefEmbed> {
        Some(self)
    }

    fn supported_types(&self) -> &[&str] {
        &SUPPORTED_TYPES
    }

    fn read_cai_store(&self, asset_path: &Path) -> Result<Vec<u8>> {
        let mut f: File = File::open(asset_path)?;
        self.read_cai(&mut f)
    }

    fn save_cai_store(&self, asset_path: &Path, store_bytes: &[u8]) -> Result<()> {
        add_c2pa_to_font(asset_path, store_bytes)
    }

    fn get_object_locations(&self, asset_path: &Path) -> Result<Vec<HashObjectPositions>> {
        let mut buf_reader = open_bufreader_for_file(asset_path)?;
        get_object_locations_from_stream(self, &mut buf_reader)
    }

    fn remove_cai_store(&self, asset_path: &Path) -> Result<()> {
        remove_c2pa_from_font(asset_path)
    }

    fn asset_box_hash_ref(&self) -> Option<&dyn AssetBoxHash> {
        Some(self)
    }
}

// Implementation for the asset box hash trait for general box hash support
impl AssetBoxHash for WoffIO {
    fn get_box_map(&self, input_stream: &mut dyn CAIRead) -> Result<Vec<BoxMap>> {
        // Get the chunk positions
        let positions = self.get_chunk_positions(input_stream)?;
        // Create a box map vector to map the chunk positions to
        let mut box_maps = Vec::<BoxMap>::new();
        for position in positions {
            let box_map = BoxMap {
                names: vec![format!("{:?}", position.chunk_type)],
                alg: None,
                hash: ByteBuf::from(Vec::new()),
                pad: ByteBuf::from(Vec::new()),
                range_start: position.offset as usize,
                range_len: position.length as usize,
            };
            box_maps.push(box_map);
        }
        Ok(box_maps)
    }
}

impl RemoteRefEmbed for WoffIO {
    #[allow(unused_variables)]
    fn embed_reference(
        &self,
        asset_path: &Path,
        embed_ref: crate::asset_io::RemoteRefEmbedType,
    ) -> Result<()> {
        match embed_ref {
            crate::asset_io::RemoteRefEmbedType::Xmp(manifest_uri) => {
                #[cfg(feature = "xmp_write")]
                {
                    font_xmp_support::add_reference_as_xmp_to_font(asset_path, &manifest_uri)
                }
                #[cfg(not(feature = "xmp_write"))]
                {
                    add_reference_to_font(asset_path, &manifest_uri)
                }
            }
            crate::asset_io::RemoteRefEmbedType::StegoS(_) => Err(Error::UnsupportedType),
            crate::asset_io::RemoteRefEmbedType::StegoB(_) => Err(Error::UnsupportedType),
            crate::asset_io::RemoteRefEmbedType::Watermark(_) => Err(Error::UnsupportedType),
        }
    }

    fn embed_reference_to_stream(
        &self,
        reader: &mut dyn CAIRead,
        output_stream: &mut dyn CAIReadWrite,
        embed_ref: RemoteRefEmbedType,
    ) -> Result<()> {
        match embed_ref {
            crate::asset_io::RemoteRefEmbedType::Xmp(manifest_uri) => {
                #[cfg(feature = "xmp_write")]
                {
                    font_xmp_support::add_reference_as_xmp_to_stream(
                        reader,
                        output_stream,
                        &manifest_uri,
                    )
                }
                #[cfg(not(feature = "xmp_write"))]
                {
                    add_reference_to_stream(reader, output_stream, &manifest_uri)
                }
            }
            crate::asset_io::RemoteRefEmbedType::StegoS(_) => Err(Error::UnsupportedType),
            crate::asset_io::RemoteRefEmbedType::StegoB(_) => Err(Error::UnsupportedType),
            crate::asset_io::RemoteRefEmbedType::Watermark(_) => Err(Error::UnsupportedType),
        }
    }
}

#[cfg(test)]
pub mod tests {
    #![allow(clippy::expect_used)]
    #![allow(clippy::panic)]
    #![allow(clippy::unwrap_used)]

    use std::io::Cursor;

    use claims::*;

    use super::*;

    #[test]
    // Key to test comments:
    //
    //   IIP - Invalid/Ignored/Passthrough
    //         This field's value is bogus, possibly illegal, but it is expected
    //         that this code will neither detect nor modify it.
    fn add_required_chunks_to_stream_minimal() {
        let min_font_data = vec![
            0x77, 0x4f, 0x46, 0x46, // wOFF
            0x72, 0x73, 0x74, 0x75, // flavor (IIP)
            0x00, 0x00, 0x00, 0x2c, // length (44)
            0x00, 0x00, 0x00, 0x00, // numTables (0) / reserved (0)
            0x00, 0x00, 0x00, 0x0c, // totalSfntSize (12 for header only)
            0x82, 0x83, 0x84, 0x85, // majorVersion / minorVersion (IIP)
            0x00, 0x00, 0x00, 0x00, // metaOffset (0)
            0x00, 0x00, 0x00, 0x00, // metaLength (0)
            0x00, 0x00, 0x00, 0x00, // metaOrigLength (0)
            0x00, 0x00, 0x00, 0x00, // privOffset (0)
            0x00, 0x00, 0x00, 0x00, // privLength (0)
        ];
        let expected_min_font_data_min_c2pa = vec![
            // WOFFHeader
            0x77, 0x4f, 0x46, 0x46, // wOFF
            0x72, 0x73, 0x74, 0x75, // flavor (IIP)
            0x00, 0x00, 0x00, 0x54, // length (84)
            0x00, 0x01, 0x00, 0x00, // numTables (1) / reserved (0)
            0x00, 0x00, 0x00, 0x30, // totalSfntSize (48 = 12 + 16 + 20)
            0x82, 0x83, 0x84, 0x85, // majorVersion / minorVersion (IIP)
            0x00, 0x00, 0x00, 0x00, // metaOffset (0)
            0x00, 0x00, 0x00, 0x00, // metaLength (0)
            0x00, 0x00, 0x00, 0x00, // metaOrigLength (0)
            0x00, 0x00, 0x00, 0x00, // privOffset (0)
            0x00, 0x00, 0x00, 0x00, // privLength (0)
            // WOFFTableDirectory
            0x43, 0x32, 0x50, 0x41, // C2PA
            0x00, 0x00, 0x00, 0x40, //   offset (64)
            0x00, 0x00, 0x00, 0x14, //   compLength (20)
            0x00, 0x00, 0x00, 0x14, //   origLength (20)
            0x00, 0x01, 0x00, 0x00, //   origChecksum (0x00010000)
            // C2PA Table
            0x00, 0x01, 0x00, 0x00, // Major / Minor versions
            0x00, 0x00, 0x00, 0x00, // Manifest URI offset (0)
            0x00, 0x00, 0x00, 0x00, // Manifest URI length (0) / reserved (0)
            0x00, 0x00, 0x00, 0x00, // C2PA manifest store offset (0)
            0x00, 0x00, 0x00, 0x00, // C2PA manifest store length (0)
        ];
        let mut the_reader: Cursor<&[u8]> = Cursor::<&[u8]>::new(&min_font_data);
        let mut the_writer = Cursor::new(Vec::new());

        assert_ok!(add_required_chunks_to_stream(
            &mut the_reader,
            &mut the_writer
        ));

        // Write into the "file" and seek to the beginning
        assert_eq!(
            expected_min_font_data_min_c2pa,
            the_writer.get_ref().as_slice()
        );
    }

    #[test]
    fn get_chunk_positions_minimal() {
        let font_data = vec![
            // WOFFHeader
            0x77, 0x4f, 0x46, 0x46, // wOFF
            0x72, 0x73, 0x74, 0x75, // flavor (IIP)
            0x00, 0x00, 0x00, 0x2c, // length (44)
            0x00, 0x00, 0x00, 0x00, // numTables (0) / reserved (0)
            0x00, 0x00, 0x00, 0x0c, // totalSfntSize (12 for header only)
            0x00, 0x00, 0x00, 0x00, // majorVersion / minorVersion
            0x00, 0x00, 0x00, 0x00, // metaOffset (0)
            0x00, 0x00, 0x00, 0x00, // metaLength (0)
            0x00, 0x00, 0x00, 0x00, // metaOrigLength (0)
            0x00, 0x00, 0x00, 0x00, // privOffset (0)
            0x00, 0x00, 0x00, 0x00, // privLength (0)
        ];
        let mut font_stream: Cursor<&[u8]> = Cursor::<&[u8]>::new(&font_data);
        let woff_io = WoffIO {};
        let positions = woff_io.get_chunk_positions(&mut font_stream).unwrap();
        // Should have one position reported for the table directory itself
        assert_eq!(2, positions.len());
        let header_posn = positions.get(0).unwrap();
        assert_eq!(
            *header_posn,
            ChunkPosition {
                offset: 0,
                length: 44,
                name: WOFF_HEADER_CHUNK_NAME.data,
                chunk_type: ChunkType::Header,
            }
        );
        let directory_posn = positions.get(1).unwrap();
        assert_eq!(
            *directory_posn,
            ChunkPosition {
                offset: 44,
                length: 0,
                name: WOFF_DIRECTORY_CHUNK_NAME.data,
                chunk_type: ChunkType::Directory,
            }
        );
    }
}
