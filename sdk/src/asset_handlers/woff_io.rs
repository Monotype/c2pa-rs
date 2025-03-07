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
    fs::File,
    io::{BufReader, Read, Seek, Write},
    path::*,
};

use c2pa_font_handler::{chunks::ChunkReader, woff1::font::Woff1Font, FontDataRead};
use serde_bytes::ByteBuf;
use tempfile::TempDir;
use uuid::Uuid;

use crate::{
    assertions::BoxMap,
    asset_handlers::font_io::*,
    asset_io::{
        AssetBoxHash, AssetIO, CAIRead, CAIReadWrite, CAIReader, CAIWriter, HashObjectPositions,
        RemoteRefEmbed, RemoteRefEmbedType,
    },
    error::Error,
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
/// # Remarks
/// This module depends on the `feature = "xmp_write"` to be enabled.
#[cfg(feature = "xmp_write")]
mod font_xmp_support {
    use super::*;
    use crate::utils::xmp_inmemory_utils::{add_provenance, add_xmp_key, MIN_XMP};

    /// Creates a default `XmpMeta` object for fonts, using the supplied
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
        let xmp = MIN_XMP.to_string();

        // Add in the namespace for DocumentID and InstanceID.
        add_xmp_key(&xmp, "xmlns:xmpMM", "http://ns.adobe.com/xap/1.0/mm/").map_err(|_e| {
            FontError::XmpError("Unable to add media management namespace".to_string())
        })?;

        // Add in DocumentID and InstanceID.
        add_xmp_key(
            &xmp,
            "xmpMM:DocumentID",
            document_id.unwrap_or(Uuid::new_v4().to_string()).as_str(),
        )
        .map_err(|_e| FontError::XmpError("Unable to add DocumentID".to_string()))?;
        add_xmp_key(
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
        use std::io::SeekFrom;
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
    _destination: &mut TDest,
    _manifest_store_data: &[u8],
) -> Result<()>
where
    TSource: Read + Seek + ?Sized,
    TDest: Write + ?Sized,
{
    source.rewind()?;
    todo!("C2PA record update not supported in crate yet");
    /*
    let mut font = Woff1Font::from_reader(source)?;
    let c2pa_record = UpdateContentCredentialRecord::builder()
        .with_content_credential(manifest_store_data.to_vec())
        .build();
    font.update_c2pa_record(c2pa_record);
    font.write(destination)?;
    Ok(())
    */
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
    _destination: &mut TDest,
    _manifest_uri: &str,
) -> Result<()>
where
    TSource: Read + Seek + ?Sized,
    TDest: Write + ?Sized,
{
    source.rewind()?;
    todo!("C2PA record update not supported in crate yet");
    /*
    let mut font = Woff1Font::from_reader(source)?;
    let c2pa_record = UpdateContentCredentialRecord::builder()
        .with_active_manifest_uri(manifest_uri.to_string())
        .build();
    font.update_c2pa_record(c2pa_record);
    font.write(destination)?;
    Ok(())
    */
}

/// Adds the required chunks to the source stream for supporting C2PA, if the
/// chunks are already present nothing is done.  Writes the resulting data to
/// the destination stream.
///
/// # Remarks
/// Neither streams are rewound before and/or after the operation, so it is up
/// to the caller.
#[allow(dead_code)]
fn add_required_chunks_to_stream<TReader, TWriter>(
    _input_stream: &mut TReader,
    _output_stream: &mut TWriter,
) -> Result<()>
where
    TReader: Read + Seek + ?Sized,
    TWriter: Read + Seek + ?Sized + Write,
{
    todo!("C2PA record update not supported in crate yet");
    /*
    let mut font = Woff1Font::from_reader(input_stream)?;
    if !font.has_c2pa() {
        font.add_c2pa_record(ContentCredentialRecord::default())?;
    }
    font.write(output_stream)?;
    Ok(())
    */
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
    _destination: &mut TDest,
) -> Result<()>
where
    TSource: Read + Seek + ?Sized,
    TDest: Write + ?Sized,
{
    source.rewind()?;
    todo!("C2PA record update not supported in crate yet");
    /*
    // Load the font from the stream
    let mut font = Woff1Font::from_reader(source)?;
    // Remove the table from the collection
    font.remove_c2pa_record()?;
    // And write it to the destination stream
    font.write(destination)?;
    Ok(())
    */
}

/// Removes the reference to the active manifest from the source stream, writing
/// to the destination.  Returns an optional active manifest URI reference, if
/// there was one.
#[allow(dead_code)]
fn remove_reference_from_stream<TSource, TDest>(
    source: &mut TSource,
    _destination: &mut TDest,
) -> Result<()>
where
    TSource: Read + Seek + ?Sized,
    TDest: Write + ?Sized,
{
    source.rewind()?;
    todo!("C2PA record update not supported in crate yet");
    /*
    let mut font = Woff1Font::from_reader(source)?;
    let update_record = UpdateContentCredentialRecord::builder()
        .without_active_manifest_uri()
        .build();
    font.update_c2pa_record(update_record)?;
    font.write(destination)?;
    Ok(())
    */
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
    todo!()
    /*
    // We must take into account a font that may not have a C2PA table in it at
    // this point, adding any required chunks needed for C2PA to work correctly.
    let output_vec: Vec<u8> = Vec::new();
    let mut output_stream = Cursor::new(output_vec);
    add_required_chunks_to_stream(reader, &mut output_stream)?;
    output_stream.rewind()?;

    // Build up the positions we will hand back to the caller
    let mut positions: Vec<HashObjectPositions> = Vec::new();

    // Which will be built up from the different chunks from the file
    let chunk_positions = Woff1Font::get_chunk_positions(&mut output_stream)?;
    for chunk_position in chunk_positions {
        todo!()
    }
    Ok(positions)
    */
}

/// Reads the `C2PA` font table from the data stream, returning the `C2PA` font
/// table data
fn read_c2pa_from_stream<T: Read + Seek + ?Sized>(
    reader: &mut T,
) -> Result<c2pa_font_handler::sfnt::table::TableC2PA> {
    // Convert all errors from the reader to a deserialization error.
    let _woff = Woff1Font::from_reader(reader)?;
    todo!("C2PA record update not supported in crate yet; will be updated to WOFF1 C2PA table");
}

/// Main WOFF IO feature.
pub(crate) struct WoffIO {}

impl WoffIO {
    #[allow(dead_code)]
    pub(crate) fn default_document_id() -> String {
        format!("fontsoftware:did:{}", Uuid::new_v4())
    }

    #[allow(dead_code)]
    pub(crate) fn default_instance_id() -> String {
        format!("fontsoftware:iid:{}", Uuid::new_v4())
    }
}

/// WOFF implementation of the CAILoader trait.
impl CAIReader for WoffIO {
    fn read_cai(&self, _asset_reader: &mut dyn CAIRead) -> crate::error::Result<Vec<u8>> {
        todo!("C2PA record update not supported in crate yet");
        /*
        let mut font = Woff1Font::from_reader(asset_reader)?;
        if let Some(record) = font.get_c2pa_record() {
            Ok(record.get_content_credential().to_vec())
        } else {
            Err(FontError::JumbfNotFound.into())
        }
        */
    }

    fn read_xmp(&self, asset_reader: &mut dyn CAIRead) -> Option<String> {
        // Fonts have no XMP data.
        // BUT, for now we will pretend it does and read from the reference
        read_reference_from_stream(asset_reader).unwrap_or_default()
    }
}

/// WOFF/TTF implementations for the CAIWriter trait.
impl CAIWriter for WoffIO {
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
        // Supported extension and mime-types
        &[
            "application/font-woff",
            "application/x-font-woff",
            "font/woff",
            "woff",
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
impl AssetBoxHash for WoffIO {
    fn get_box_map(&self, input_stream: &mut dyn CAIRead) -> crate::error::Result<Vec<BoxMap>> {
        // Get the chunk positions
        let positions = Woff1Font::get_chunk_positions(input_stream)
            .map_err(FontError::from)
            .map_err(wrap_font_err)?;
        // Create a box map vector to map the chunk positions to
        let mut box_maps = Vec::<BoxMap>::new();
        for position in positions {
            let box_map = BoxMap {
                names: vec![position
                    .name_as_string()
                    .map_err(FontError::from)
                    .map_err(wrap_font_err)?],
                alg: None,
                hash: ByteBuf::from(Vec::new()),
                pad: ByteBuf::from(Vec::new()),
                range_start: position.offset(),
                range_len: position.length(),
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
    ) -> crate::error::Result<()> {
        match embed_ref {
            crate::asset_io::RemoteRefEmbedType::Xmp(manifest_uri) => {
                #[cfg(feature = "xmp_write")]
                {
                    font_xmp_support::add_reference_as_xmp_to_font(asset_path, &manifest_uri)
                        .map_err(wrap_font_err)
                }
                #[cfg(not(feature = "xmp_write"))]
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
                #[cfg(feature = "xmp_write")]
                {
                    font_xmp_support::add_reference_as_xmp_to_stream(
                        reader,
                        output_stream,
                        &manifest_uri,
                    )
                    .map_err(wrap_font_err)
                }
                #[cfg(not(feature = "xmp_write"))]
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

    use std::io::Cursor;

    use c2pa_font_handler::{
        chunks::ChunkPosition,
        woff1::{directory::Woff1DirectoryEntry, font::WoffChunkType, header::Woff1Header},
    };
    use tempfile::tempdir;

    use super::*;
    use crate::utils::test::{fixture_path, temp_dir_path};

    #[ignore] // Need WOFF 1 test fixture
    #[test]
    #[cfg(not(feature = "xmp_write"))]
    // Key to cryptic test comments.
    //
    //   IIP - Invalid/Ignored/Passthrough
    //         This field's value is bogus, possibly illegal, but it is expected
    //         that this code will neither detect nor modify it. Examples are
    //         "reserved" bytes in font tables which are supposed to be zero,
    //         major and/or minor version fields that look pretty in the spec
    //         but never have any practical effect in the real world, etc.
    /// Verifies the adding of a remote C2PA manifest reference works as
    /// expected.
    fn add_c2pa_ref() {
        let c2pa_data = "test data";

        // Need WOFF 1 test fixture
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

    /// Verifies the adding of a remote C2PA manifest reference as XMP works as
    /// expected.
    #[ignore] // Need WOFF 1 test fixture
    #[test]
    #[cfg(feature = "xmp_write")]
    fn add_c2pa_ref() {
        use crate::utils::xmp_inmemory_utils::extract_provenance;

        let c2pa_data = "test data";

        // Load the basic WOFF 1 test fixture
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
                let provenance = extract_provenance(manifest_uri.as_str()).unwrap();
                assert_eq!(expected_manifest_uri, provenance);
            }
            _ => panic!("Expected to read a reference from the font file"),
        };
    }

    #[test]
    /// Verify when reading the object locations for hashing, we get zero
    /// positions when the font contains zero tables
    fn get_chunk_positions_without_any_tables() {
        let font_data = vec![
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
        let mut font_stream: Cursor<&[u8]> = Cursor::<&[u8]>::new(&font_data);
        let positions = Woff1Font::get_chunk_positions(&mut font_stream).unwrap();
        // Should have one position reported for the table directory itself
        assert_eq!(2, positions.len());
        assert_eq!(0, positions.first().unwrap().offset());
        assert_eq!(
            size_of::<Woff1Header>(),
            positions.first().unwrap().length() as usize
        );
        assert_eq!(
            &WoffChunkType::Header,
            positions.first().unwrap().chunk_type()
        );
        assert_eq!(
            size_of::<Woff1Header>(),
            positions.get(1).unwrap().offset() as usize
        );
        assert_eq!(0, positions.get(1).unwrap().length() as usize);
        assert_eq!(
            &WoffChunkType::DirectoryEntry,
            positions.get(1).unwrap().chunk_type()
        );
    }

    #[test]
    /// Verify when reading the object locations for hashing, we get zero
    /// positions when the font does not contain a C2PA font table
    fn get_chunk_positions_without_c2pa_table() {
        let font_data = vec![
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
            0x67, 0x61, 0x71, 0x66, // garf
            0x00, 0x00, 0x00, 0x40, //   offset (64)
            0x00, 0x00, 0x00, 0x07, //   compLength (7)
            0x00, 0x00, 0x00, 0x07, //   origLength (7)
            0x12, 0x34, 0x56, 0x78, //   origChecksum (0x12345678)
            // garf Table
            0x6c, 0x61, 0x73, 0x61, // Major / Minor versions
            0x67, 0x6e, 0x61,
        ];
        let mut font_stream: Cursor<&[u8]> = Cursor::<&[u8]>::new(&font_data);
        let positions = Woff1Font::get_chunk_positions(&mut font_stream).unwrap();

        // Should have 3 positions reported for the header, directory, and table.
        // record, and the table data
        assert_eq!(3, positions.len());

        let hdr_chunk = positions.first().unwrap();
        assert_eq!(&WoffChunkType::Header, hdr_chunk.chunk_type());
        assert_eq!(0, hdr_chunk.offset());
        assert_eq!(size_of::<Woff1Header>(), hdr_chunk.length() as usize);

        let dir_chunk = positions.get(1).unwrap();
        assert_eq!(&WoffChunkType::DirectoryEntry, dir_chunk.chunk_type());
        assert_eq!(size_of::<Woff1Header>(), dir_chunk.offset() as usize);
        assert_eq!(
            size_of::<Woff1DirectoryEntry>(),
            dir_chunk.length() as usize
        );

        let tbl_chunk = positions.get(2).unwrap();
        assert_eq!(&WoffChunkType::TableData, tbl_chunk.chunk_type());
        assert_eq!(
            size_of::<Woff1Header>() + size_of::<Woff1DirectoryEntry>(),
            tbl_chunk.offset() as usize
        );
        assert_eq!(7, tbl_chunk.length());
    }

    #[ignore] // Need WOFF 1 test fixture
    #[test]
    fn get_object_locations() {
        // Load the basic WOFF 1 test fixture - C2PA-XYZ - Select WOFF 1 test fixture
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

    #[test]
    #[ignore]
    /// Verify the C2PA table data can be read from a font stream
    fn reads_c2pa_table_from_stream() {
        todo!("C2PA record update not supported in crate yet");
        /*
        let font_data = vec![
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
            0x00, 0x00, 0x00, 0x25, //   compLength (37)
            0x00, 0x00, 0x00, 0x25, //   origLength (37)
            0x12, 0x34, 0x56, 0x78, //   origChecksum (0x12345678)
            // C2PA Table
            0x00, 0x01, 0x00, 0x04, // Major / Minor versions
            0x00, 0x00, 0x00, 0x14, // Manifest URI offset (0)
            0x00, 0x08, 0x00, 0x00, // Manifest URI length (0) / reserved (0)
            0x00, 0x00, 0x00, 0x1c, // C2PA manifest store offset (0)
            0x00, 0x00, 0x00, 0x09, // C2PA manifest store length (0)
            0x66, 0x69, 0x6c, 0x65, // active manifest uri data
            0x3a, 0x2f, 0x2f, 0x61, // active manifest uri data cont'd
            // Rust Question - Is there some way of breaking up this array
            // definition into chunks? For example, in C, the syntax
            //    "some" "more" "string"
            // gets consolidated by the compiler into the single string literal
            // "somemorestring" - if we could could do that, we could D.R.Y. up
            // the definition of this content-fragment and the literal in the
            // assert down below that checks. (And maybe the chunk lengths could
            // be compile-time-knowable, too, for checking size/offset stuff?)
            0x74, 0x65, 0x73, 0x74, // manifest store data
            0x2d, 0x64, 0x61, 0x74, // manifest store data, cont'd
            0x61, // manifest store data, cont'd
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
        */
    }

    #[ignore] // Need WOFF 1 test fixture
    #[test]
    /// Verifies the ability to write/read C2PA manifest store data to/from an
    /// OpenType font
    fn remove_c2pa_manifest_store() {
        let c2pa_data = "test data";

        // Load the basic WOFF 1 test fixture - C2PA-XYZ - Select WOFF 1 test fixture
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

    #[ignore] // Need WOFF 1 test fixture
    #[test]
    /// Verifies the ability to write/read C2PA manifest store data to/from an
    /// OpenType font
    fn write_read_c2pa_from_font() {
        let c2pa_data = "test data";

        // Load the basic WOFF 1 test fixture - C2PA-XYZ - Select WOFF 1 test fixture
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
        use std::fs::File;

        use tempfile::tempdir;

        use crate::{
            asset_handlers::woff_io::{font_xmp_support, WoffIO},
            asset_io::CAIReader,
            utils::{test::temp_dir_path, xmp_inmemory_utils::extract_provenance},
        };

        #[ignore] // Need WOFF 1 test fixture
        #[test]
        /// Verifies the `font_xmp_support::add_reference_as_xmp_to_stream` is
        /// able to add a reference to as XMP when there is already data in the
        /// reference field.
        fn add_reference_as_xmp_to_stream_with_data() {
            // Load the basic WOFF 1 test fixture - C2PA-XYZ - Select WOFF 1 test fixture
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

            let woff_handler = WoffIO {};
            let mut f: File = File::open(output).unwrap();
            match woff_handler.read_xmp(&mut f) {
                Some(xmp_data_str) => {
                    let xmp_value = extract_provenance(xmp_data_str.as_str()).unwrap();
                    assert_eq!("new test data", xmp_value);
                }
                None => panic!("Expected to read XMP from the resource."),
            }
        }

        /// Verifies the `font_xmp_support::build_xmp_from_stream` method
        /// correctly returns error for NotFound when there is no data in the
        /// stream to return.
        #[test]
        #[ignore]
        fn build_xmp_from_stream_without_reference() {
            todo!("C2PA record update not supported in crate yet");
            /*
            let font_data = vec![
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
                0x67, 0x61, 0x71, 0x66, // garf
                0x00, 0x00, 0x00, 0x40, //   offset (64)
                0x00, 0x00, 0x00, 0x07, //   compLength (7)
                0x00, 0x00, 0x00, 0x07, //   origLength (7)
                0x12, 0x34, 0x56, 0x78, //   origChecksum (0x12345678)
                // garf Table
                0x6c, 0x61, 0x73, 0x61, // Major / Minor versions
                0x67, 0x6e, 0x61,
            ];
            let mut font_stream: Cursor<&[u8]> = Cursor::<&[u8]>::new(&font_data);
            match font_xmp_support::build_xmp_from_stream(&mut font_stream) {
                Ok(_) => panic!("Did not expect an OK result, as data is missing"),
                Err(FontError::XmpNotFound) => {}
                Err(_) => panic!("Unexpected error when building XMP data"),
            }
            */
        }

        /// Verifies the `font_xmp_support::build_xmp_from_stream` method
        /// correctly returns error for NotFound when there is no data in the
        /// stream to return.
        #[test]
        #[ignore]
        fn build_xmp_from_stream_with_reference_not_xmp() {
            todo!("C2PA record update not supported in crate yet");
            /*
            let font_data = vec![
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
                0x00, 0x00, 0x00, 0x25, //   compLength (37)
                0x00, 0x00, 0x00, 0x25, //   origLength (37)
                0x12, 0x34, 0x56, 0x78, //   origChecksum (0x12345678)
                // C2PA Table
                0x00, 0x01, 0x00, 0x04, // Major / Minor versions
                0x00, 0x00, 0x00, 0x14, // Manifest URI offset (0)
                0x00, 0x08, 0x00, 0x00, // Manifest URI length (0) / reserved (0)
                0x00, 0x00, 0x00, 0x1c, // C2PA manifest store offset (0)
                0x00, 0x00, 0x00, 0x09, // C2PA manifest store length (0)
                0x66, 0x69, 0x6c, 0x65, // active manifest uri data
                0x3a, 0x2f, 0x2f, 0x61, // active manifest uri data cont'd
                // Rust Question - Is there some way of breaking up this array
                // definition into chunks? For example, in C, the syntax
                //    "some" "more" "string"
                // gets consolidated by the compiler into the single string literal
                // "somemorestring" - if we could could do that, we could D.R.Y. up
                // the definition of this content-fragment and the literal in the
                // assert down below that checks. (And maybe the chunk lengths could
                // be compile-time-knowable, too, for checking size/offset stuff?)
                0x74, 0x65, 0x73, 0x74, // manifest store data
                0x2d, 0x64, 0x61, 0x74, // manifest store data, cont'd
                0x61, // manifest store data, cont'd
            ];
            let mut font_stream: Cursor<&[u8]> = Cursor::<&[u8]>::new(&font_data);
            match font_xmp_support::build_xmp_from_stream(&mut font_stream) {
                Ok(_xmp_data) => {}
                Err(_) => panic!("Unexpected error when building XMP data"),
            }
            */
        }
    }

    #[test]
    #[ignore]
    fn add_required_chunks_to_stream_minimal() {
        todo!("C2PA record update not supported in crate yet");
        /*
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
            0x00, 0x00, 0x00, 0x01, //   origChecksum (0x00000001)
            // C2PA Table
            0x00, 0x00, 0x00, 0x01, // Major / Minor versions
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
        */
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
        let positions = Woff1Font::get_chunk_positions(&mut font_stream).unwrap();
        // Should have one position reported for the table directory itself
        assert_eq!(2, positions.len());
        let header_posn = positions.first().unwrap();
        assert_eq!(
            *header_posn,
            ChunkPosition::new(0, 44, *b"\x00\x00\x00W", WoffChunkType::Header,)
        );
        let directory_posn = positions.get(1).unwrap();
        assert_eq!(
            *directory_posn,
            ChunkPosition::new(44, 0, *b"\x00\x00\x01D", WoffChunkType::DirectoryEntry,)
        );
    }
}
