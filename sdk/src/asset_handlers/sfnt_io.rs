// Copyright 2022,2023,2025 Monotype. All rights reserved.
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
    fs::File,
    io::{BufReader, Cursor, Read, Seek, Write},
    path::*,
};

use c2pa_font_handler::{
    c2pa::{C2PASupport, ContentCredentialRecord, UpdatableC2PA, UpdateContentCredentialRecord},
    chunks::{ChunkPosition, ChunkReader, ChunkTypeTrait},
    sfnt::{
        font::{SfntChunkType, SfntFont},
        table::{NamedTable, TableC2PA},
    },
    tag::FontTag,
    Font, FontDataRead, MutFontDataWrite,
};
use log::trace;
use serde_bytes::ByteBuf;
use tempfile::TempDir;

use crate::{
    assertions::BoxMap,
    asset_handlers::font_io::*,
    asset_io::{
        AssetBoxHash, AssetIO, CAIRead, CAIReadWrite, CAIReader, CAIWriter, HashBlockObjectType,
        HashObjectPositions, RemoteRefEmbed, RemoteRefEmbedType,
    },
    error::Error,
};

/// Font software IO object
pub(crate) struct SfntIO {}

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
/// # Remarks
/// This module is only compiled when the `font_xmp` feature is enabled.
#[cfg(feature = "font_xmp")]
mod font_xmp_support {
    use std::io::SeekFrom;

    use uuid::Uuid;

    use super::*;
    use crate::utils::xmp_inmemory_utils::{add_provenance, add_xmp_key, MIN_XMP};

    /// Creates a default `XmpMeta` object for fonts, using the specified
    /// document and instance identifiers.
    ///
    /// # Remarks
    /// Default/random values will be used for the document/instance IDs as
    /// needed.
    fn default_font_xmp_meta(
        document_id: Option<String>,
        instance_id: Option<String>,
    ) -> Result<String> {
        // Start with the default key.
        let mut xmp = MIN_XMP.to_string();

        // Add in the namespace for DocumentID and InstanceID.
        xmp =
            add_xmp_key(&xmp, "xmlns:xmpMM", "http://ns.adobe.com/xap/1.0/mm/").map_err(|_e| {
                FontError::XmpError("Unable to add media management namespace".to_string())
            })?;

        // Add in DocumentID and InstanceID.
        xmp = add_xmp_key(
            &xmp,
            "xmpMM:DocumentID",
            document_id.unwrap_or(Uuid::new_v4().to_string()).as_str(),
        )
        .map_err(|_e| FontError::XmpError("Unable to add DocumentID".to_string()))?;
        xmp = add_xmp_key(
            &xmp,
            "xmpMM:InstanceID",
            instance_id.unwrap_or(Uuid::new_v4().to_string()).as_str(),
        )
        .map_err(|_e| FontError::XmpError("Unable to add InstanceID".to_string()))?;

        Ok(xmp)
    }

    /// Builds a `XmpMeta` element from the data within the source stream, based
    /// on either the information already in the stream or default values.
    ///
    /// # Remarks
    /// The use of this function really shouldn't be needed, but currently the SDK
    /// is tightly coupled to the use of XMP with assets.
    pub(crate) fn build_xmp_from_stream<TSource>(source: &mut TSource) -> Result<String>
    where
        TSource: Read + Seek + ?Sized,
    {
        match read_reference_from_stream(source)? {
            // For now we pretend the reference read from the stream is really XMP
            // data
            Some(xmp) => Ok(xmp),
            // Mention there is no data representing XMP found
            None => Err(FontError::XmpNotFound),
        }
    }

    /// Adds a C2PA manifest reference (specified by URI, JUMBF or URL based) as
    /// XMP data to a font file (specified by path).
    ///
    /// # Remarks
    /// This method is considered a stop-gap for now until the official SDK
    /// offers a more generic method to indicate a document ID, instance ID,
    /// and a reference to the a remote manifest.
    pub(crate) fn add_reference_as_xmp_to_font(font_path: &Path, manifest_uri: &str) -> Result<()> {
        process_file_with_streams(font_path, move |input_stream, temp_file| {
            // Write the manifest URI to the stream
            add_reference_as_xmp_to_stream(input_stream, temp_file.get_mut_file(), manifest_uri)
        })
    }

    /// Adds a C2PA manifest reference (specified as a URI, JUMBF or URL based)
    /// as XMP data to the stream, writing the result to the destination stream.
    ///
    /// # Remarks
    /// This method is considered a stop-gap for now until the official SDK
    /// offers a more generic method to indicate a document ID, instance ID,
    /// and a reference to the a remote manifest.
    #[allow(dead_code)]
    pub(crate) fn add_reference_as_xmp_to_stream<TSource, TDest>(
        source: &mut TSource,
        destination: &mut TDest,
        manifest_uri: &str,
    ) -> Result<()>
    where
        TSource: Read + Seek + ?Sized,
        TDest: Write + ?Sized,
    {
        // Build a simple XMP meta element from the current source stream
        let mut xmp_meta = match build_xmp_from_stream(source) {
            // Use the data already available
            Ok(meta) => meta,
            // If data was not found for building out the XMP, we will default
            // to some good starting points
            Err(FontError::XmpNotFound) => default_font_xmp_meta(None, None)?,
            // At this point, the font is considered to be invalid possibly
            Err(error) => return Err(error),
        };
        // Reset the source stream to the beginning
        source.seek(SeekFrom::Start(0))?;
        // Add the provenance to the XMP data.
        xmp_meta = add_provenance(&xmp_meta, manifest_uri)
            .map_err(|_e| FontError::XmpError("Unable to add provenance".to_string()))?;
        // Finally write the XMP data as a string to the stream
        add_reference_to_stream(source, destination, &xmp_meta)?;

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
    /// which should be deleted once the object is dropped.  Uses the specified
    /// base name for the temporary file.
    pub(crate) fn new(base_name: &Path) -> Result<Self> {
        let temp_dir = TempDir::new()?;
        let temp_dir_path = temp_dir.path();
        let path = temp_dir_path.join(
            base_name
                .file_name()
                .ok_or_else(|| FontError::BadParam("Invalid file name".to_string()))?,
        );
        let file = File::create(&path)?;
        Ok(Self {
            temp_dir,
            path: path.into(),
            file,
        })
    }

    /// Get the path of the temporary file
    pub(crate) fn get_path(&self) -> &Path {
        self.path.as_ref()
    }

    /// Get a mutable reference to the temporary file
    pub(crate) fn get_mut_file(&mut self) -> &mut File {
        &mut self.file
    }
}

/// Adds C2PA manifest store data to a font file (specified by path).
fn add_c2pa_to_font(font_path: &Path, manifest_store_data: &[u8]) -> Result<()> {
    process_file_with_streams(font_path, move |input_stream, temp_file| {
        // Add the C2PA data to the temp file
        add_c2pa_to_stream(input_stream, temp_file.get_mut_file(), manifest_store_data)
    })
}

/// Adds C2PA manifest store data to a font stream, writing the result to the
/// destination stream.
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
    let mut font = SfntFont::from_reader(source)?;
    let c2pa_record = UpdateContentCredentialRecord::builder()
        .with_content_credential(manifest_store_data.to_vec())
        .build();
    font.update_c2pa_record(c2pa_record)?;
    font.write(destination)?;
    Ok(())
}

