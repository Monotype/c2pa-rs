// Copyright 2022,2023 Monotype. All rights reserved.
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
    convert::TryFrom,
    fs::File,
    io::{BufReader, Cursor, Read, Seek, SeekFrom, Write},
    path::*,
};

use byteorder::{BigEndian, ReadBytesExt};
use fonttools::{font::Font, table_store::CowPtr, tables, tables::C2PA::C2PA, types::*};
use log::debug;

use crate::{
    asset_io::{
        AssetIO, CAIRead, CAIReadWrite, CAIReader, CAIWriter, HashBlockObjectType,
        HashObjectPositions, RemoteRefEmbed, RemoteRefEmbedType,
    },
    error::{Error, Result},
};

/// Supported extension and mime-types
static SUPPORTED_TYPES: [&str; 7] = [
    "application/font-snft",
    "font/otf",
    "font/sfnt",
    "font/otf",
    "otf",
    "sfnt",
    "ttf",
];

/// Tag for the 'C2PA' table in a font.
const C2PA_TABLE_TAG: Tag = tables::C2PA::TAG;

/// Various valid version tags seen in a OTF/TTF file.
pub enum FontVersion {
    /// TrueType (ttf) version for Windows and/or Adobe
    TrueType = 0x00010000,
    /// OpenType (otf) version
    OpenType = 0x4f54544f,
    /// Old style PostScript font housed in a sfnt wrapper
    Typ1 = 0x74797031,
    /// 'true' font, a TrueType font for OS X and iOS only
    AppleTrue = 0x74727565,
}

/// Used to try and convert from a u32 value to FontVersion
impl TryFrom<u32> for FontVersion {
    type Error = ();

    /// Tries to convert from u32 to a valid font version.
    fn try_from(v: u32) -> core::result::Result<Self, Self::Error> {
        match v {
            x if x == FontVersion::TrueType as u32 => Ok(FontVersion::TrueType),
            x if x == FontVersion::OpenType as u32 => Ok(FontVersion::OpenType),
            x if x == FontVersion::Typ1 as u32 => Ok(FontVersion::Typ1),
            x if x == FontVersion::AppleTrue as u32 => Ok(FontVersion::AppleTrue),
            _ => Err(()),
        }
    }
}

/// Adds C2PA manifest store data to a font file
///
/// ## Arguments
///
/// * `font_path` - Path to a font file
/// * `manifest_store_data` - C2PA manifest store data to add to the font file
fn add_c2pa_to_font(font_path: &Path, manifest_store_data: &[u8]) -> Result<()> {
    debug!("Saving to file path: {:?}", font_path);
    // open the font source
    let mut font = std::fs::File::open(font_path)?;
    // Will use a buffer to create a stream over the data
    let mut font_buffer = Vec::new();
    // which is read from the font file
    font.read_to_end(&mut font_buffer)?;
    let mut font_stream = Cursor::new(font_buffer);

    // And we will open the same file as write, truncating it
    let mut new_file = std::fs::File::options()
        .write(true)
        .truncate(true)
        .open(font_path)?;

    add_c2pa_to_stream(&mut font_stream, &mut new_file, manifest_store_data)
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
    let mut font_file: Font = Font::from_reader(source).map_err(|_err| Error::FontLoadError)?;
    match font_file.tables.C2PA() {
        Ok(Some(c2pa_table)) => {
            font_file.tables.insert(C2PA::new(
                c2pa_table.activeManifestUri.clone(),
                Some(manifest_store_data.to_vec()),
            ));
        }
        Ok(None) => font_file
            .tables
            .insert(C2PA::new(None, Some(manifest_store_data.to_vec()))),
        Err(_) => return Err(Error::DeserializationError),
    };
    // Write to the destination stream
    font_file
        .write(destination)
        .map_err(|_| Error::FontSaveError)?;
    Ok(())
}

