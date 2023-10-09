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
    collections::{
        HashMap,
    },
    convert::TryFrom,
    fs::File,
    io::{BufReader, Cursor, Read, Seek, SeekFrom, Write},
    mem::{size_of},
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

/// Debug hook for watching Result<T, Error> values fly past.
/// 
/// Debug the code
///    let my_thing = mysteriously_failing_function(args);
/// by writing
///    let my_thing = debug_cough(mysteriously_failing_function(args));
/// 
#[allow(dead_code)]
fn debug_cough<T>(result: Result<T>) -> Result<T> {
    match result {
        Err(ref e) => {
            trace!("cough: {:?}", e);
        },
        Ok(_) => {
            trace!("cough: Ok");
        }
    }
    result
}

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

// generic font stuff - true for SFNT, WOFF/2, EOT, ...
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
struct TableTag {
    data: [u8; 4],
}

impl TableTag {
    fn as_bytes(&self) -> &[u8] {
        &self.data
    }
    pub fn new<T: Read + Seek + ?Sized>(source_stream: &mut T) -> Result<Self> {
        Ok(Self {
            data: [source_stream.read_u8()?, source_stream.read_u8()?,  // Ick, YHGTBKM...
                   source_stream.read_u8()?, source_stream.read_u8()?],
        })
    }
}

/// Ad-hoc "tag" names for font-file entities which are not really tables,
/// but which it is frequently convenient to treat as if they were. We re-use
/// the font-file magic strings, because they are extremely unlikely to appear
/// as table tags.
const WOFF_HEADER_TAG: TableTag = TableTag { data: *b"wOFF", };
const WOFF_METADATA_TAG: TableTag = TableTag { data: *b"wXML", };
const WOFF_PRIVATE_DATA_TAG: TableTag = TableTag { data: *b"wPRV", };
/// Pseudo-tag for the table directory.
const TABLE_DIRECTORY_TAG: TableTag = TableTag { data: *b"\t\t\t\t", };
/// Tag for the 'C2PA' table.
const C2PA_TABLE_TAG: TableTag = TableTag { data: *b"C2PA", };
/// Tag for the 'head' table in a font.
const HEAD_TABLE_TAG: TableTag = TableTag { data: *b"head", };

/// 32-bit font-format identifier.
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

/// Used to attempt conversion from u32 to a Magic value.
impl TryFrom<u32> for Magic {
    type Error = ();

    /// Tries to convert from u32 to a valid font version.
    fn try_from(v: u32) -> core::result::Result<Self, Self::Error> {
        match v {
            x if x == Magic::TrueType as u32 => Ok(Magic::TrueType),
            x if x == Magic::OpenType as u32 => Ok(Magic::OpenType),
            x if x == Magic::Woff as u32 => Ok(Magic::Woff),
            x if x == Magic::Woff2 as u32 => Ok(Magic::Woff2),
            _ => Err(()),
        }
    }
}