/// Adds the manifest URI reference to the font at the given path.
#[allow(dead_code)]
fn add_reference_to_font(font_path: &Path, manifest_uri: &str) -> Result<()> {
    process_file_with_streams(font_path, move |input_stream, temp_file| {
        // Write the manifest URI to the stream
        add_reference_to_stream(input_stream, temp_file.get_mut_file(), manifest_uri)
    })
}

/// Adds the specified reference URI to the source data, writing the result to
/// the destination stream.
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
    let mut font = SfntFont::from_reader(source)?;
    let update_record = UpdateContentCredentialRecord::builder()
        .with_active_manifest_uri(manifest_uri.to_string())
        .build();
    font.update_c2pa_record(update_record)?;
    font.write(destination)?;
    Ok(())
}

/// Adds the required chunks to the source stream for supporting C2PA, if the
/// chunks are already present nothing is done.  Writes the resulting data to
/// the destination stream.
///
/// # Remarks
/// Neither streams are rewound before and/or after the operation, so it is up
/// to the caller.
fn add_required_chunks_to_stream<TReader, TWriter>(
    input_stream: &mut TReader,
    output_stream: &mut TWriter,
) -> Result<()>
where
    TReader: Read + Seek + ?Sized,
    TWriter: Read + Seek + ?Sized + Write,
{
    let mut font = SfntFont::from_reader(input_stream)?;
    // Read the font from the input stream
    if !font.has_c2pa() {
        font.add_c2pa_record(ContentCredentialRecord::default())?;
    }
    // Write the font to the output stream
    font.write(output_stream)?;
    Ok(())
}

/// Opens a BufReader for the given file path
fn open_bufreader_for_file(file_path: &Path) -> Result<BufReader<File>> {
    let file = File::open(file_path)?;
    Ok(BufReader::new(file))
}

/// Processes a font file (specified by path) by stream with the given callback.
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

/// Reads the C2PA manifest store reference from the font file (specified by
/// path).
#[allow(dead_code)]
fn read_reference_from_font(font_path: &Path) -> Result<Option<String>> {
    // open the font source
    let mut font_stream = open_bufreader_for_file(font_path)?;
    read_reference_from_stream(&mut font_stream)
}