/// Adds the manifest URI reference to the font at the given path.
///
/// ## Arguments
///
/// * `asset_path` - Path to a font file
/// * `manifest_uri` - Reference URI to a manifest store
fn add_reference_to_font(asset_path: &Path, manifest_uri: &str) -> Result<()> {
    // open the font source
    let mut font = std::fs::File::open(asset_path)?;
    // Will use a buffer to create a stream over the data
    let mut font_buffer = Vec::new();
    // which is read from the font file
    font.read_to_end(&mut font_buffer)?;
    let mut font_stream = Cursor::new(font_buffer);

    // And we will open the same file as write, truncating it
    let mut new_file = std::fs::File::options()
        .write(true)
        .truncate(true)
        .open(asset_path)?;

    // Write the manifest URI to the stream
    add_reference_to_stream(&mut font_stream, &mut new_file, manifest_uri)
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
    let mut font = Font::from_reader(source).map_err(|_| Error::FontLoadError)?;
    match font.tables.C2PA() {
        Ok(Some(c2pa_table)) => {
            font.tables.insert(C2PA::new(
                Some(manifest_uri.to_string()),
                c2pa_table.get_manifest_store().clone().map(|x| x.to_vec()),
            ));
        }
        Ok(None) => font
            .tables
            .insert(C2PA::new(Some(manifest_uri.to_string()), None)),
        Err(_) => return Err(Error::DeserializationError),
    };
    font.write(destination).map_err(|_| Error::FontSaveError)?;
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
    let mut font = std::fs::File::open(font_path)?;
    // Will use a buffer to create a stream over the data
    let mut font_buffer = Vec::new();
    // which is read from the font file
    font.read_to_end(&mut font_buffer)?;
    let mut font_stream = Cursor::new(font_buffer);
    read_reference_to_stream(&mut font_stream)
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
fn read_reference_to_stream<TSource>(source: &mut TSource) -> Result<Option<String>>
where
    TSource: Read + Seek + ?Sized,
{
    match read_c2pa_from_stream(source) {
        Ok(c2pa_data) => Ok(c2pa_data.activeManifestUri.to_owned()),
        Err(Error::JumbfNotFound) => Ok(None),
        Err(_) => Err(Error::DeserializationError),
    }
}

/// Remove the `C2PA` font table from the font file.
///
/// ## Arguments
///
/// * `asset_path` - path to the font file to remove C2PA from
fn remove_c2pa_from_font(asset_path: &Path) -> Result<()> {
    // open the font source
    let mut font = std::fs::File::open(asset_path)?;
    // Will use a buffer to create a stream over the data
    let mut font_buffer = Vec::new();
    // which is read from the font file
    font.read_to_end(&mut font_buffer)?;
    let mut font_stream = Cursor::new(font_buffer);

    // And we will open the same file as write, truncating it
    let mut new_file = std::fs::File::options()
        .write(true)
        .truncate(true)
        .open(asset_path)?;

    // Write the manifest URI to the stream
    remove_c2pa_from_stream(&mut font_stream, &mut new_file)
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
    // Load the font from the stream
    let mut font = Font::from_reader(source).map_err(|_| Error::FontLoadError)?;
    // Remove the table from the collection
    font.tables.remove(C2PA_TABLE_TAG);
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
    let mut font = Font::from_reader(source).map_err(|_| Error::FontLoadError)?;
    let manifest_uri = match font.tables.C2PA() {
        Ok(Some(c2pa_table)) => {
            let manifest_uri = c2pa_table.activeManifestUri.clone();
            font.tables.insert(C2PA::new(
                None,
                c2pa_table.get_manifest_store().clone().map(|x| x.to_vec()),
            ));
            manifest_uri
        }
        Ok(None) => None,
        Err(_) => return Err(Error::DeserializationError),
    };
    font.write(destination).map_err(|_| Error::FontSaveError)?;
    Ok(manifest_uri)
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
fn get_object_locations_from_stream<T>(reader: &mut T) -> Result<Vec<HashObjectPositions>>
where
    T: Read + Seek + ?Sized,
{
    let mut positions: Vec<HashObjectPositions> = Vec::new();
    let table_header_sz: usize = 12;
    let table_entry_sz: usize = 16;
    // Create a 16-byte buffer to hold each table entry as we read through the file
    let mut table_entry_buf: [u8; 16] = [0; 16];
    // We need to get the offset to the 'name' table and exclude the length of it and its data.
    // Verify the font has a valid version in it before assuming the rest is
    // valid (NOTE: we don't actually do anything with it, just as a safety check).
    let sfnt_u32: u32 = reader.read_u32::<BigEndian>()?;
    let _sfnt_version: FontVersion =
        <u32 as std::convert::TryInto<FontVersion>>::try_into(sfnt_u32)
            .map_err(|_err| Error::UnsupportedFontError)?;

    // Using a counter to calculate the offset to the name table
    let mut table_counter: usize = 0;
    // Get the number of tables available from the next 2 bytes
    let num_tables: u16 = reader.read_u16::<BigEndian>()?;
    // Advance to the start of the table entries
    reader.seek(SeekFrom::Start(12))?;

    while reader.read_exact(&mut table_entry_buf).is_ok() {
        if C2PA_TABLE_TAG.as_bytes() == &table_entry_buf[0..4] {
            // We will need to add a position for the 'C2PA' entry since the
            // checksum changes.
            positions.push(HashObjectPositions {
                offset: table_header_sz + (table_entry_sz * table_counter),
                length: table_entry_sz,
                htype: HashBlockObjectType::Cai,
            });

            // Then grab the offset and length of the actual name table to
            // create the other exclusion zone.

            let offset = (&table_entry_buf[8..12]).read_u32::<BigEndian>()?;
            let length = (&table_entry_buf[12..16]).read_u32::<BigEndian>()?;
            positions.push(HashObjectPositions {
                offset: offset as usize,
                length: length as usize,
                htype: HashBlockObjectType::Cai,
            });

            // Finally return our collection of positions to ignore/exclude.
            return Ok(positions);
        }
        table_counter += 1;

        // If we have iterated over all of our tables, bail
        if table_counter >= num_tables as usize {
            break;
        }
    }
    // We must provide positions for operations to work, so if we don't have
    // one we will default to the entire file
    if positions.is_empty() {
        let current_pos = reader.stream_position()?;
        reader.seek(SeekFrom::End(0))?;
        let stream_len = reader.stream_position()?;
        reader.seek(SeekFrom::Start(current_pos))?;
        positions.push(HashObjectPositions {
            offset: 0,
            length: stream_len as usize,
            htype: HashBlockObjectType::Cai,
        });
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
fn read_c2pa_from_stream<T: Read + Seek + ?Sized>(reader: &mut T) -> Result<CowPtr<C2PA>> {
    let font: Font = Font::from_reader(reader).map_err(|_| Error::FontLoadError)?;
    // Grab the C2PA table.
    font.tables
        .C2PA()
        .map_err(|_err| Error::DeserializationError)?
        .ok_or(Error::JumbfNotFound)
}

/// Main OTF IO feature.
pub struct OtfIO {}

impl OtfIO {}

/// OTF implementation of the CAILoader trait.
impl CAIReader for OtfIO {
    fn read_cai(&self, asset_reader: &mut dyn CAIRead) -> Result<Vec<u8>> {
        let c2pa_table = read_c2pa_from_stream(asset_reader)?;
        match c2pa_table.get_manifest_store() {
            Some(manifest_store) => Ok(manifest_store.to_vec()),
            _ => Ok(vec![]),
        }
    }

    #[allow(unused_variables)]
    fn read_xmp(&self, asset_reader: &mut dyn CAIRead) -> Option<String> {
        // Fonts have no XMP data.
        None
    }
}

/// OTF/TTF implementations for the CAIWriter trait.
impl CAIWriter for OtfIO {
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
        get_object_locations_from_stream(input_stream)
    }

    fn remove_cai_store_from_stream(
        &self,
        input_stream: &mut dyn CAIRead,
        output_stream: &mut dyn CAIReadWrite,
    ) -> Result<()> {
        remove_c2pa_from_stream(input_stream, output_stream)
    }
}

/// OTF/TTF implementations for the AssetIO trait.
impl AssetIO for OtfIO {
    fn new(_asset_type: &str) -> Self
    where
        Self: Sized,
    {
        OtfIO {}
    }

    fn get_handler(&self, asset_type: &str) -> Box<dyn AssetIO> {
        Box::new(OtfIO::new(asset_type))
    }

    fn get_reader(&self) -> &dyn CAIReader {
        self
    }

    fn get_writer(&self, asset_type: &str) -> Option<Box<dyn CAIWriter>> {
        Some(Box::new(OtfIO::new(asset_type)))
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
        let file = File::open(asset_path)?;
        let mut buf_reader = BufReader::new(file);
        get_object_locations_from_stream(&mut buf_reader)
    }

    fn remove_cai_store(&self, asset_path: &Path) -> Result<()> {
        remove_c2pa_from_font(asset_path)
    }
}

impl RemoteRefEmbed for OtfIO {
    #[allow(unused_variables)]
    fn embed_reference(
        &self,
        asset_path: &Path,
        embed_ref: crate::asset_io::RemoteRefEmbedType,
    ) -> Result<()> {
        match embed_ref {
            crate::asset_io::RemoteRefEmbedType::Xmp(manifest_uri) => {
                add_reference_to_font(asset_path, &manifest_uri)
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
                add_reference_to_stream(source_stream, output_stream, &manifest_uri)
            }
            crate::asset_io::RemoteRefEmbedType::StegoS(_) => Err(Error::UnsupportedType),
            crate::asset_io::RemoteRefEmbedType::StegoB(_) => Err(Error::UnsupportedType),
            crate::asset_io::RemoteRefEmbedType::Watermark(_) => Err(Error::UnsupportedType),
        }
    }
}

#[cfg(test)]
pub mod tests {
    use tempfile::tempdir;

    use super::*;
    use crate::utils::test::temp_dir_path;

    #[test]
    fn add_c2pa_ref() {
        let c2pa_data = "test data";

        // Load the basic OTF test fixture
        let source = crate::utils::test::fixture_path("font.otf");

        // Create a temporary output for the file
        let temp_dir = tempdir().unwrap();
        let output = temp_dir_path(&temp_dir, "test.otf");

        // Copy the source to the output
        std::fs::copy(source, &output).unwrap();

        // Create our OtfIO asset handler for testing
        let otf_io = OtfIO {};

        let expected_manifest_uri = "https://test/ref";

        otf_io
            .embed_reference(
                &output,
                crate::asset_io::RemoteRefEmbedType::Xmp(expected_manifest_uri.to_owned()),
            )
            .unwrap();
        // Save the C2PA manifest store to the file
        otf_io
            .save_cai_store(&output, c2pa_data.as_bytes())
            .unwrap();
        // Loading it back from the same output file
        let loaded_c2pa = otf_io.read_cai_store(&output).unwrap();
        // Which should work out to be the same in the end
        assert_eq!(&loaded_c2pa, c2pa_data.as_bytes());

        match read_reference_from_font(&output) {
            Ok(Some(manifest_uri)) => assert_eq!(expected_manifest_uri, manifest_uri),
            _ => panic!("Expected to read a reference from the font file"),
        };
    }

    /// Verify when reading the object locations for hashing, we get zero
    /// positions when the font contains zero tables
    #[test]
    fn get_object_locations_without_any_tables() {
        let font_data = vec![
            0x4f, 0x54, 0x54, 0x4f, // OTTO
            0x00, 0x00, // 0 tables
        ];
        let mut font_stream: Cursor<&[u8]> = Cursor::<&[u8]>::new(&font_data);
        let positions = get_object_locations_from_stream(&mut font_stream).unwrap();
        // Should have one position reported for the entire "file"
        assert_eq!(1, positions.len());
        assert_eq!(0, positions.get(0).unwrap().offset);
        assert_eq!(6, positions.get(0).unwrap().length);
    }

    /// Verify when reading the object locations for hashing, we get zero
    /// positions when the font does not contain a C2PA font table
    #[test]
    fn get_object_locations_without_c2pa_table() {
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
        let positions = get_object_locations_from_stream(&mut font_stream).unwrap();
        // Should have one position reported for the entire "file"
        assert_eq!(1, positions.len());
        assert_eq!(0, positions.get(0).unwrap().offset);
        assert_eq!(29, positions.get(0).unwrap().length);
    }

    /// Verifies the correct object positions are returned when a font contains
    /// C2PA data
    #[test]
    fn get_object_locations_with_table() {
        let font_data = vec![
            0x4f, 0x54, 0x54, 0x4f, // OTTO - OpenType tag
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
        let positions = get_object_locations_from_stream(&mut font_stream).unwrap();
        // Should have 2 positions reported
        assert_eq!(2, positions.len());
        // The first one is the position to the table header entry which describes where
        // the C2PA table is
        assert_eq!(16, positions.get(0).unwrap().length);
        assert_eq!(12, positions.get(0).unwrap().offset);
        // The second one is the position of the actual C2PA table
        assert_eq!(26, positions.get(1).unwrap().length);
        assert_eq!(28, positions.get(1).unwrap().offset);
    }

    /// Verify the C2PA table data can be read from a font stream
    #[test]
    fn reads_c2pa_table_from_stream() {
        let font_data = vec![
            0x4f, 0x54, 0x54, 0x4f, // OTTO - OpenType tag
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
        assert_eq!(Some("file://a".to_string()), c2pa_data.activeManifestUri);
        // Verify the embedded C2PA data as well
        assert_eq!(
            Some(vec![0x74, 0x65, 0x73, 0x74, 0x2d, 0x64, 0x61, 0x74, 0x61].as_ref()),
            c2pa_data.get_manifest_store()
        );
    }

    /// Verifies the ability to write/read C2PA manifest store data to/from an
    /// OpenType font
    #[test]
    fn remove_c2pa_manifest_store() {
        let c2pa_data = "test data";

        // Load the basic OTF test fixture
        let source = crate::utils::test::fixture_path("font.otf");

        // Create a temporary output for the file
        let temp_dir = tempdir().unwrap();
        let output = temp_dir_path(&temp_dir, "test.otf");

        // Copy the source to the output
        std::fs::copy(source, &output).unwrap();

        // Create our OtfIO asset handler for testing
        let otf_io = OtfIO {};

        // Save the C2PA manifest store to the file
        otf_io
            .save_cai_store(&output, c2pa_data.as_bytes())
            .unwrap();
        // Loading it back from the same output file
        let loaded_c2pa = otf_io.read_cai_store(&output).unwrap();
        // Which should work out to be the same in the end
        assert_eq!(&loaded_c2pa, c2pa_data.as_bytes());

        otf_io.remove_cai_store(&output).unwrap();
        match otf_io.read_cai_store(&output) {
            Err(Error::JumbfNotFound) => (),
            _ => panic!("Should not contain any C2PA data"),
        };
    }

    /// Verifies the ability to write/read C2PA manifest store data to/from an
    /// OpenType font
    #[test]
    fn write_read_c2pa_from_font() {
        let c2pa_data = "test data";

        // Load the basic OTF test fixture
        let source = crate::utils::test::fixture_path("font.otf");

        // Create a temporary output for the file
        let temp_dir = tempdir().unwrap();
        let output = temp_dir_path(&temp_dir, "test.otf");

        // Copy the source to the output
        std::fs::copy(source, &output).unwrap();

        // Create our OtfIO asset handler for testing
        let otf_io = OtfIO {};

        // Save the C2PA manifest store to the file
        otf_io
            .save_cai_store(&output, c2pa_data.as_bytes())
            .unwrap();
        // Loading it back from the same output file
        let loaded_c2pa = otf_io.read_cai_store(&output).unwrap();
        // Which should work out to be the same in the end
        assert_eq!(&loaded_c2pa, c2pa_data.as_bytes());
    }
}