/// 'C2PA' font table - in storage
#[derive(Debug)]
#[repr(C, packed)]       // As defined by the C2PA spec.
#[allow(non_snake_case)] // As named by the C2PA spec.
struct TableC2PARaw {
    majorVersion: u16,
    minorVersion: u16,
    activeManifestUriOffset: u32,
    activeManifestUriLength: u16,
    manifestStoreOffset: u32,
    manifestStoreLength: u32,
}
impl TableC2PARaw {
    fn write<TDest: Write + ?Sized>(&mut self, destination: &mut TDest) -> Result<()> {
        destination.write_u16::<BigEndian>(self.majorVersion)?;
        destination.write_u16::<BigEndian>(self.minorVersion)?;
        destination.write_u32::<BigEndian>(self.activeManifestUriOffset)?;
        destination.write_u16::<BigEndian>(self.activeManifestUriLength as u16)?;
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
    /// Creates a new C2PA record with the current default version information.
    pub fn new(active_manifest_uri: Option<String>, manifest_store: Option<Vec<u8>>) -> Self {
        Self {
            active_manifest_uri: active_manifest_uri,
            manifest_store: manifest_store,
            ..TableC2PA::default()
        }
    }

    /// TBD - Construct TableC2PA from a source_stream and T
    pub fn _read<T: Read + Seek + ?Sized>(_source_stream: &mut T) -> core::result::Result<Font, Error> {
        Err(Error::FontLoadError)?
    }

    /// Get the manifest store data if available
    pub fn get_manifest_store(&self) -> Option<&[u8]> {
        self.manifest_store.as_deref()
    }

    fn write<TDest: Write + ?Sized>(&mut self, destination: &mut TDest) -> Result<()> {
        // Set up the structured data
        let mut raw_table = TableC2PARaw {
            majorVersion: self.major_version,
            minorVersion: self.minor_version,
            activeManifestUriOffset: 0,
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
            raw_table.manifestStoreOffset = size_of::<TableC2PARaw>() as u32 + raw_table.activeManifestUriOffset + raw_table.activeManifestUriLength as u32;
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
#[repr(C, packed)]       // As defined by Open Font Format / OpenType (though we don't as yet directly support exotics like FIXED).
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
    /// TBD - Construct TableC2PA from a source_stream and T
    pub fn _read<T: Read + Seek + ?Sized>(_source_stream: &mut T) -> core::result::Result<Font, Error> {
        Err(Error::FontLoadError)?
    }
}

/// Generic font table with unknown contents.
#[derive(Debug)]
struct TableUnspecified {
    data: Vec<u8>,
}

impl TableUnspecified {
    /// TBD - Construct TableC2PA from a source_stream and T
    pub fn _read<T: Read + Seek + ?Sized>(_source_stream: &mut T) -> core::result::Result<Font, Error> {
        Err(Error::FontLoadError)?
    }
    fn write<TDest: Write + ?Sized>(&mut self, destination: &mut TDest) -> Result<()> {
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
    /// WOFFHeader - not really a table
    WoffHeader(WoffHeader),
}

/// Font abstraction sufficient for SFNT (TrueType/OpenType), WOFF (1 or 2) and
/// EOT fonts. Potentially composable to support TrueType/OpenType Collections.
struct Font {
    /// Magic number for this font's container format
    magic: Magic, //
    /// All the Tables in this font, keyed by TableTag
    tables: HashMap<TableTag, Table>,
}

impl Font {
    /// New blank font
    pub fn new(magic: Magic) -> Self {
        Self {
            magic: magic,
            tables: HashMap::new(),
        }
    }

    /// Reads in a font file.
    fn read<T: Read + Seek + ?Sized>(source_stream: &mut T) -> core::result::Result<Font, Error> {
        // Must always rewind input
        source_stream.rewind()?;
        // Verify the font has a valid version in it before assuming the rest is
        // valid (NOTE: we don't actually do anything with it, just as a safety check).
        let font_magic_u32: u32 = source_stream.read_u32::<BigEndian>()?;
        let font_magic: Magic = <u32 as std::convert::TryInto<Magic>>::try_into(font_magic_u32)
            .map_err(|_err| Error::UnsupportedFontError)?;
        // Check the magic number
        match font_magic {
            Magic::OpenType|Magic::TrueType => todo!("Yow! Implement read_sfnt!"),
            Magic::Woff => Font::read_woff(source_stream),
            Magic::Woff2 => todo!("Yow! Implement read_woff2!"),
        }
    }

    /// Writes out a font file.
    fn write<TDest: Write + ?Sized>(&mut self, destination: &mut TDest) -> Result<()> {
        // Find the header - we must have one
        let woff_hdr = match self.tables.get_mut(&WOFF_HEADER_TAG) {
            Some(Table::WoffHeader(woff_hdr)) => {
                // We have a WOFF header, so we can write it out
                woff_hdr
            },
            _ => {
                // We don't have a WOFF header, so we can't write it out
                Err(Error::FontSaveError)?
            }
        };
        // TBD - Rewind destination?
        //destination.rewind()?;
        // Write the header
        woff_hdr.write(destination)?;

        // Build up ordered list of tables
        let mut ordered_tags: Vec<TableTag> = Vec::new();
        for tag in self.tables.keys() {
            match tag {
                &WOFF_HEADER_TAG|&WOFF_METADATA_TAG|&WOFF_PRIVATE_DATA_TAG|
                &TABLE_DIRECTORY_TAG => {
                    // Fake table, skip it.
                    continue;
                }
                _ => {
                    match ordered_tags.binary_search(&tag) {
                        Ok(_) => Err(Error::FontSaveError)?,
                        Err(pos) => ordered_tags.insert(pos, tag.clone()),
                    }
                }
            }
        }
        // Write out all the tables in list order
        for tag in ordered_tags {
            match self.tables.get_mut(&tag) {
                Some(Table::Unspecified(table)) => table.write(destination)?,
                Some(Table::C2PA(c2pa_table)) => c2pa_table.write(destination)?,
                Some(Table::WoffHeader(_)) => Err(Error::FontSaveError)?, // canthappen
                None => Err(Error::FontSaveError)?,
            }
        }
        // Write the metadata, if any
        if let Some(Table::Unspecified(metadata)) = self.tables.get_mut(&WOFF_METADATA_TAG) {
            metadata.write(destination)?
        }
        // Write the private data, if any
        if let Some(Table::Unspecified(private_data)) = self.tables.get_mut(&WOFF_PRIVATE_DATA_TAG) {
            private_data.write(destination)?
        }
        // If we made it here, it all worked.
        Ok(())
    }

    /// Reads in a WOFF 1 font file
    fn read_woff<T: Read + Seek + ?Sized>(
        source_stream: &mut T,
    ) -> core::result::Result<Font, Error> {
        // Read in the WOFFHeader & record its chunk.
        // We expect to be called with the stream positions just past the magic number.
        //
        let mut the_font: Font = Font::new(Magic::Woff);
        the_font.magic = Magic::Woff;
        let woff_hdr = WoffHeader::new(source_stream)?;

        // Push a pseudo-table to store the header
        let woff_hdr_pseudo_table = Table::WoffHeader(woff_hdr);
        the_font.tables.insert(WOFF_HEADER_TAG, woff_hdr_pseudo_table);

        // Loop through the `tableRecords` array
        let mut table_counter = 0;
        
        while table_counter < woff_hdr.numTables as usize {
            // Try to parse the next dir entry
            let wtde = WoffTableDirEntry::new(source_stream)?;
            let next_table_dir_entry_offset = source_stream.stream_position()?;

            // Load this table
            let mut table_data: Vec<u8> = vec![0; wtde.compLength as usize];
            source_stream.seek(SeekFrom::Start(wtde.offset as u64))?;
            source_stream.read_exact(&mut table_data)?;

            let table: Table = {
                match wtde.tag {
                    //C2PA_TABLE_TAG => {
                    //    Table::C2PA({tbl: }) {}
                    //}
                    //HEAD_TABLE_TAG => {
                    //    Table::Head(TableHead) {}
                    //}
                    _ => Table::Unspecified(TableUnspecified { data: table_data }),
                }
            };

            // Store it in the bucket
            the_font.tables.insert(wtde.tag, table);

            // Increment the table counter
            table_counter += 1;

            // Head back to the table directory for the next iteration
            source_stream.seek(SeekFrom::Start(next_table_dir_entry_offset))?;
        }

        // If XML metadata is present, store it as a table.
        if woff_hdr.metaOffset != 0 {
            source_stream.seek(SeekFrom::Start(woff_hdr.metaOffset as u64))?;
            let mut meta_data: Vec<u8> = vec![0; woff_hdr.metaLength as usize];
            source_stream.read_exact(&mut meta_data)?;
            the_font.tables.insert(
                WOFF_METADATA_TAG,
                Table::Unspecified(TableUnspecified { data: meta_data }),
            );
        }

        // If private data is present, store it as a table.
        if woff_hdr.privOffset != 0 {
            source_stream.seek(SeekFrom::Start(woff_hdr.privOffset as u64))?;
            let mut private_data: Vec<u8> = vec![0; woff_hdr.privLength as usize];
            source_stream.read_exact(&mut private_data)?;
            the_font.tables.insert(
                WOFF_PRIVATE_DATA_TAG,
                Table::Unspecified(TableUnspecified { data: private_data }),
            );
        }
        Ok(the_font)
    }
}

/// TBD: All the serialization structures so far have been defined using native
/// Rust types; should we go all-out in the other direction, and establish a
/// layer of "font" types (FWORD, FIXED, etc.)?

/// WOFF 1.0 file header, from the WOFF spec.
/// 
/// TBD: Should this be treated as a "Table", perhaps with a magic tag value that
/// always sorts first, for operational reasons?
#[derive(Copy, Clone, Debug)]
#[repr(C, packed)]       // As defined by the WOFF spec. (though we don't as yet directly support exotics like FIXED)
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
    // TBD should this be `new_from_stream`?
    pub fn new<T: Read + Seek + ?Sized>(source_stream: &mut T) -> Result<Self> {
        Ok(Self {
            signature: Magic::Woff as u32,
            flavor: source_stream.read_u32::<BigEndian>()?,
            length: source_stream.read_u32::<BigEndian>()?,
            numTables: source_stream.read_u16::<BigEndian>()?,
            reserved: source_stream.read_u16::<BigEndian>()?,
            totalSfntSize: source_stream.read_u32::<BigEndian>()?,
            majorVersion: source_stream.read_u16::<BigEndian>()?,
            minorVersion: source_stream.read_u16::<BigEndian>()?,
            metaOffset: source_stream.read_u32::<BigEndian>()?,
            metaLength: source_stream.read_u32::<BigEndian>()?,
            metaOrigLength: source_stream.read_u32::<BigEndian>()?,
            privOffset: source_stream.read_u32::<BigEndian>()?,
            privLength: source_stream.read_u32::<BigEndian>()?,
        })
    }

    fn write<TDest: Write + ?Sized>(&mut self, destination: &mut TDest) -> Result<()> {
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
#[derive(Debug)]
#[repr(C, packed)]       // As defined by the WOFF spec. (though we don't as yet directly support exotics like FIXED)
#[allow(non_snake_case)] // As named by the WOFF spec.
struct WoffTableDirEntry {
    tag: TableTag,
    offset: u32,
    compLength: u32,
    origLength: u32,
    origChecksum: u32,
}
impl WoffTableDirEntry {
    pub fn new<T: Read + Seek + ?Sized>(source_stream: &mut T) -> Result<Self> {
        Ok(Self {
            tag: TableTag::new(source_stream)?,
            offset: source_stream.read_u32::<BigEndian>()?,
            compLength: source_stream.read_u32::<BigEndian>()?,
            origLength: source_stream.read_u32::<BigEndian>()?,
            origChecksum: source_stream.read_u32::<BigEndian>()?,
        })
    }
}

/// Represents types of regions within a font file.
#[derive(Debug, PartialEq, Eq)]
pub enum ChunkType {
    /// Table directory, excluding the table record array
    TableDirectory,
    /// Table data
    Table,
    /// Table record entry in the table directory
    TableRecord,
    /// Non-table data: file headers, WOFF metadata, WOFF private, ...
    _Other,
}

/// Represents regions within a font file that may be of interest when it
/// comes to hashing data for C2PA.
#[derive(Debug)]
pub struct ChunkPositions {
    /// Offset to the start of the chunk
    pub offset: u64,
    /// Length of the chunk
    pub length: u32,
    /// Tag of the chunk
    pub name: [u8; 4],
    /// Type of chunk
    pub chunk_type: ChunkType,
}

/// Custom trait for reading chunks of data from a scalable font (SFNT).
pub trait ChunkReader {
    type Error;
    /// Gets a collection of positions of chunks within the font.
    ///
    /// ## Arguments
    /// * `source_stream` - Source stream to read data from
    ///
    /// ## Returns
    /// A collection of positions/offsets and length to omit from hashing.
    fn get_chunk_positions<T: Read + Seek + ?Sized>(
        &self,
        source_stream: &mut T,
    ) -> core::result::Result<Vec<ChunkPositions>, Self::Error>;
}

/// Reads in chunks for a WOFF 1.0 file (or any file -- the format gets handled
/// during Read, we should probably just build up this chunk map at that time,
/// right?
impl ChunkReader for WoffIO {
    type Error = crate::error::Error;

    fn get_chunk_positions<T: Read + Seek + ?Sized>(
        &self,
        source_stream: &mut T,
    ) -> core::result::Result<Vec<ChunkPositions>, Self::Error> {
        source_stream.rewind()?;
        let mut positions: Vec<ChunkPositions> = Vec::new();
        // WOFFHeader
        // Create a 20-byte buffer to hold each table entry as we read through the file
        let mut woff_dirent_buf: [u8; size_of::<WoffTableDirEntry>()] =
            [0; size_of::<WoffTableDirEntry>()];
        // Verify the font has a valid version in it before assuming the rest is
        // valid (NOTE: we don't actually do anything with it, just as a safety check).
        let font_magic_u32: u32 = source_stream.read_u32::<BigEndian>()?;
        let _font_magic: Magic = <u32 as std::convert::TryInto<Magic>>::try_into(font_magic_u32)
            .map_err(|_err| Error::UnsupportedFontError)?;
        // Add the position of the table directory, excluding the actual table
        // records, as those positions will be added separately
        positions.push(ChunkPositions {
            offset: 0,
            length: size_of::<WoffHeader>() as u32,
            name: TABLE_DIRECTORY_TAG.data,
            chunk_type: ChunkType::TableDirectory,
        });

        // Table counter, to keep up with how many tables we have processed.
        let mut table_counter = 0;
        // Get the number of tables available from the next 2 bytes
        let num_tables = source_stream.read_u16::<BigEndian>()? as usize;
        // Advance to the start of the table entries
        source_stream.seek(SeekFrom::Start(size_of::<WoffHeader>() as u64))?;

        // Create a temporary vector to hold the table offsets and lengths, which
        // will be added after the table records have been added
        let mut table_offset_pos = Vec::new();

        // Loop through the `tableRecords` array
        while source_stream.read_exact(&mut woff_dirent_buf).is_ok() {
            // Grab the tag of the table record entry
            let mut table_tag: [u8; 4] = [0; 4];
            table_tag.copy_from_slice(&woff_dirent_buf[0..4]);

            // Then grab the offset and length of the actual name table to
            // create the other exclusion zone.
            let offset = (&woff_dirent_buf[5..8]).read_u32::<BigEndian>()?;
            let _comp_length = (&woff_dirent_buf[9..12]).read_u32::<BigEndian>()?;
            let orig_length = (&woff_dirent_buf[13..16]).read_u32::<BigEndian>()?;

            // At this point we will add the table record entry to the temporary
            // buffer as just a regular table
            table_offset_pos.push((offset, orig_length, table_tag, ChunkType::Table));

            // Build up table record chunk to add to the positions
            let mut name: [u8; 4] = [0; 4];
            // Copy from the table tag to be owned by the chunk position record
            name.copy_from_slice(&table_tag);

            // Create a table record chunk position as a default table record type
            // and add it to the collection of positions
            positions.push(ChunkPositions {
                offset: (size_of::<WoffHeader>() + (size_of::<WoffTableDirEntry>() * table_counter)) as u64,
                length: size_of::<WoffTableDirEntry>() as u32,
                name,
                chunk_type: ChunkType::TableRecord,
            });

            // Increment the table counter
            table_counter += 1;

            // If we have iterated over all of our tables, bail
            if table_counter >= num_tables {
                break;
            }
        }
        // Now we can add the table offsets and lengths to the positions, appearing
        // after the table record chunks, staying as close to the original font layout
        // as possible
        // NOTE: The font specification doesn't necessarily ensure the table data records
        //       have to be in order, but that shouldn't really matter.
        for entry in table_offset_pos {
            let mut name = [0; 4];
            name.copy_from_slice(entry.2.as_slice());
            positions.push(ChunkPositions {
                offset: entry.0 as u64,
                length: entry.1,
                name,
                chunk_type: entry.3,
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
    let mut font = Font::read(source).map_err(|_| Error::FontLoadError)?;
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
    let mut font = Font::read(source).map_err(|_| Error::FontLoadError)?;
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
    let mut font = Font::read(input_stream).map_err(|_| Error::FontLoadError)?;
    // If the C2PA table does not exist, then we will add an empty one
    if font.tables.get(&C2PA_TABLE_TAG).is_none() {
        font.tables.insert(C2PA_TABLE_TAG, Table::C2PA(TableC2PA::new(None, None)));
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
    let mut font = Font::read(source).map_err(|_| Error::FontLoadError)?;
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
    //source.rewind()?; hmm...
    let mut font = Font::read(source).map_err(|_| Error::FontLoadError)?;
    let old_manifest_uri_maybe = match font.tables.get_mut(&C2PA_TABLE_TAG) {
        // If there isn't one, how pleasant, there will be so much less to do.
        None => None,
        // If there is, replace its `active_manifest_uri` value with the
        // provided one.
        Some(ostensible_c2pa_table) => {
            match ostensible_c2pa_table {
                Table::C2PA(c2pa_table) => {
                    if c2pa_table.active_manifest_uri.is_none() {
                        None
                    }
                    else {
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
            ChunkType::TableDirectory => vec![HashObjectPositions {
                offset: chunk_position.offset as usize,
                length: chunk_position.length as usize,
                htype: HashBlockObjectType::Other,
            }],
            // For the table record entries, we will specialize the C2PA table
            // record and all others will be added as is
            ChunkType::TableRecord => {
                if &chunk_position.name == C2PA_TABLE_TAG.as_bytes() {
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
            ChunkType::Table|ChunkType::_Other => {
                let mut table_positions = Vec::<HashObjectPositions>::new();
                // We must split out the head table to ignore the checksum
                // adjustment, because it changes after the C2PA table is
                // written to the font
                if &chunk_position.name == HEAD_TABLE_TAG.as_bytes() {
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
                } else if &chunk_position.name == C2PA_TABLE_TAG.as_bytes() {
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
    let font: Font = Font::read(reader).map_err(|_| Error::FontLoadError)?;
    let c2pa_table: Option<TableC2PA> = match font.tables.get(&C2PA_TABLE_TAG) {
        None => None,
        Some(ostensible_c2pa_table) => {
            match ostensible_c2pa_table {
                Table::C2PA(bonafied_c2pa_table) => Some(bonafied_c2pa_table.clone()),
                _ => {
                    todo!("A non-C2PA table was found with the C2PA tag. We should report this as if it were an error, which it most certainly is.");
                }
            }
        }
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
        source_stream: &mut dyn CAIRead,
        output_stream: &mut dyn CAIReadWrite,
        embed_ref: RemoteRefEmbedType,
    ) -> Result<()> {
        match embed_ref {
            crate::asset_io::RemoteRefEmbedType::Xmp(manifest_uri) => {
                #[cfg(feature = "xmp_write")]
                {
                    font_xmp_support::add_reference_as_xmp_to_stream(
                        source_stream,
                        output_stream,
                        &manifest_uri,
                    )
                }
                #[cfg(not(feature = "xmp_write"))]
                {
                    add_reference_to_stream(source_stream, output_stream, &manifest_uri)
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

    use tempfile::tempdir;

    use super::*;
    use crate::utils::test::{fixture_path, temp_dir_path};

    #[test]
    #[cfg(not(feature = "xmp_write"))]
    /// Verifies the adding of a remote C2PA manifest reference works as
    /// expected.
    fn add_c2pa_ref() {
        let c2pa_data = "test data";

        // Load the basic WOFF test fixture
        let source = fixture_path("font.woff");

        // Create a temporary output for the file
        let temp_dir = tempdir().unwrap();
        let output = temp_dir_path(&temp_dir, "test.woff");

        // Copy the source to the output
        std::fs::copy(source, &output).unwrap();

        // Create our WoffIO asset handler for testing
        let woff_io = WoffIO {};

        let expected_manifest_uri = "https://test/ref";

        woff_io
            .embed_reference(
                &output,
                crate::asset_io::RemoteRefEmbedType::Xmp(expected_manifest_uri.to_owned()),
            )
            .unwrap();
        // Save the C2PA manifest store to the file
        woff_io
            .save_cai_store(&output, c2pa_data.as_bytes())
            .unwrap();
        // Loading it back from the same output file
        let loaded_c2pa = woff_io.read_cai_store(&output).unwrap();
        // Which should work out to be the same in the end
        assert_eq!(&loaded_c2pa, c2pa_data.as_bytes());

        match read_reference_from_font(&output) {
            Ok(Some(manifest_uri)) => assert_eq!(expected_manifest_uri, manifest_uri),
            _ => panic!("Expected to read a reference from the font file"),
        };
    }

    #[test]
    #[cfg(feature = "xmp_write")]
    /// Verifies the adding of a remote C2PA manifest reference as XMP works as
    /// expected.
    fn add_c2pa_ref() {
        use std::str::FromStr;

        use xmp_toolkit::XmpMeta;

        let c2pa_data = "test data";

        // Load the basic WOFF test fixture
        let source = fixture_path("font.woff");

        // Create a temporary output for the file
        let temp_dir = tempdir().unwrap();
        let output = temp_dir_path(&temp_dir, "test.woff");

        // Copy the source to the output
        std::fs::copy(source, &output).unwrap();

        // Create our WoffIO asset handler for testing
        let woff_io = WoffIO {};

        let expected_manifest_uri = "https://test/ref";

        woff_io
            .embed_reference(
                &output,
                crate::asset_io::RemoteRefEmbedType::Xmp(expected_manifest_uri.to_owned()),
            )
            .unwrap();
        // Save the C2PA manifest store to the file
        woff_io
            .save_cai_store(&output, c2pa_data.as_bytes())
            .unwrap();
        // Loading it back from the same output file
        let loaded_c2pa = woff_io.read_cai_store(&output).unwrap();
        // Which should work out to be the same in the end
        assert_eq!(&loaded_c2pa, c2pa_data.as_bytes());

        match read_reference_from_font(&output) {
            Ok(Some(manifest_uri)) => {
                let xmp_meta = XmpMeta::from_str(manifest_uri.as_str()).unwrap();
                let provenance = xmp_meta
                    .property("http://purl.org/dc/terms/", "provenance")
                    .unwrap();
                assert_eq!(expected_manifest_uri, provenance.value.as_str());
            }
            _ => panic!("Expected to read a reference from the font file"),
        };
    }

    /// Verify when reading the object locations for hashing, we get zero
    /// positions when the font contains zero tables
    #[test]
    fn get_chunk_positions_without_any_tables() {
        let font_data = vec![
            0x4f, 0x54, 0x54, 0x4f, // OTTO
            0x00, 0x00, // 0 tables
        ];
        let mut font_stream: Cursor<&[u8]> = Cursor::<&[u8]>::new(&font_data);
        let woff_io = WoffIO {};
        let positions = woff_io.get_chunk_positions(&mut font_stream).unwrap();
        // Should have one position reported for the table directory itself
        assert_eq!(1, positions.len());
        assert_eq!(0, positions.get(0).unwrap().offset);
        assert_eq!(12, positions.get(0).unwrap().length);
    }

    /// Verify when reading the object locations for hashing, we get zero
    /// positions when the font does not contain a C2PA font table
    #[test]
    fn get_chunk_positions_without_c2pa_table() {
        let font_data = vec![
            0x4f, 0x54, 0x54, 0x4f, // OTTO
            0x00, 0x01, // 1 tables
            0x00, 0x00, // search range
            0x00, 0x00, // entry selector
            0x00, 0x00, // range shift
            0x43, 0x32, 0x50, 0x42, // C2PB table tag
            0x00, 0x00, 0x00, 0x00, // Checksum
            0x00, 0x00, 0x00, 0x1c, // offset to table data
            0x00, 0x00, 0x00, 0x01, // length of table data
            0x00, // C2PB data
        ];
        let mut font_stream: Cursor<&[u8]> = Cursor::<&[u8]>::new(&font_data);
        let woff_io = WoffIO {};
        let positions = woff_io.get_chunk_positions(&mut font_stream).unwrap();
        // Should have 3 positions reported for the table directory, table
        // record, and the table data
        assert_eq!(3, positions.len());

        let table_directory = positions.get(0).unwrap();
        assert_eq!(ChunkType::TableDirectory, table_directory.chunk_type);
        assert_eq!(0, table_directory.offset);
        assert_eq!(12, table_directory.length);

        let table_record = positions.get(1).unwrap();
        assert_eq!(ChunkType::TableRecord, table_record.chunk_type);
        assert_eq!(12, table_record.offset);
        assert_eq!(16, table_record.length);

        let table = positions.get(2).unwrap();
        assert_eq!(ChunkType::Table, table.chunk_type);
        assert_eq!(28, table.offset);
        assert_eq!(1, table.length);
    }

    #[test]
    fn get_object_locations() {
        // Load the basic WOFF test fixture
        let source = fixture_path("font.woff");

        // Create a temporary output for the file
        let temp_dir = tempdir().unwrap();
        let output = temp_dir_path(&temp_dir, "test.woff");

        // Copy the source to the output
        std::fs::copy(source, &output).unwrap();

        // Create our WoffIO asset handler for testing
        let woff_io = WoffIO {};
        // The font has 11 records, 11 tables, 1 table directory
        // but the head table will expand from 1 to 3 positions bringing it to 25
        // And then the required C2PA chunks will be added, bringing it to 27
        let object_positions = woff_io.get_object_locations(&output).unwrap();
        assert_eq!(27, object_positions.len());
    }

    /// Verify the C2PA table data can be read from a font stream
    #[test]
    fn reads_c2pa_table_from_stream() {
        let font_data = vec![
            0x4f, 0x54, 0x54, 0x4f, // wOFF - WOFF tag
            0x00, 0x01, // 1 table
            0x00, 0x00, // search range
            0x00, 0x00, // entry selector
            0x00, 0x00, // range shift
            0x43, 0x32, 0x50, 0x41, // C2PA table tag
            0x00, 0x00, 0x00, 0x00, // Checksum
            0x00, 0x00, 0x00, 0x1c, // offset to table data
            0x00, 0x00, 0x00, 0x23, // length of table data
            0x00, 0x00, // Major version
            0x00, 0x01, // Minor version
            0x00, 0x00, 0x00, 0x12, // Active manifest URI offset
            0x00, 0x08, // Active manifest URI length
            0x00, 0x00, 0x00, 0x1a, // C2PA manifest store offset
            0x00, 0x00, 0x00, 0x09, // C2PA manifest store length
            0x66, 0x69, 0x6c, 0x65, 0x3a, 0x2f, 0x2f,
            0x61, // active manifest uri data (e.g., file://a)
            0x74, 0x65, 0x73, 0x74, 0x2d, 0x64, 0x61, 0x74, 0x61, // C2PA manifest store data
        ];
        let mut font_stream: Cursor<&[u8]> = Cursor::<&[u8]>::new(&font_data);
        let c2pa_data = read_c2pa_from_stream(&mut font_stream).unwrap();
        // Verify the active manifest uri
        assert_eq!(Some("file://a".to_string()), c2pa_data.active_manifest_uri);
        // Verify the embedded C2PA data as well
        assert_eq!(
            Some(vec![0x74, 0x65, 0x73, 0x74, 0x2d, 0x64, 0x61, 0x74, 0x61].as_ref()),
            c2pa_data.get_manifest_store()
        );
    }

    /// Verifies the ability to write/read C2PA manifest store data to/from a
    /// WOFF font
    #[test]
    fn remove_c2pa_manifest_store() {
        let c2pa_data = "test data";

        // Load the basic WOFF test fixture
        let source = fixture_path("font.woff");

        // Create a temporary output for the file
        let temp_dir = tempdir().unwrap();
        let output = temp_dir_path(&temp_dir, "test.woff");

        // Copy the source to the output
        std::fs::copy(source, &output).unwrap();

        // Create our WoffIO asset handler for testing
        let woff_io = WoffIO {};

        // Save the C2PA manifest store to the file
        woff_io
            .save_cai_store(&output, c2pa_data.as_bytes())
            .unwrap();
        // Loading it back from the same output file
        let loaded_c2pa = woff_io.read_cai_store(&output).unwrap();
        // Which should work out to be the same in the end
        assert_eq!(&loaded_c2pa, c2pa_data.as_bytes());

        woff_io.remove_cai_store(&output).unwrap();
        match woff_io.read_cai_store(&output) {
            Err(Error::JumbfNotFound) => (),
            _ => panic!("Should not contain any C2PA data"),
        };
    }

    /// Verifies the ability to write/read C2PA manifest store data to/from an
    /// Woff font
    #[test]
    fn write_read_c2pa_from_font() {
        let c2pa_data = "test data";

        // Load the basic WOFF test fixture
        let source = fixture_path("font.woff");

        // Create a temporary output for the file
        let temp_dir = tempdir().unwrap();
        let output = temp_dir_path(&temp_dir, "test.woff");

        // Copy the source to the output
        std::fs::copy(source, &output).unwrap();

        // Create our WoffIO asset handler for testing
        let woff_io = WoffIO {};

        // Save the C2PA manifest store to the file
        woff_io
            .save_cai_store(&output, c2pa_data.as_bytes())
            .unwrap();
        // Loading it back from the same output file
        let loaded_c2pa = woff_io.read_cai_store(&output).unwrap();
        // Which should work out to be the same in the end
        assert_eq!(&loaded_c2pa, c2pa_data.as_bytes());
    }

    #[cfg(feature = "xmp_write")]
    #[cfg(test)]
    pub mod font_xmp_support_tests {
        use std::{fs::File, io::Cursor, str::FromStr};

        use tempfile::tempdir;
        use xmp_toolkit::XmpMeta;

        use crate::{
            asset_handlers::woff_io::{font_xmp_support, WoffIO},
            asset_io::CAIReader,
            utils::test::temp_dir_path,
            Error,
        };

        /// Verifies the `font_xmp_support::add_reference_as_xmp_to_stream` is
        /// able to add a reference to as XMP when there is already data in the
        /// reference field.
        #[test]
        fn add_reference_as_xmp_to_stream_with_data() {
            // Load the basic WOFF test fixture
            let source = crate::utils::test::fixture_path("font.woff");

            // Create a temporary output for the file
            let temp_dir = tempdir().unwrap();
            let output = temp_dir_path(&temp_dir, "test.woff");

            // Copy the source to the output
            std::fs::copy(source, &output).unwrap();

            // Add a reference to the font
            match font_xmp_support::add_reference_as_xmp_to_font(&output, "test data") {
                Ok(_) => {}
                Err(_) => panic!("Unexpected error when building XMP data"),
            }

            // Add again, with a new value
            match font_xmp_support::add_reference_as_xmp_to_font(&output, "new test data") {
                Ok(_) => {}
                Err(_) => panic!("Unexpected error when building XMP data"),
            }

            let otf_handler = WoffIO {};
            let mut f: File = File::open(output).unwrap();
            match otf_handler.read_xmp(&mut f) {
                Some(xmp_data_str) => {
                    let xmp_data = XmpMeta::from_str(&xmp_data_str).unwrap();
                    match xmp_data.property("http://purl.org/dc/terms/", "provenance") {
                        Some(xmp_value) => assert_eq!("new test data", xmp_value.value),
                        None => panic!("Expected a value for provenance"),
                    }
                }
                None => panic!("Expected to read XMP from the resource."),
            }
        }

        /// Verifies the `font_xmp_support::build_xmp_from_stream` method
        /// correctly returns error for NotFound when there is no data in the
        /// stream to return.
        #[test]
        fn build_xmp_from_stream_without_reference() {
            let font_data = vec![
                0x4f, 0x54, 0x54, 0x4f, // OTTO
                0x00, 0x01, // 1 tables
                0x00, 0x00, // search range
                0x00, 0x00, // entry selector
                0x00, 0x00, // range shift
                0x43, 0x32, 0x50, 0x42, // C2PB table tag
                0x00, 0x00, 0x00, 0x00, // Checksum
                0x00, 0x00, 0x00, 0x1c, // offset to table data
                0x00, 0x00, 0x00, 0x01, // length of table data
                0x00, // C2PB data
            ];
            let mut font_stream: Cursor<&[u8]> = Cursor::<&[u8]>::new(&font_data);
            match font_xmp_support::build_xmp_from_stream(&mut font_stream) {
                Ok(_) => panic!("Did not expect an OK result, as data is missing"),
                Err(Error::NotFound) => {}
                Err(_) => panic!("Unexpected error when building XMP data"),
            }
        }

        /// Verifies the `font_xmp_support::build_xmp_from_stream` method
        /// correctly returns error for NotFound when there is no data in the
        /// stream to return.
        #[test]
        fn build_xmp_from_stream_with_reference_not_xmp() {
            let font_data = vec![
                // Yikes! Needs to change
                0x4f, 0x54, 0x54, 0x4f, // wOFF - WOFF tag
                0x00, 0x01, // 1 tables
                0x00, 0x00, // search range
                0x00, 0x00, // entry selector
                0x00, 0x00, // range shift
                0x43, 0x32, 0x50, 0x41, // C2PA table tag
                0x00, 0x00, 0x00, 0x00, // Checksum
                0x00, 0x00, 0x00, 0x1c, // offset to table data
                0x00, 0x00, 0x00, 0x1a, // length of table data
                0x00, 0x00, // Major version
                0x00, 0x01, // Minor version
                0x00, 0x00, 0x00, 0x12, // Active manifest URI offset
                0x00, 0x08, // Active manifest URI length
                0x00, 0x00, 0x00, 0x00, // C2PA manifest store offset
                0x00, 0x00, 0x00, 0x00, // C2PA manifest store length
                0x66, 0x69, 0x6c, 0x65, 0x3a, 0x2f, 0x2f, 0x61, // active manifest uri data
            ];
            let mut font_stream: Cursor<&[u8]> = Cursor::<&[u8]>::new(&font_data);
            match font_xmp_support::build_xmp_from_stream(&mut font_stream) {
                Ok(_xmp_data) => {}
                Err(_) => panic!("Unexpected error when building XMP data"),
            }
        }
    }
}