/// Reads the C2PA manifest store reference from the stream.
#[allow(dead_code)]
fn read_reference_from_stream<TSource>(source: &mut TSource) -> Result<Option<String>>
where
    TSource: Read + Seek + ?Sized,
{
    match read_c2pa_from_stream(source) {
        Ok(c2pa_data) => Ok(c2pa_data.active_manifest_uri),
        Err(FontError::JumbfNotFound) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Remove the `C2PA` font table from the font file (specified by path).
fn remove_c2pa_from_font(font_path: &Path) -> Result<()> {
    process_file_with_streams(font_path, move |input_stream, temp_file| {
        // Remove the C2PA manifest store from the stream
        remove_c2pa_from_stream(input_stream, temp_file.get_mut_file())
    })
}

/// Remove the `C2PA` font table from the font data stream, writing to the
/// destination.
fn remove_c2pa_from_stream<TSource, TDest>(
    source: &mut TSource,
    destination: &mut TDest,
) -> Result<()>
where
    TSource: Read + Seek + ?Sized,
    TDest: Write + ?Sized,
{
    source.rewind()?;
    let mut font = SfntFont::from_reader(source)?;
    // Remove the C2PA record from the font
    font.remove_c2pa_record()?;
    // And write it to the destination stream
    font.write(destination)?;
    Ok(())
}

/// Removes the reference to the active manifest from the source stream, writing
/// to the destination. Returns an optional active manifest URI reference, if
/// one was present.
#[allow(dead_code)]
fn remove_reference_from_stream<TSource, TDest>(
    source: &mut TSource,
    destination: &mut TDest,
) -> Result<()>
where
    TSource: Read + Seek + ?Sized,
    TDest: Write + ?Sized,
{
    source.rewind()?;
    let mut font = SfntFont::from_reader(source)?;
    let update_record = UpdateContentCredentialRecord::builder()
        .without_active_manifest_uri()
        .build();
    font.update_c2pa_record(update_record)?;
    font.write(destination)?;
    Ok(())
}

/// Gets a collection of positions of hash objects from the reader which are to
/// be excluded from the hashing, used to omit from hashing.
fn get_object_locations_from_stream<T>(reader: &mut T) -> Result<Vec<HashObjectPositions>>
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

    // NOTE - This call is pointless when we already have a C2PA table, and
    // a bit silly-seeming when we don't?
    add_required_chunks_to_stream(reader, &mut output_stream)?;
    output_stream.rewind()?;

    // Build up the positions we will hand back to the caller
    let mut locations: Vec<HashObjectPositions> = Vec::new();

    // Which will be built up from the different chunks from the file
    for chunk in SfntFont::get_chunk_positions(&mut output_stream)? {
        if chunk.chunk_type().should_hash() {
            locations.push(HashObjectPositions {
                offset: chunk.offset(),
                length: chunk.length(),
                htype: HashBlockObjectType::Other,
            });
        } else {
            locations.push(HashObjectPositions {
                offset: chunk.offset(),
                length: chunk.length(),
                htype: HashBlockObjectType::Cai,
            });
        }
    }
    Ok(locations)
}

/// Reads the `C2PA` font table from the data stream, returning the `C2PA` font
/// table data
fn read_c2pa_from_stream<T: Read + Seek + ?Sized>(reader: &mut T) -> Result<TableC2PA> {
    let sfnt = SfntFont::from_reader(reader)?;
    // Convert all errors from the reader to a deserialization error.
    match sfnt.table(&FontTag::C2PA) {
        None => Err(FontError::JumbfNotFound),
        // If there is, return its `manifest_store` value.
        Some(NamedTable::C2PA(c2pa)) => Ok((*c2pa).clone()),
        // Yikes! Non-C2PA table with C2PA tag!
        Some(_) => Err(FontError::InvalidNamedTable("Non-C2PA table with C2PA tag")),
    }
}

/// SFNT implementation of the CAILoader trait.
impl CAIReader for SfntIO {
    fn read_cai(&self, asset_reader: &mut dyn CAIRead) -> crate::error::Result<Vec<u8>> {
        let c2pa_table = read_c2pa_from_stream(asset_reader).map_err(|e| match e {
            FontError::JumbfNotFound => Error::JumbfNotFound,
            _ => wrap_font_err(e),
        })?;
        match c2pa_table.manifest_store {
            Some(manifest_store) => Ok(manifest_store.to_vec()),
            _ => Err(Error::JumbfNotFound),
        }
    }

    fn read_xmp(&self, asset_reader: &mut dyn CAIRead) -> Option<String> {
        // Fonts have no XMP data.
        // BUT, for now we will pretend it does and read from the reference
        read_reference_from_stream(asset_reader).unwrap_or_default()
    }
}

/// SFNT implementations for the CAIWriter trait.
impl CAIWriter for SfntIO {
    fn write_cai(
        &self,
        input_stream: &mut dyn CAIRead,
        output_stream: &mut dyn CAIReadWrite,
        store_bytes: &[u8],
    ) -> crate::error::Result<()> {
        add_c2pa_to_stream(input_stream, output_stream, store_bytes).map_err(wrap_font_err)
    }

    fn get_object_locations_from_stream(
        &self,
        input_stream: &mut dyn CAIRead,
    ) -> crate::error::Result<Vec<HashObjectPositions>> {
        get_object_locations_from_stream(input_stream).map_err(wrap_font_err)
    }

    fn remove_cai_store_from_stream(
        &self,
        input_stream: &mut dyn CAIRead,
        output_stream: &mut dyn CAIReadWrite,
    ) -> crate::error::Result<()> {
        remove_c2pa_from_stream(input_stream, output_stream).map_err(wrap_font_err)
    }
}

/// SFNT implementations for the AssetIO trait.
impl AssetIO for SfntIO {
    fn new(_asset_type: &str) -> Self
    where
        Self: Sized,
    {
        SfntIO {}
    }

    fn get_handler(&self, asset_type: &str) -> Box<dyn AssetIO> {
        Box::new(SfntIO::new(asset_type))
    }

    fn get_reader(&self) -> &dyn CAIReader {
        self
    }

    fn get_writer(&self, asset_type: &str) -> Option<Box<dyn CAIWriter>> {
        Some(Box::new(SfntIO::new(asset_type)))
    }

    fn remote_ref_writer_ref(&self) -> Option<&dyn RemoteRefEmbed> {
        Some(self)
    }

    fn supported_types(&self) -> &[&str] {
        // Supported extension and mime-types
        &[
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
        ]
    }

    fn read_cai_store(&self, asset_path: &Path) -> crate::error::Result<Vec<u8>> {
        let mut f: File = File::open(asset_path)?;
        self.read_cai(&mut f)
    }

    fn save_cai_store(&self, asset_path: &Path, store_bytes: &[u8]) -> crate::error::Result<()> {
        add_c2pa_to_font(asset_path, store_bytes).map_err(wrap_font_err)
    }

    fn get_object_locations(
        &self,
        asset_path: &Path,
    ) -> crate::error::Result<Vec<HashObjectPositions>> {
        let mut buf_reader = open_bufreader_for_file(asset_path)?;
        get_object_locations_from_stream(&mut buf_reader).map_err(wrap_font_err)
    }

    fn remove_cai_store(&self, asset_path: &Path) -> crate::error::Result<()> {
        remove_c2pa_from_font(asset_path).map_err(wrap_font_err)
    }

    fn asset_box_hash_ref(&self) -> Option<&dyn AssetBoxHash> {
        Some(self)
    }
}

// Implementation for the asset box hash trait for general box hash support
impl AssetBoxHash for SfntIO {
    fn get_box_map(&self, input_stream: &mut dyn CAIRead) -> crate::error::Result<Vec<BoxMap>> {
        // The SDK doesn't necessarily promise the input stream is rewound, so do so
        // now to make sure we can parse the font.
        input_stream.rewind()?;

        // Get the chunk positions
        let chunks = SfntFont::get_chunk_positions(input_stream).map_err(FontError::from)?;
        // Create a box map vector to map the chunk positions to
        let mut box_maps = Vec::<BoxMap>::new();

        // Function to create and push a box map to the vector
        let create_and_push_box_map =
            |chunk: &ChunkPosition<SfntChunkType>, box_maps: &mut Vec<BoxMap>| {
                let name = chunk.name_as_string()?;
                // NOTE: The actual hash is not calculated here, as it is stated in the
                //       trait documentation that the hash is calculated by the caller.
                let box_map = BoxMap {
                    names: vec![name],
                    alg: None,
                    hash: ByteBuf::from(Vec::new()),
                    pad: ByteBuf::from(Vec::new()),
                    range_start: chunk.offset(),
                    range_len: chunk.length(),
                };
                box_maps.push(box_map);
                Ok::<(), FontError>(())
            };

        for chunk in chunks {
            // If the chunk type is to be included in the hash, then generate
            // and add the hash to the box map
            if chunk.chunk_type().should_hash() {
                create_and_push_box_map(&chunk, &mut box_maps)?;
            }
        }
        // Do not iterate if the log level is not set to at least trace
        if log::max_level().cmp(&log::LevelFilter::Trace).is_ge() {
            for (i, box_map) in box_maps.iter().enumerate() {
                trace!("get_box_map/boxes[{:02}]: {:?}", i, &box_map);
            }
        }
        Ok(box_maps)
    }
}

impl RemoteRefEmbed for SfntIO {
    #[allow(unused_variables)]
    fn embed_reference(
        &self,
        asset_path: &Path,
        embed_ref: crate::asset_io::RemoteRefEmbedType,
    ) -> crate::error::Result<()> {
        match embed_ref {
            crate::asset_io::RemoteRefEmbedType::Xmp(manifest_uri) => {
                #[cfg(feature = "font_xmp")]
                {
                    font_xmp_support::add_reference_as_xmp_to_font(asset_path, &manifest_uri)
                        .map_err(wrap_font_err)
                }
                #[cfg(not(feature = "font_xmp"))]
                {
                    add_reference_to_font(asset_path, &manifest_uri).map_err(wrap_font_err)
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
    ) -> crate::error::Result<()> {
        match embed_ref {
            crate::asset_io::RemoteRefEmbedType::Xmp(manifest_uri) => {
                #[cfg(feature = "font_xmp")]
                {
                    font_xmp_support::add_reference_as_xmp_to_stream(
                        reader,
                        output_stream,
                        &manifest_uri,
                    )
                    .map_err(wrap_font_err)
                }
                #[cfg(not(feature = "font_xmp"))]
                {
                    add_reference_to_stream(reader, output_stream, &manifest_uri)
                        .map_err(wrap_font_err)
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

    use std::num::Wrapping;

    use byteorder::{BigEndian, ByteOrder};
    use claims::*;
    use tempfile::tempdir;

    use super::*;
    use crate::utils::test::{fixture_path, temp_dir_path};
    /// Computes a 32-bit big-endian OpenType-style checksum on the given byte
    /// array, which is presumed to start on a 4-byte boundary.
    ///
    /// # Remarks
    /// Note that trailing pad bytes do not affect this checksum - it's not a real
    /// CRC.
    ///
    /// # Panics
    /// Panics if the the `bytes` array is not aligned on a 4-byte boundary.
    #[allow(dead_code)]
    pub(crate) fn checksum(bytes: &[u8]) -> Wrapping<u32> {
        // Cut your pie into 1x4cm pieces to serve
        let words = bytes.chunks_exact(size_of::<u32>());
        // ...and then any remainder...
        let frag_cksum: Wrapping<u32> = Wrapping(
            // (away, mayhap, with issue #32463)
            words
                .remainder()
                .iter()
                .fold(Wrapping(0_u32), |acc, byte| {
                    // At some point, it should be possible to:
                    // - Remove the `Wrapping(...)` surrounding the outer expression
                    // - Get rid of `.0` and just access plain `acc`
                    // - Get rid of `.0` down there getting applied to the end of
                    //   this .fold(), as well as
                    // - Get rid of the `Wrapping(...)` in this next expression
                    // but unfortunately as of this writing, attempting to call
                    // `.rotate_left` on a `Wrapping<u32>` fails:
                    //   use of unstable library feature 'wrapping_int_impl', see issue
                    //     #32463 <https://github.com/rust-lang/rust/issues/32463>
                    Wrapping(acc.0.rotate_left(u8::BITS) + *byte as u32)
                })
                .0 // (goes away, mayhap, when issue #32463 is done)
                .rotate_left(u8::BITS * (size_of::<u32>() - words.remainder().len()) as u32),
        );
        // Sum all the exact chunks...
        let chunks_cksum: Wrapping<u32> = words
            .fold(Wrapping(0_u32), |running_cksum, exact_chunk| {
                running_cksum + Wrapping(BigEndian::read_u32(exact_chunk))
            });
        // Combine ingredients & serve.
        chunks_cksum + frag_cksum
    }

    #[test]
    #[cfg(not(target_os = "wasi"))]
    fn get_object_locations_c2pa_absent() {
        // Load the basic OTF test fixture
        let source = fixture_path("font.otf");

        // Create a temporary output for the file
        let temp_dir = tempdir().unwrap();
        let output = temp_dir_path(&temp_dir, "test.otf");

        // Copy the source to the output
        std::fs::copy(source, &output).unwrap();

        // Create our SfntIO asset handler for testing
        let sfnt_io = SfntIO {};

        // We expect 15 "objects"
        // 0. The "header" + "directory"
        // 1. The first 8 bytes of the `head` table, as "Other"
        // 2. The 4-byte checksumAdjustment field, as "Cai"
        // 3. The rest of the `head` table.
        // 4-13. The other tables in the font.
        // 14. The C2PA table, automatically added when the font is deserialized
        let positions = sfnt_io.get_object_locations(&output).unwrap();
        assert_eq!(15, positions.len());
    }

    #[test]
    #[cfg(not(target_os = "wasi"))]
    fn get_object_locations_c2pa_present() {
        // Load the basic OTF test fixture
        let source = fixture_path("font_c2pa.otf");

        // Create a temporary output for the file
        let temp_dir = tempdir().unwrap();
        let output = temp_dir_path(&temp_dir, "test.otf");

        // Copy the source to the output
        std::fs::copy(source, &output).unwrap();

        // Create our SfntIO asset handler for testing
        let sfnt_io = SfntIO {};

        // We expect 15 "objects"
        // 0. The "header" + "directory"
        // 1. The first 8 bytes of the `head` table, as "Other"
        // 2. The 4-byte checksumAdjustment field, as "Cai"
        // 3. The rest of the `head` table.
        // 4-13. The other tables in the font.
        // 14. The C2PA table, automatically added when the font is deserialized
        //
        // Note that we also flip on Trace logging, to test those paths.
        let saved_log_level = log::max_level();
        log::set_max_level(log::LevelFilter::Trace);
        let positions = sfnt_io.get_object_locations(&output).unwrap();
        log::set_max_level(saved_log_level);
        assert_eq!(15, positions.len());
    }

    #[test]
    #[cfg(all(not(feature = "font_xmp"), not(target_os = "wasi")))]
    /// Verifies the adding of a remote C2PA manifest reference works as
    /// expected.
    fn add_c2pa_ref() {
        let c2pa_data = "test data";

        // Load the basic OTF test fixture
        let source = fixture_path("font.otf");

        // Create a temporary output for the file
        let temp_dir = tempdir().unwrap();
        let output = temp_dir_path(&temp_dir, "test.otf");

        // Copy the source to the output
        std::fs::copy(source, &output).unwrap();

        // Create our SfntIO asset handler for testing
        let sfnt_io = SfntIO {};

        let expected_manifest_uri = "https://test/ref";

        sfnt_io
            .embed_reference(
                &output,
                crate::asset_io::RemoteRefEmbedType::Xmp(expected_manifest_uri.to_owned()),
            )
            .unwrap();
        // Save the C2PA manifest store to the file
        sfnt_io
            .save_cai_store(&output, c2pa_data.as_bytes())
            .unwrap();
        // Loading it back from the same output file
        let loaded_c2pa = sfnt_io.read_cai_store(&output).unwrap();
        // Which should work out to be the same in the end
        assert_eq!(&loaded_c2pa, c2pa_data.as_bytes());

        match read_reference_from_font(&output) {
            Ok(Some(manifest_uri)) => assert_eq!(expected_manifest_uri, manifest_uri),
            _ => panic!("Expected to read a reference from the font file"),
        };

        let output_data = std::fs::read(&output).unwrap();
        assert_eq!(checksum(&output_data).0, SFNT_EXPECTED_CHECKSUM);
    }

    #[test]
    #[cfg(all(feature = "font_xmp", not(target_os = "wasi")))]
    /// Verifies the adding of a remote C2PA manifest reference as XMP works as
    /// expected.
    fn add_c2pa_ref() {
        use crate::utils::xmp_inmemory_utils::extract_provenance;

        let c2pa_data = "test data";

        // Load the basic OTF test fixture
        let source = fixture_path("font.otf");

        // Create a temporary output for the file
        let temp_dir = tempdir().unwrap();
        let output = temp_dir_path(&temp_dir, "test.otf");

        // Copy the source to the output
        std::fs::copy(source, &output).unwrap();

        // Create our SfntIO asset handler for testing
        let sfnt_io = SfntIO {};

        let expected_manifest_uri = "https://test/ref";

        sfnt_io
            .embed_reference(
                &output,
                crate::asset_io::RemoteRefEmbedType::Xmp(expected_manifest_uri.to_owned()),
            )
            .unwrap();
        // Save the C2PA manifest store to the file
        sfnt_io
            .save_cai_store(&output, c2pa_data.as_bytes())
            .unwrap();
        // Loading it back from the same output file
        let loaded_c2pa = sfnt_io.read_cai_store(&output).unwrap();
        // Which should work out to be the same in the end
        assert_eq!(&loaded_c2pa, c2pa_data.as_bytes());

        match read_reference_from_font(&output) {
            Ok(Some(manifest_uri)) => {
                let provenance = extract_provenance(manifest_uri.as_str()).unwrap();
                assert_eq!(expected_manifest_uri, provenance);
            }
            _ => panic!("Expected to read a reference from the font file"),
        };

        let output_data = std::fs::read(&output).unwrap();
        assert_eq!(checksum(&output_data).0, SFNT_EXPECTED_CHECKSUM);
    }

    #[test]
    /// Verify read/write idempotency
    fn read_write_idempotent_no_c2pa() {
        // Load the basic OTF test fixture
        let mut font_stream = File::open(fixture_path("font.otf")).unwrap();
        // Read & build
        let mut sfnt = SfntFont::from_reader(&mut font_stream).unwrap();
        // Then serialize back out
        let mut test_data = Vec::new();
        sfnt.write(&mut test_data).unwrap();
        // and read _that_ back in...
        let mut font_data = Vec::new();
        font_stream.rewind().unwrap();
        font_stream.read_to_end(&mut font_data).unwrap();
        // data should match & checksum should be right
        assert_eq!(font_data, test_data);
        let naive_test_cksum = checksum(&test_data).0;
        assert_eq!(naive_test_cksum, SFNT_EXPECTED_CHECKSUM);
    }

    #[test]
    /// Verify read/write idempotency
    fn read_write_idempotent_yes_c2pa() {
        // Load the basic OTF test fixture
        let mut font_stream = File::open(fixture_path("font_c2pa.otf")).unwrap();
        // Read & build
        let mut sfnt = SfntFont::from_reader(&mut font_stream).unwrap();
        // Then serialize back out
        let mut test_data = Vec::new();
        sfnt.write(&mut test_data).unwrap();
        // and read _that_ back in...
        let mut font_data = Vec::new();
        font_stream.rewind().unwrap();
        font_stream.read_to_end(&mut font_data).unwrap();
        // data should match & checksum should be right
        assert_eq!(font_data, test_data);
        let naive_test_cksum = checksum(&test_data).0;
        assert_eq!(naive_test_cksum, SFNT_EXPECTED_CHECKSUM);
    }

    #[test]
    /// Try to read a font with an invalid table offset
    fn sfnt_from_reader_bad_offset() {
        let font_data = vec![
            0x4f, 0x54, 0x54, 0x4f, // OTTO - OpenType tag
            0x00, 0x01, // 1 table
            0x00, 0x00, // search range
            0x00, 0x00, // entry selector
            0x00, 0x00, // range shift
            0x43, 0x32, 0x50, 0x41, // C2PA table tag
            0x00, 0x00, 0x00, 0x00, // Checksum
            0x0f, 0x00, 0x00, 0x1c, // offset to table data
            0x00, 0x00, 0x00, 0x25, // length of table data
            0x00, 0x00, // Major version
            0x00, 0x01, // Minor version
            0x00, 0x00, 0x00, 0x14, // Active manifest URI offset
            0x00, 0x08, // Active manifest URI length
            0x00, 0x00, // reserved
            0x00, 0x00, 0x00, 0x1c, // C2PA manifest store offset
            0x00, 0x00, 0x00, 0x09, // C2PA manifest store length
            0x66, 0x69, 0x6c, 0x65, 0x3a, 0x2f, 0x2f,
            0x61, // active manifest uri data (e.g., file://a)
            0x74, 0x65, 0x73, 0x74, 0x2d, 0x64, 0x61, 0x74, 0x61, // C2PA manifest store data
        ];
        let mut font_stream: Cursor<&[u8]> = Cursor::<&[u8]>::new(&font_data);
        // Read & build
        let result = SfntFont::from_reader(&mut font_stream);
        assert!(result.is_err());
    }

    #[test]
    /// Try to read a font truncated in the header
    fn sfnt_from_reader_trunc_header() {
        let font_data = vec![
            0x4f, 0x54, 0x54, 0x4f, // OTTO - OpenType tag
            0x00, 0x01, // 1 table
            0x00, 0x00, // search range
            0x00, 0x00, // entry selector
            0x00, // range sh...
        ];
        let mut font_stream: Cursor<&[u8]> = Cursor::<&[u8]>::new(&font_data);
        // Read & build
        let result = SfntFont::from_reader(&mut font_stream);
        assert!(result.is_err());
    }

    #[test]
    /// Try to read a font truncated in the directory
    fn sfnt_from_reader_trunc_directory() {
        let font_data = vec![
            0x4f, 0x54, 0x54, 0x4f, // OTTO - OpenType tag
            0x00, 0x01, // 1 table
            0x00, 0x00, // search range
            0x00, 0x00, // entry selector
            0x00, 0x00, // range shift
            0x43, 0x32, 0x50, 0x41, // C2PA table tag
            0x00, 0x00, 0x00, 0x00, // Checksum
            0x0f, 0x00, 0x00, 0x1c, // offset to table data
            0x00, 0x00, 0x00, // length of table da...
        ];
        let mut font_stream: Cursor<&[u8]> = Cursor::<&[u8]>::new(&font_data);
        // Read & build
        let result = SfntFont::from_reader(&mut font_stream);
        assert!(result.is_err());
    }

    #[test]
    /// Try to read a font truncated in the C2PA table's prologue
    fn sfnt_from_reader_trunc_c2pa_table_prologue() {
        let font_data = vec![
            0x4f, 0x54, 0x54, 0x4f, // OTTO - OpenType tag
            0x00, 0x01, // 1 table
            0x00, 0x00, // search range
            0x00, 0x00, // entry selector
            0x00, 0x00, // range shift
            0x43, 0x32, 0x50, 0x41, // C2PA table tag
            0x00, 0x00, 0x00, 0x00, // Checksum
            0x00, 0x00, 0x00, 0x1c, // offset to table data
            0x00, 0x00, 0x00, 0x25, // length of table data
            0x00, 0x00, // Major version
            0x00, 0x01, // Minor version
            0x00, 0x00, 0x00, 0x14, // Active manifest URI offset
            0x00, 0x08, // Active manifest URI length
            0x00, // reserv...
        ];
        let mut font_stream: Cursor<&[u8]> = Cursor::<&[u8]>::new(&font_data);
        // Read & build
        let result = SfntFont::from_reader(&mut font_stream);
        assert!(result.is_err());
    }

    #[test]
    /// Try to read a font truncated in the C2PA table's uri storage
    fn sfnt_from_reader_trunc_c2pa_table_uri() {
        let font_data = vec![
            0x4f, 0x54, 0x54, 0x4f, // OTTO - OpenType tag
            0x00, 0x01, // 1 table
            0x00, 0x00, // search range
            0x00, 0x00, // entry selector
            0x00, 0x00, // range shift
            0x43, 0x32, 0x50, 0x41, // C2PA table tag
            0x00, 0x00, 0x00, 0x00, // Checksum
            0x00, 0x00, 0x00, 0x1c, // offset to table data
            0x00, 0x00, 0x00, 0x25, // length of table data
            0x00, 0x00, // Major version
            0x00, 0x01, // Minor version
            0x00, 0x00, 0x00, 0x14, // Active manifest URI offset
            0x00, 0x08, // Active manifest URI length
            0x00, 0x00, // reserved
            0x00, 0x00, 0x00, 0x1c, // C2PA manifest store offset
            0x00, 0x00, 0x00, 0x09, // C2PA manifest store length
            0x66, 0x69, 0x6c, 0x65, 0x3a, 0x2f, 0x2f,
            // Partial active manifest uri data
        ];
        let mut font_stream: Cursor<&[u8]> = Cursor::<&[u8]>::new(&font_data);
        // Read & build
        let result = SfntFont::from_reader(&mut font_stream);
        assert!(result.is_err());
    }

    #[test]
    /// Try to read a font truncated in the C2PA table's manifest storage
    fn sfnt_from_reader_trunc_c2pa_table_manifest_store() {
        let font_data = vec![
            0x4f, 0x54, 0x54, 0x4f, // OTTO - OpenType tag
            0x00, 0x01, // 1 table
            0x00, 0x00, // search range
            0x00, 0x00, // entry selector
            0x00, 0x00, // range shift
            0x43, 0x32, 0x50, 0x41, // C2PA table tag
            0x00, 0x00, 0x00, 0x00, // Checksum
            0x00, 0x00, 0x00, 0x1c, // offset to table data
            0x00, 0x00, 0x00, 0x25, // length of table data
            0x00, 0x00, // Major version
            0x00, 0x01, // Minor version
            0x00, 0x00, 0x00, 0x14, // Active manifest URI offset
            0x00, 0x08, // Active manifest URI length
            0x00, 0x00, // reserved
            0x00, 0x00, 0x00, 0x1c, // C2PA manifest store offset
            0x00, 0x00, 0x00, 0x09, // C2PA manifest store length
            0x66, 0x69, 0x6c, 0x65, 0x3a, 0x2f, 0x2f,
            0x61, // active manifest uri data (e.g., file://a)
            0x74, 0x65, 0x73, 0x74, 0x2d, 0x64, 0x61, // Partial manifest data
        ];
        let mut font_stream: Cursor<&[u8]> = Cursor::<&[u8]>::new(&font_data);
        // Read & build
        let result = SfntFont::from_reader(&mut font_stream);
        assert!(result.is_err());
    }

    #[test]
    /// Verify the C2PA table data can be read from a font stream
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
            0x00, 0x00, 0x00, 0x25, // length of table data
            0x00, 0x00, // Major version
            0x00, 0x01, // Minor version
            0x00, 0x00, 0x00, 0x14, // Active manifest URI offset
            0x00, 0x08, // Active manifest URI length
            0x00, 0x00, // reserved
            0x00, 0x00, 0x00, 0x1c, // C2PA manifest store offset
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
            c2pa_data.manifest_store.as_deref()
        );
    }

    #[test]
    #[cfg(not(target_os = "wasi"))]
    /// Verifies the ability to write/read C2PA manifest store data to/from an
    /// OpenType font
    fn remove_c2pa_manifest_store() {
        let c2pa_data = "test data";

        // Load the basic OTF test fixture
        let source = fixture_path("font.otf");

        // Create a temporary output for the file
        let temp_dir = tempdir().unwrap();
        let output = temp_dir_path(&temp_dir, "test.otf");

        // Copy the source to the output
        std::fs::copy(source, &output).unwrap();

        // Create our SfntIO asset handler for testing
        let sfnt_io = SfntIO {};

        // Save the C2PA manifest store to the file
        sfnt_io
            .save_cai_store(&output, c2pa_data.as_bytes())
            .unwrap();
        // Loading it back from the same output file
        let loaded_c2pa = sfnt_io.read_cai_store(&output).unwrap();
        // Which should work out to be the same in the end
        assert_eq!(&loaded_c2pa, c2pa_data.as_bytes());

        sfnt_io.remove_cai_store(&output).unwrap();
        match sfnt_io.read_cai_store(&output) {
            Err(Error::JumbfNotFound) => (),
            _ => panic!("Should not contain any C2PA data"),
        };
    }

    #[test]
    #[cfg(not(target_os = "wasi"))]
    /// Verifies the ability to write/read C2PA manifest store data to/from an
    /// OpenType font
    fn write_read_c2pa_from_font() {
        let c2pa_data = "test data";

        // Load the basic OTF test fixture
        let source = fixture_path("font.otf");

        // Create a temporary output for the file
        let temp_dir = tempdir().unwrap();
        let output = temp_dir_path(&temp_dir, "test.otf");

        // Copy the source to the output
        std::fs::copy(source, &output).unwrap();

        // Create our SfntIO asset handler for testing
        let sfnt_io = SfntIO {};

        // Save the C2PA manifest store to the file
        sfnt_io
            .save_cai_store(&output, c2pa_data.as_bytes())
            .unwrap();
        // Loading it back from the same output file
        let loaded_c2pa = sfnt_io.read_cai_store(&output).unwrap();
        // Which should work out to be the same in the end
        assert_eq!(&loaded_c2pa, c2pa_data.as_bytes());
    }

    #[cfg(feature = "font_xmp")]
    #[cfg(test)]
    pub mod font_xmp_support_tests {
        use std::{fs::File, io::Cursor, path::Path};

        use claims::*;
        use tempfile::tempdir;

        use super::{process_file_with_streams, remove_reference_from_stream};
        use crate::{
            asset_handlers::{
                font_io::FontError,
                sfnt_io::{font_xmp_support, SfntIO},
            },
            asset_io::CAIReader,
            utils::{test::temp_dir_path, xmp_inmemory_utils::extract_provenance},
        };

        /// Remove any remote manifest reference from any `C2PA` font table which
        /// exists in the given font file (specified by path).
        #[allow(dead_code)]
        fn remove_reference_from_font(font_path: &Path) -> Result<(), FontError> {
            process_file_with_streams(font_path, move |input_stream, temp_file| {
                remove_reference_from_stream(input_stream, temp_file.get_mut_file())?;
                Ok(())
            })
        }

        #[test]
        #[cfg(not(target_os = "wasi"))]
        /// Verifies the `font_xmp_support::add_reference_as_xmp_to_stream` is
        /// able to add a reference to as XMP when there is already data in the
        /// reference field.
        fn add_reference_as_xmp_to_stream_with_data() {
            // Load the basic OTF test fixture
            let source = crate::utils::test::fixture_path("font.otf");

            // Create a temporary output for the file
            let temp_dir = tempdir().unwrap();
            let output = temp_dir_path(&temp_dir, "test.otf");

            // Copy the source to the output
            std::fs::copy(source, &output).unwrap();

            // Add a reference to the font
            assert_ok!(font_xmp_support::add_reference_as_xmp_to_font(
                &output,
                "test data"
            ));

            // Add again, with a new value
            assert_ok!(font_xmp_support::add_reference_as_xmp_to_font(
                &output,
                "new test data"
            ));
            let otf_handler = SfntIO {};
            // Verify the reference was updated
            {
                let mut f: File = File::open(&output).unwrap();
                match otf_handler.read_xmp(&mut f) {
                    Some(xmp_data_str) => {
                        let xmp_value = extract_provenance(xmp_data_str.as_str()).unwrap();
                        assert_eq!("new test data", xmp_value);
                    }
                    None => panic!("Expected to read XMP from the resource."),
                }
            }
            // Remove the reference
            assert_ok!(remove_reference_from_font(&output));
            // Verify the reference was removed
            {
                let mut f: File = File::open(&output).unwrap();
                assert_none!(otf_handler.read_xmp(&mut f));
            }
        }

        #[test]
        /// Verifies the `font_xmp_support::build_xmp_from_stream` method
        /// correctly returns error for NotFound when there is no data in the
        /// stream to return.
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
            // Note - We could improve code-coverage here (eliminating all these
            // match-arms) if we could just do:
            //   assert_err_eq!(font_xmp_support::build_xmp_from_stream(&mut font_stream), Err(FontError::XmpNotFound));
            // but that requires adding PartialOrd to FontError...
            match font_xmp_support::build_xmp_from_stream(&mut font_stream) {
                Ok(_) => panic!("Did not expect an OK result, as data is missing"),
                Err(FontError::XmpNotFound) => {}
                Err(_) => panic!("Unexpected error when building XMP data"),
            }
        }

        #[test]
        /// Verifies the `font_xmp_support::build_xmp_from_stream` method
        /// correctly returns error for NotFound when there is no data in the
        /// stream to return.
        fn build_xmp_from_stream_with_reference_not_xmp() {
            let font_data = vec![
                0x4f, 0x54, 0x54, 0x4f, // OTTO - OpenType tag
                0x00, 0x01, // 1 tables
                0x00, 0x00, // search range
                0x00, 0x00, // entry selector
                0x00, 0x00, // range shift
                0x43, 0x32, 0x50, 0x41, // C2PA table tag
                0x00, 0x00, 0x00, 0x00, // Checksum
                0x00, 0x00, 0x00, 0x1c, // offset to table data
                0x00, 0x00, 0x00, 0x1c, // length of table data
                0x00, 0x00, // Major version
                0x00, 0x01, // Minor version
                0x00, 0x00, 0x00, 0x14, // Active manifest URI offset
                0x00, 0x08, // Active manifest URI length
                0x00, 0x00, // reserved
                0x00, 0x00, 0x00, 0x00, // C2PA manifest store offset
                0x00, 0x00, 0x00, 0x00, // C2PA manifest store length
                0x66, 0x69, 0x6c, 0x65, 0x3a, 0x2f, 0x2f, 0x61, // active manifest uri data
            ];
            let mut font_stream: Cursor<&[u8]> = Cursor::<&[u8]>::new(&font_data);
            assert_ok!(font_xmp_support::build_xmp_from_stream(&mut font_stream));
        }
    }

    #[test]
    fn test_write_cai_using_stream_existing_cai_data() {
        let source = include_bytes!("../../tests/fixtures/font_c2pa.otf");
        let mut stream = Cursor::new(source.to_vec());
        let sfnt_io = SfntIO {};

        // cai data already exists
        assert!(matches!(
            sfnt_io.read_cai(&mut stream),
            Ok(data) if !data.is_empty(),
        ));

        // write new data
        let output: Vec<u8> = Vec::new();
        let mut output_stream = Cursor::new(output);
        let data_to_write: Vec<u8> = vec![0, 1, 1, 2, 3, 5, 8, 34, 21, 23];
        assert!(sfnt_io
            .write_cai(&mut stream, &mut output_stream, &data_to_write)
            .is_ok());
        // new data replaces the existing cai data
        assert_ok!(output_stream.rewind()); // <- Why is this rewind needed? sfnt_io tests don't need to do this...
        let data_written = sfnt_io.read_cai(&mut output_stream).unwrap();
        assert_eq!(data_to_write, data_written);
    }

    #[test]
    fn test_write_cai_using_stream_no_cai_data() {
        let source = include_bytes!("../../tests/fixtures/font.otf");
        let mut stream = Cursor::new(source.to_vec());
        let sfnt_io = SfntIO {};

        // no cai data present in stream.
        assert!(matches!(
            sfnt_io.read_cai(&mut stream),
            Err(Error::JumbfNotFound)
        ));

        // write new data.
        let output: Vec<u8> = Vec::new();
        let mut output_stream = Cursor::new(output);

        let data_to_write: Vec<u8> = vec![0, 1, 1, 23, 3, 5, 8, 1, 21, 34];
        assert!(sfnt_io
            .write_cai(&mut stream, &mut output_stream, &data_to_write)
            .is_ok());

        // assert new cai data is present.
        assert_ok!(output_stream.rewind());
        let data_written = sfnt_io.read_cai(&mut output_stream).unwrap();
        assert_eq!(data_to_write, data_written);
    }

    #[test]
    fn test_write_cai_data_to_stream_wrong_format() {
        let source = include_bytes!("../../tests/fixtures/mars.webp");
        let mut stream = Cursor::new(source.to_vec());
        let sfnt_io = SfntIO {};

        let output: Vec<u8> = Vec::new();
        let mut output_stream = Cursor::new(output);
        assert!(matches!(
            sfnt_io.write_cai(&mut stream, &mut output_stream, &[]),
            // TBD - Should we be mapping these kinds of failures to
            // Error::InvalidAsset(_), as, for example, the PNG asset
            // code does?
            Err(Error::FontError(_)),
        ));
    }

    #[test]
    fn test_stream_object_locations() {
        let source = include_bytes!("../../tests/fixtures/font_c2pa.otf");
        let mut stream = Cursor::new(source.to_vec());
        let sfnt_io = SfntIO {};
        let cai_posns = sfnt_io
            .get_object_locations_from_stream(&mut stream)
            .unwrap();
        assert_eq!(cai_posns.len(), 15);
    }

    #[test]
    fn test_stream_object_locations_with_incorrect_file_type() {
        let source = include_bytes!("../../tests/fixtures/unsupported_type.txt");
        let mut stream = Cursor::new(source.to_vec());
        let sfnt_io = SfntIO {};
        assert!(matches!(
            sfnt_io.get_object_locations_from_stream(&mut stream),
            Err(Error::FontError(_))
        ));
    }

    #[test]
    fn test_stream_object_locations_adds_offsets_to_file_without_claims() {
        let source = include_bytes!("../../tests/fixtures/font.otf");
        let mut stream = Cursor::new(source.to_vec());

        let sfnt_io = SfntIO {};
        assert!(sfnt_io
            .get_object_locations_from_stream(&mut stream)
            .unwrap()
            .into_iter()
            .any(|chunk| chunk.htype == HashBlockObjectType::Cai));
    }

    #[test]
    #[cfg(not(target_os = "wasi"))]
    fn test_remove_c2pa() {
        let source = fixture_path("font_c2pa.otf");
        let temp_dir = tempfile::tempdir().unwrap();
        let output = temp_dir_path(&temp_dir, "font_c2pa_removed.otf");
        std::fs::copy(source, &output).unwrap();

        let sfnt_io = SfntIO {};
        sfnt_io.remove_cai_store(&output).unwrap();

        // read back in asset, JumbfNotFound is expected since it was removed
        match sfnt_io.read_cai_store(&output) {
            Err(Error::JumbfNotFound) => (),
            _ => unreachable!(),
        }
    }

    #[test]
    fn test_remove_c2pa_from_stream() {
        let source = fixture_path("font_c2pa.otf");

        let source_bytes = std::fs::read(source).unwrap();
        let mut source_stream = Cursor::new(source_bytes);

        let sfnt_io = SfntIO {};
        let sfnt_writer = sfnt_io.get_writer("ttf").unwrap();

        let output_bytes = Vec::new();
        let mut output_stream = Cursor::new(output_bytes);

        sfnt_writer
            .remove_cai_store_from_stream(&mut source_stream, &mut output_stream)
            .unwrap();

        // read back in asset, JumbfNotFound is expected since it was removed
        let sfnt_reader = sfnt_io.get_reader();
        assert_ok!(output_stream.rewind());
        match sfnt_reader.read_cai(&mut output_stream) {
            Err(Error::JumbfNotFound) => (),
            _ => unreachable!(),
        }
    }
}
