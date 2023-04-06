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
use fonttools::{font::Font, tables, tables::C2PA::C2PA, types::*};
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

/// Gets a collection of positions of hash objects, which are to be excluded from the hashing.
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
    Ok(positions)
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

/// Main OTF IO feature.
pub struct OtfIO {}

impl OtfIO {}

/// OTF implementation of the CAILoader trait.
impl CAIReader for OtfIO {
    fn read_cai(&self, asset_reader: &mut dyn CAIRead) -> Result<Vec<u8>> {
        let font_file: Font =
            Font::from_reader(asset_reader).map_err(|_err| Error::FontLoadError)?;
        // Grab the C2PA table.
        let c2pa_table = font_file
            .tables
            .C2PA()
            .map_err(|_err| Error::DeserializationError)?
            .ok_or(Error::JumbfNotFound)?;
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
        _input_stream: &mut dyn CAIRead,
        _output_stream: &mut dyn CAIReadWrite,
        _store_bytes: &[u8],
    ) -> Result<()> {
        todo!("Implement write_cai for streaming");
    }

    fn get_object_locations_from_stream(
        &self,
        input_stream: &mut dyn CAIRead,
    ) -> Result<Vec<HashObjectPositions>> {
        get_object_locations_from_stream(input_stream)
    }

    fn remove_cai_store_from_stream(
        &self,
        _input_stream: &mut dyn CAIRead,
        _output_stream: &mut dyn CAIReadWrite,
    ) -> Result<()> {
        todo!("Implement for streaming")
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
        debug!("Saving to file path: {:?}", asset_path);
        let mut font_file: Font = Font::load(asset_path).map_err(|_err| Error::FontLoadError)?;
        match font_file.tables.C2PA() {
            Ok(Some(c2pa_table)) => {
                font_file.tables.insert(C2PA::new(
                    c2pa_table.activeManifestUri.clone(),
                    Some(store_bytes.to_vec()),
                ));
            }
            Ok(None) => font_file
                .tables
                .insert(C2PA::new(None, Some(store_bytes.to_vec()))),
            Err(_) => return Err(Error::DeserializationError),
        };
        // Save back to the original file.
        font_file
            .save(asset_path)
            .map_err(|_err| Error::FontSaveError)?;
        Ok(())
    }

    fn get_object_locations(&self, asset_path: &Path) -> Result<Vec<HashObjectPositions>> {
        let file = std::fs::File::open(asset_path)?;
        let mut buf_reader = BufReader::new(file);
        get_object_locations_from_stream(&mut buf_reader)
    }

    fn remove_cai_store(&self, asset_path: &Path) -> Result<()> {
        let mut font_file: Font = Font::load(asset_path).map_err(|_err| Error::FontLoadError)?;
        font_file.tables.remove(C2PA_TABLE_TAG);
        // Write out modified font.
        font_file
            .save(asset_path)
            .map_err(|_err| Error::FontSaveError)?;

        Ok(())
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

    /* Rudimentary integration test, left here to allow for quick verification until
       property unit/integration testing has been implemented.

    use super::*;

        #[test]
        fn add_cai() {
            let font_path = Path::new("<path to font>/CultStd.otf");
            let manifest: [u8; 12] = [0u8; 12];
            let otf_io = OtfIO {};
            otf_io.save_cai_store(&font_path, &manifest).ok();
            let parsed_manifest = otf_io.read_cai_store(&font_path).unwrap();
            let parsed_manifest_slice = parsed_manifest.as_slice();
            assert_eq!(manifest, parsed_manifest_slice);
        }
    */
}
