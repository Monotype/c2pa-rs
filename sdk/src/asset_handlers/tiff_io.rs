// Copyright 2023 Adobe. All rights reserved.
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
    collections::{BTreeMap, HashMap},
    fs::OpenOptions,
    io::{Cursor, Read, Seek, SeekFrom, Write},
    path::Path,
    vec,
};

use atree::{Arena, Token};
use byteorder::{NativeEndian, ReadBytesExt, WriteBytesExt};
use byteordered::{with_order, ByteOrdered, Endianness};
use conv::ValueFrom;

use crate::{
    asset_io::{
        rename_or_move, AssetIO, AssetPatch, CAIRead, CAIReadWrite, CAIReader, CAIWriter,
        ComposedManifestRef, HashBlockObjectType, HashObjectPositions, RemoteRefEmbed,
        RemoteRefEmbedType,
    },
    error::{Error, Result},
    utils::{
        io_utils::{safe_vec, stream_len, tempfile_builder, ReaderUtils},
        xmp_inmemory_utils::{add_provenance, MIN_XMP},
    },
};

const II: [u8; 2] = *b"II";
const MM: [u8; 2] = *b"MM";

const C2PA_TAG: u16 = 0xcd41;
const XMP_TAG: u16 = 0x02bc;
const SUBFILE_TAG: u16 = 0x014a;
const EXIFIFD_TAG: u16 = 0x8769;
const GPSIFD_TAG: u16 = 0x8825;
const C2PA_FIELD_TYPE: u16 = 7;

const STRIPBYTECOUNTS: u16 = 279;
const STRIPOFFSETS: u16 = 273;
const TILEBYTECOUNTS: u16 = 325;
const TILEOFFSETS: u16 = 324;

const SUBFILES: [u16; 3] = [SUBFILE_TAG, EXIFIFD_TAG, GPSIFD_TAG];

static SUPPORTED_TYPES: [&str; 10] = [
    "tif",
    "tiff",
    "image/tiff",
    "dng",
    "image/dng",
    "image/x-adobe-dng",
    "arw",
    "image/x-sony-arw",
    "nef",
    "image/x-nikon-nef",
];

// Writing native formats is beyond the scope of the SDK.
static SUPPORTED_WRITER_TYPES: [&str; 6] = [
    "tif",
    "tiff",
    "image/tiff",
    "dng",
    "image/dng",
    "image/x-adobe-dng",
];

// The type of an IFD entry
#[derive(Debug, PartialEq)]
enum IFDEntryType {
    Byte = 1,       // 8-bit unsigned integer
    Ascii = 2,      // 8-bit byte that contains a 7-bit ASCII code; the last byte must be zero
    Short = 3,      // 16-bit unsigned integer
    Long = 4,       // 32-bit unsigned integer
    Rational = 5,   // Fraction stored as two 32-bit unsigned integers
    Sbyte = 6,      // 8-bit signed integer
    Undefined = 7,  // 8-bit byte that may contain anything, depending on the field
    Sshort = 8,     // 16-bit signed integer
    Slong = 9,      // 32-bit signed integer
    Srational = 10, // Fraction stored as two 32-bit signed integers
    Float = 11,     // 32-bit IEEE floating point
    Double = 12,    // 64-bit IEEE floating point
    Ifd = 13,       // 32-bit unsigned integer (offset)
    Long8 = 16,     // BigTIFF 64-bit unsigned integer
    Slong8 = 17,    // BigTIFF 64-bit unsigned integer (offset)
    Ifd8 = 18,      // 64-bit unsigned integer (offset)
}

impl IFDEntryType {
    pub fn from_u16(val: u16) -> Option<IFDEntryType> {
        match val {
            1 => Some(IFDEntryType::Byte),
            2 => Some(IFDEntryType::Ascii),
            3 => Some(IFDEntryType::Short),
            4 => Some(IFDEntryType::Long),
            5 => Some(IFDEntryType::Rational),
            6 => Some(IFDEntryType::Sbyte),
            7 => Some(IFDEntryType::Undefined),
            8 => Some(IFDEntryType::Sshort),
            9 => Some(IFDEntryType::Slong),
            10 => Some(IFDEntryType::Srational),
            11 => Some(IFDEntryType::Float),
            12 => Some(IFDEntryType::Double),
            13 => Some(IFDEntryType::Ifd),
            16 => Some(IFDEntryType::Long8),
            17 => Some(IFDEntryType::Slong8),
            18 => Some(IFDEntryType::Ifd8),
            _ => None,
        }
    }
}

// TIFF IFD Entry (value_offset is in target endian)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IfdEntry {
    entry_tag: u16,
    entry_type: u16,
    value_count: u64,
    value_offset: u64,
}

// helper enum to know if the IFD requires special handling
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IfdType {
    Page,
    Subfile,
    Exif,
    Gps,
}

// TIFF IFD
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageFileDirectory {
    offset: u64,
    entry_cnt: u64,
    ifd_type: IfdType,
    entries: HashMap<u16, IfdEntry>,
    next_ifd_offset: Option<u64>,
}

impl ImageFileDirectory {
    #[allow(dead_code)]
    pub fn get_tag(&self, tag_id: u16) -> Option<&IfdEntry> {
        self.entries.get(&tag_id)
    }

    #[allow(dead_code)]
    pub fn get_tag_mut(&mut self, tag_id: u16) -> Option<&mut IfdEntry> {
        self.entries.get_mut(&tag_id)
    }
}

// Struct to map the contents of a TIFF file
#[allow(dead_code)]
pub(crate) struct TiffStructure {
    byte_order: Endianness,
    big_tiff: bool,
    first_ifd_offset: u64,
    first_ifd: Option<ImageFileDirectory>,
}

impl TiffStructure {
    #[allow(dead_code)]
    pub fn load<R>(reader: &mut R) -> Result<Self>
    where
        R: Read + Seek + ?Sized,
    {
        let mut endianness = [0u8, 2];
        reader.read_exact(&mut endianness)?;

        let byte_order = match endianness {
            II => Endianness::Little,
            MM => Endianness::Big,
            _ => {
                return Err(Error::InvalidAsset(
                    "Could not parse input image".to_owned(),
                ))
            }
        };

        let mut byte_reader = ByteOrdered::runtime(reader, byte_order);

        let big_tiff = match byte_reader.read_u16() {
            Ok(42) => false,
            Ok(43) => {
                // read Big TIFF structs
                // Read byte size of offsets, must be 8
                if byte_reader.read_u16()? != 8 {
                    return Err(Error::InvalidAsset(
                        "Could not parse input image".to_owned(),
                    ));
                }
                // must currently be 0
                if byte_reader.read_u16()? != 0 {
                    return Err(Error::InvalidAsset(
                        "Could not parse input image".to_owned(),
                    ));
                }
                true
            }
            _ => {
                return Err(Error::InvalidAsset(
                    "Could not parse input image".to_owned(),
                ))
            }
        };

        let first_ifd_offset = if big_tiff {
            byte_reader.read_u64()?
        } else {
            byte_reader.read_u32()?.into()
        };

        // move read pointer to IFD
        byte_reader.seek(SeekFrom::Start(first_ifd_offset))?;
        let first_ifd = TiffStructure::read_ifd(
            byte_reader.into_inner(),
            byte_order,
            big_tiff,
            IfdType::Page,
        )?;

        let ts = TiffStructure {
            byte_order,
            big_tiff,
            first_ifd_offset,
            first_ifd: Some(first_ifd),
        };

        Ok(ts)
    }

    // read IFD entries, all value_offset are in source endianness
    pub fn read_ifd_entries<R>(
        byte_reader: &mut ByteOrdered<&mut R, Endianness>,
        big_tiff: bool,
        entry_cnt: u64,
        entries: &mut HashMap<u16, IfdEntry>,
    ) -> Result<()>
    where
        R: Read + Seek + ?Sized,
    {
        for _ in 0..entry_cnt {
            let tag = byte_reader.read_u16()?;
            let tag_type = byte_reader.read_u16()?;

            let (count, data_offset) = if big_tiff {
                let count = byte_reader.read_u64()?;
                let mut buf = [0; 8];
                byte_reader.read_exact(&mut buf)?;

                let data_offset = buf
                    .as_slice()
                    .read_u64::<NativeEndian>()
                    .map_err(|_err| Error::InvalidAsset("value out of range".to_string()))?;
                (count, data_offset)
            } else {
                let count = byte_reader.read_u32()?;
                let mut buf = [0; 4];
                byte_reader.read_exact(&mut buf)?;

                let data_offset = buf
                    .as_slice()
                    .read_u32::<NativeEndian>()
                    .map_err(|_err| Error::InvalidAsset("value out of range".to_string()))?;
                (count.into(), data_offset.into())
            };

            let ifd_entry = IfdEntry {
                entry_tag: tag,
                entry_type: tag_type,
                value_count: count,
                value_offset: data_offset,
            };

            /*
            println!(
                "{}, {}, {}. {:?}",
                ifd_entry.entry_tag,
                ifd_entry.entry_type,
                ifd_entry.value_count,
                ifd_entry.value_offset.to_ne_bytes()
            );
            */

            entries.insert(tag, ifd_entry);
        }

        Ok(())
    }

    // read IFD from reader
    pub fn read_ifd<R>(
        reader: &mut R,
        byte_order: Endianness,
        big_tiff: bool,
        ifd_type: IfdType,
    ) -> Result<ImageFileDirectory>
    where
        R: Read + Seek + ReadBytesExt + ?Sized,
    {
        let mut byte_reader = ByteOrdered::runtime(reader, byte_order);

        let ifd_offset = byte_reader.stream_position()?;
        //println!("IFD Offset: {:#x}", ifd_offset);

        let entry_cnt = if big_tiff {
            byte_reader.read_u64()?
        } else {
            byte_reader.read_u16()?.into()
        };

        let mut ifd = ImageFileDirectory {
            offset: ifd_offset,
            entry_cnt,
            ifd_type,
            entries: HashMap::new(),
            next_ifd_offset: None,
        };

        TiffStructure::read_ifd_entries(&mut byte_reader, big_tiff, entry_cnt, &mut ifd.entries)?;

        let next_ifd = if big_tiff {
            byte_reader.read_u64()?
        } else {
            byte_reader.read_u32()?.into()
        };

        match next_ifd {
            0 => (),
            _ => ifd.next_ifd_offset = Some(next_ifd),
        };

        Ok(ifd)
    }
}

// offset are stored in source endianness so to use offset value in Seek calls we must convert to native endianness
fn decode_offset(offset_file_native: u64, endianness: Endianness, big_tiff: bool) -> Result<u64> {
    let offset: u64;
    let offset_bytes = offset_file_native.to_ne_bytes();
    let offset_reader = Cursor::new(offset_bytes);

    with_order!(offset_reader, endianness, |src| {
        if big_tiff {
            let o = src.read_u64()?;
            offset = o;
        } else {
            let o = src.read_u32()?;
            offset = o.into();
        }
    });

    Ok(offset)
}

// create tree of TIFF structure IFDs and IFD entries.
fn map_tiff<R>(mut input: &mut R) -> Result<(Arena<ImageFileDirectory>, Token, Endianness, bool)>
where
    R: Read + Seek + ?Sized,
{
    let _size = stream_len(input)?;
    input.rewind()?;

    let ts = TiffStructure::load(input)?;

    let (tiff_tree, page_0): (Arena<ImageFileDirectory>, Token) = if let Some(ifd) =
        ts.first_ifd.clone()
    {
        let (mut tiff_tree, page_0_token) = Arena::with_data(ifd);
        /* No multi-page at the moment
        // get the pages
        loop {
            if let Some(next_ifd_offset) = &tiff_tree[current_token].data.next_ifd_offset {
                input.seek(SeekFrom::Start(*next_ifd_offset))?;

                let next_ifd = TiffStructure::read_ifd(input, ts.byte_order, ts.big_tiff, IFDType::PageIFD)?;

                current_token = current_token.append(&mut tiff_tree, next_ifd)

            } else {
                break;
            }
        }
        */

        // look for known special IFDs on page 0
        let page0_subifd = tiff_tree[page_0_token].data.get_tag(SUBFILE_TAG).copied();

        // grab SubIFDs for page 0 (DNG)
        if let Some(subifd) = page0_subifd {
            let decoded_offset = decode_offset(subifd.value_offset, ts.byte_order, ts.big_tiff)?;
            input.seek(SeekFrom::Start(decoded_offset))?;

            let num_longs_x4 = usize::value_from(
                subifd
                    .value_count
                    .checked_mul(4)
                    .ok_or_else(|| Error::InvalidAsset("value out of range".to_string()))?,
            )
            .map_err(|_err| Error::InvalidAsset("value out of range".to_string()))?;
            let mut subfile_offsets = safe_vec(subifd.value_count, Some(0u32))?; // will contain offsets in native endianness

            if num_longs_x4 <= 4 || ts.big_tiff && num_longs_x4 <= 8 {
                let offset_bytes = subifd.value_offset.to_ne_bytes();
                let offset_reader = Cursor::new(offset_bytes);

                with_order!(offset_reader, ts.byte_order, |src| {
                    for item in subfile_offsets.iter_mut().take(num_longs_x4 / 4) {
                        let s = src.read_u32()?; // read a long from offset
                        *item = s; // write a long in output endian
                    }
                });
            } else {
                let buf = input.read_to_vec(num_longs_x4 as u64)?;
                let offsets_buf = Cursor::new(buf);

                with_order!(offsets_buf, ts.byte_order, |src| {
                    for item in subfile_offsets.iter_mut().take(num_longs_x4 / 4) {
                        let s = src.read_u32()?; // read a long from offset
                        *item = s; // write a long in output endian
                    }
                });
            }

            // get all subfiles
            for subfile_offset in subfile_offsets {
                let u64_offset = u64::value_from(subfile_offset)
                    .map_err(|_err| Error::InvalidAsset("value out of range".to_string()))?;
                input.seek(SeekFrom::Start(u64_offset))?;

                //println!("Reading SubIFD: {}", u64_offset);

                let subfile_ifd =
                    TiffStructure::read_ifd(input, ts.byte_order, ts.big_tiff, IfdType::Subfile)?;
                let subfile_token = tiff_tree.new_node(subfile_ifd);

                page_0_token
                    .append_node(&mut tiff_tree, subfile_token)
                    .map_err(|_err| Error::InvalidAsset("Bad TIFF Structure".to_string()))?;
            }
        }

        // grab EXIF IFD for page 0 (DNG)
        if let Some(exififd) = tiff_tree[page_0_token].data.get_tag(EXIFIFD_TAG) {
            let decoded_offset = decode_offset(exififd.value_offset, ts.byte_order, ts.big_tiff)?;
            input.seek(SeekFrom::Start(decoded_offset))?;

            //println!("EXIF Reading SubIFD: {}", decoded_offset);

            let exif_ifd =
                TiffStructure::read_ifd(input, ts.byte_order, ts.big_tiff, IfdType::Exif)?;
            let exif_token = tiff_tree.new_node(exif_ifd);

            page_0_token
                .append_node(&mut tiff_tree, exif_token)
                .map_err(|_err| Error::InvalidAsset("Bad TIFF Structure".to_string()))?;
        }

        // grab GPS IFD for page 0 (DNG)
        if let Some(gpsifd) = tiff_tree[page_0_token].data.get_tag(GPSIFD_TAG) {
            let decoded_offset = decode_offset(gpsifd.value_offset, ts.byte_order, ts.big_tiff)?;
            input.seek(SeekFrom::Start(decoded_offset))?;

            //println!("GPS Reading SubIFD: {}", decoded_offset);

            let gps_ifd = TiffStructure::read_ifd(input, ts.byte_order, ts.big_tiff, IfdType::Gps)?;
            let gps_token = tiff_tree.new_node(gps_ifd);

            page_0_token
                .append_node(&mut tiff_tree, gps_token)
                .map_err(|_err| Error::InvalidAsset("Bad TIFF Structure".to_string()))?;
        }

        (tiff_tree, page_0_token)
    } else {
        return Err(Error::InvalidAsset("TIFF structure invalid".to_string()));
    };

    Ok((tiff_tree, page_0, ts.byte_order, ts.big_tiff))
}

// struct used to clone source IFD entries. value_bytes are in target endianness
#[derive(Eq, PartialEq, Clone)]
pub(crate) struct IfdClonedEntry {
    pub entry_tag: u16,
    pub entry_type: u16,
    pub value_count: u64,
    pub value_bytes: Vec<u8>,
}

// struct to clone a TIFF/DNG and new tags if desired
pub(crate) struct TiffCloner<T>
where
    T: Read + Write + Seek,
{
    endianness: Endianness,
    big_tiff: bool,
    first_idf_offset: u64,
    writer: ByteOrdered<T, Endianness>,
    additional_ifds: BTreeMap<u16, IfdClonedEntry>,
}

impl<T: Read + Write + Seek> TiffCloner<T> {
    pub fn new(endianness: Endianness, big_tiff: bool, writer: T) -> Result<TiffCloner<T>> {
        let bo = ByteOrdered::runtime(writer, endianness);

        let mut tc = TiffCloner {
            endianness,
            big_tiff,
            first_idf_offset: 0,
            writer: bo,
            additional_ifds: BTreeMap::new(),
        };

        tc.write_header()?;

        Ok(tc)
    }

    fn offset(&mut self) -> Result<u64> {
        Ok(self.writer.stream_position()?)
    }

    fn pad_word_boundary(&mut self) -> Result<()> {
        let curr_offset = self.offset()?;
        if curr_offset % 4 != 0 {
            let padding = [0, 0, 0];
            let pad_len = 4 - (curr_offset % 4);
            self.writer.write_all(&padding[..pad_len as usize])?;
        }

        Ok(())
    }

    fn write_header(&mut self) -> Result<u64> {
        let boi = match self.endianness {
            Endianness::Big => 0x4d,
            Endianness::Little => 0x49,
        };
        let offset;

        if self.big_tiff {
            self.writer.write_all(&[boi, boi])?;
            self.writer.write_u16(43u16)?;
            self.writer.write_u16(8u16)?;
            self.writer.write_u16(0u16)?;
            offset = self.writer.stream_position()?; // first ifd offset

            self.writer.write_u64(0)?;
        } else {
            self.writer.write_all(&[boi, boi])?;
            self.writer.write_u16(42u16)?;
            offset = self.writer.stream_position()?; // first ifd offset

            self.writer.write_u32(0)?;
        }

        self.first_idf_offset = offset;
        Ok(offset)
    }

    fn write_entry_count(&mut self, count: usize) -> Result<()> {
        if self.big_tiff {
            let cnt = u64::value_from(count)
                .map_err(|_err| Error::InvalidAsset("value out of range".to_string()))?; // get beginning of chunk which starts 4 bytes before label

            self.writer.write_u64(cnt)?;
        } else {
            let cnt = u16::value_from(count)
                .map_err(|_err| Error::InvalidAsset("value out of range".to_string()))?; // get beginning of chunk which starts 4 bytes before label

            self.writer.write_u16(cnt)?;
        }

        Ok(())
    }

    fn write_ifd(&mut self, target_ifd: &mut BTreeMap<u16, IfdClonedEntry>) -> Result<u64> {
        // write out all data and save the offsets, skipping subfiles since the data is already written
        for &mut IfdClonedEntry {
            value_bytes: ref mut value_bytes_ref,
            ..
        } in target_ifd.values_mut()
        {
            let data_bytes = if self.big_tiff { 8 } else { 4 };

            if value_bytes_ref.len() > data_bytes {
                // get location of entry data start
                let offset = self.writer.stream_position()?;

                // write out the data bytes
                self.writer.write_all(value_bytes_ref)?;

                // set offset pointer in file source endian
                let mut offset_vec = vec![0; data_bytes];

                with_order!(offset_vec.as_mut_slice(), self.endianness, |ew| {
                    if self.big_tiff {
                        ew.write_u64(offset)?;
                    } else {
                        let offset_u32 = u32::value_from(offset).map_err(|_err| {
                            Error::InvalidAsset("value out of range".to_string())
                        })?; // get beginning of chunk which starts 4 bytes before label

                        ew.write_u32(offset_u32)?;
                    }
                });

                // set to new data offset position
                *value_bytes_ref = offset_vec;
            } else {
                while value_bytes_ref.len() < data_bytes {
                    value_bytes_ref.push(0);
                }
            }
        }

        // Write out the IFD

        // start on a WORD boundary
        self.pad_word_boundary()?;

        // save location of start of IFD
        let ifd_offset = self.writer.stream_position()?;

        // write out the entry count
        self.write_entry_count(target_ifd.len())?;

        // write out the directory entries
        for (tag, entry) in target_ifd.iter() {
            self.writer.write_u16(*tag)?;
            self.writer.write_u16(entry.entry_type)?;

            if self.big_tiff {
                let cnt = u64::value_from(entry.value_count)
                    .map_err(|_err| Error::InvalidAsset("value out of range".to_string()))?;

                self.writer.write_u64(cnt)?;
            } else {
                let cnt = u32::value_from(entry.value_count)
                    .map_err(|_err| Error::InvalidAsset("value out of range".to_string()))?;

                self.writer.write_u32(cnt)?;
            }

            self.writer.write_all(&entry.value_bytes)?;
        }

        Ok(ifd_offset)
    }

    // add new TAG by supplying the IDF entry
    pub fn add_target_tag(&mut self, entry: IfdClonedEntry) {
        self.additional_ifds.insert(entry.entry_tag, entry);
    }

    fn clone_image_data<R: Read + Seek + ?Sized>(
        &mut self,
        target_ifd: &mut BTreeMap<u16, IfdClonedEntry>,
        mut asset_reader: &mut R,
    ) -> Result<()> {
        match (
            target_ifd.contains_key(&STRIPBYTECOUNTS),
            target_ifd.contains_key(&STRIPOFFSETS),
            target_ifd.contains_key(&TILEBYTECOUNTS),
            target_ifd.contains_key(&TILEOFFSETS),
        ) {
            (true, true, false, false) => {
                // stripped image data
                let sbc_entry = target_ifd[&STRIPBYTECOUNTS].clone();
                let so_entry = target_ifd.get_mut(&STRIPOFFSETS).ok_or(Error::NotFound)?;

                // check for well formed TIFF
                if so_entry.value_count != sbc_entry.value_count {
                    return Err(Error::InvalidAsset(
                        "TIFF strip count does not match strip offset count".to_string(),
                    ));
                }

                let mut sbcs: Vec<u64> = safe_vec(sbc_entry.value_count, Some(0))?;
                let mut dest_offsets: Vec<u64> = Vec::new();

                // get the byte counts
                with_order!(sbc_entry.value_bytes.as_slice(), self.endianness, |src| {
                    for c in &mut sbcs {
                        match sbc_entry.entry_type {
                            4u16 => {
                                let s = src.read_u32()?;
                                *c = s.into();
                            }
                            3u16 => {
                                let s = src.read_u16()?;
                                *c = s.into();
                            }
                            16u16 => {
                                let s = src.read_u64()?;
                                *c = s;
                            }
                            _ => return Err(Error::InvalidAsset("invalid TIFF strip".to_string())),
                        }
                    }
                });

                // seek to end of file
                self.writer.seek(SeekFrom::End(0))?;

                // copy the strips
                with_order!(so_entry.value_bytes.as_slice(), self.endianness, |src| {
                    for c in sbcs.iter() {
                        let cnt = usize::value_from(*c).map_err(|_err| {
                            Error::InvalidAsset("value out of range".to_string())
                        })?;

                        // get the offset
                        let so: u64 = match so_entry.entry_type {
                            4u16 => {
                                let s = src.read_u32()?;
                                s.into()
                            }
                            3u16 => {
                                let s = src.read_u16()?;
                                s.into()
                            }
                            16u16 => src.read_u64()?,
                            _ => return Err(Error::InvalidAsset("invalid TIFF strip".to_string())),
                        };

                        let dest_offset = self.writer.stream_position()?;
                        dest_offsets.push(dest_offset);

                        // copy the strip to new file
                        asset_reader.seek(SeekFrom::Start(so))?;
                        let data = asset_reader.read_to_vec(cnt as u64)?;
                        self.writer.write_all(data.as_slice())?;
                    }
                });

                // patch the offsets
                with_order!(
                    so_entry.value_bytes.as_mut_slice(),
                    self.endianness,
                    |dest| {
                        for o in dest_offsets.iter() {
                            // get the offset
                            match so_entry.entry_type {
                                4u16 => {
                                    let offset = u32::value_from(*o).map_err(|_err| {
                                        Error::InvalidAsset("value out of range".to_string())
                                    })?;
                                    dest.write_u32(offset)?;
                                }
                                3u16 => {
                                    let offset = u16::value_from(*o).map_err(|_err| {
                                        Error::InvalidAsset("value out of range".to_string())
                                    })?;
                                    dest.write_u16(offset)?;
                                }
                                16u16 => {
                                    let offset = *o;
                                    dest.write_u64(offset)?;
                                }
                                _ => {
                                    return Err(Error::InvalidAsset(
                                        "invalid TIFF strip".to_string(),
                                    ))
                                }
                            }
                        }
                    }
                );
            }
            (false, false, true, true) => {
                // tiled image data
                let tbc_entry = target_ifd[&TILEBYTECOUNTS].clone();
                let to_entry = target_ifd.get_mut(&TILEOFFSETS).ok_or(Error::NotFound)?;

                // check for well formed TIFF
                if to_entry.value_count != tbc_entry.value_count {
                    return Err(Error::InvalidAsset(
                        "TIFF tile count does not match tile offset count".to_string(),
                    ));
                }

                let mut tbcs: Vec<u64> = safe_vec(tbc_entry.value_count, Some(0u64))?;
                let mut dest_offsets: Vec<u64> = Vec::new();

                // get the byte counts
                with_order!(tbc_entry.value_bytes.as_slice(), self.endianness, |src| {
                    for val in &mut tbcs {
                        match tbc_entry.entry_type {
                            4u16 => {
                                let s = src.read_u32()?;
                                *val = s.into();
                            }
                            3u16 => {
                                let s = src.read_u16()?;
                                *val = s.into();
                            }
                            16u16 => {
                                let s = src.read_u64()?;
                                *val = s;
                            }
                            _ => return Err(Error::InvalidAsset("invalid TIFF tile".to_string())),
                        }
                    }
                });

                // seek to end of file
                self.writer.seek(SeekFrom::End(0))?;

                // copy the tiles
                with_order!(to_entry.value_bytes.as_slice(), self.endianness, |src| {
                    for c in tbcs.iter() {
                        let cnt = usize::value_from(*c).map_err(|_err| {
                            Error::InvalidAsset("value out of range".to_string())
                        })?;

                        // get the offset
                        let to: u64 = match to_entry.entry_type {
                            4u16 => {
                                let s = src.read_u32()?;
                                s.into()
                            }
                            16u16 => src.read_u64()?,
                            _ => return Err(Error::InvalidAsset("invalid TIFF tile".to_string())),
                        };

                        let dest_offset = self.writer.stream_position()?;
                        dest_offsets.push(dest_offset);

                        // copy the tile to new file
                        asset_reader.seek(SeekFrom::Start(to))?;
                        let data = asset_reader.read_to_vec(cnt as u64)?;
                        self.writer.write_all(data.as_slice())?;
                    }
                });

                // patch the offsets
                with_order!(
                    to_entry.value_bytes.as_mut_slice(),
                    self.endianness,
                    |dest| {
                        for v in dest_offsets.iter() {
                            // get the offset
                            match to_entry.entry_type {
                                4u16 => {
                                    let offset = u32::value_from(*v).map_err(|_err| {
                                        Error::InvalidAsset("value out of range".to_string())
                                    })?;
                                    dest.write_u32(offset)?;
                                }
                                3u16 => {
                                    let offset = u16::value_from(*v).map_err(|_err| {
                                        Error::InvalidAsset("value out of range".to_string())
                                    })?;
                                    dest.write_u16(offset)?;
                                }
                                16u16 => {
                                    let offset = *v;
                                    dest.write_u64(offset)?;
                                }
                                _ => {
                                    return Err(Error::InvalidAsset(
                                        "invalid TIFF tile".to_string(),
                                    ))
                                }
                            }
                        }
                    }
                );
            }
            (_, _, _, _) => (),
        };

        Ok(())
    }

    fn clone_sub_files<R: Read + Seek + ?Sized>(
        &mut self,
        tiff_tree: &Arena<ImageFileDirectory>,
        page: Token,
        asset_reader: &mut R,
    ) -> Result<HashMap<u16, Vec<u64>>> {
        // offset map
        let mut offset_map: HashMap<u16, Vec<u64>> = HashMap::new();

        let mut offsets_ifd: Vec<u64> = Vec::new();
        let mut offsets_exif: Vec<u64> = Vec::new();
        let mut offsets_gps: Vec<u64> = Vec::new();

        // clone the EXIF entry and DNG entries
        for n in page.children(tiff_tree) {
            let ifd = &n.data;

            // clone IFD entries
            let mut cloned_ifd = self.clone_ifd_entries(&ifd.entries, asset_reader)?;

            // clone the image data
            self.clone_image_data(&mut cloned_ifd, asset_reader)?;

            // write directory
            let sub_ifd_offset = self.write_ifd(&mut cloned_ifd)?;

            // terminate since we don't support chained subifd
            if self.big_tiff {
                self.writer.write_u64(0)?;
            } else {
                self.writer.write_u32(0)?;
            }

            // fix up offset in main page known IFDs
            match ifd.ifd_type {
                IfdType::Page => (),
                IfdType::Subfile => offsets_ifd.push(sub_ifd_offset),
                IfdType::Exif => offsets_exif.push(sub_ifd_offset),
                IfdType::Gps => offsets_gps.push(sub_ifd_offset),
            };
        }

        offset_map.insert(SUBFILE_TAG, offsets_ifd);
        offset_map.insert(EXIFIFD_TAG, offsets_exif);
        offset_map.insert(GPSIFD_TAG, offsets_gps);

        Ok(offset_map)
    }

    pub fn clone_tiff<R: Read + Seek + ?Sized>(
        &mut self,
        tiff_tree: &mut Arena<ImageFileDirectory>,
        page_0: Token,
        asset_reader: &mut R,
    ) -> Result<()> {
        // handle page 0

        // clone the subfile entries (DNG)
        let subfile_offsets = self.clone_sub_files(tiff_tree, page_0, asset_reader)?;

        let page_0_idf = tiff_tree
            .get_mut(page_0)
            .ok_or_else(|| Error::InvalidAsset("TIFF does not have IFD".to_string()))?;

        // clone IFD entries
        let mut cloned_ifd = self.clone_ifd_entries(&page_0_idf.data.entries, asset_reader)?;

        // clone the image data
        self.clone_image_data(&mut cloned_ifd, asset_reader)?;

        // add in new Tags
        for (tag, new_entry) in &self.additional_ifds {
            cloned_ifd.insert(*tag, new_entry.clone());
        }

        // fix up subfile offsets
        for t in SUBFILES {
            if let Some(offsets) = subfile_offsets.get(&t) {
                if offsets.is_empty() {
                    continue;
                }

                let e = cloned_ifd
                    .get_mut(&t)
                    .ok_or_else(|| Error::InvalidAsset("TIFF does not have IFD".to_string()))?;
                let mut adjust_offsets: Vec<u8> = if self.big_tiff {
                    safe_vec(offsets.len() as u64 * 8, Some(0))?
                } else {
                    safe_vec(offsets.len() as u64 * 4, Some(0))?
                };

                with_order!(adjust_offsets.as_mut_slice(), self.endianness, |dest| {
                    for o in offsets {
                        if self.big_tiff {
                            dest.write_u64(*o)?;
                        } else {
                            let offset_u32 = u32::value_from(*o).map_err(|_err| {
                                Error::InvalidAsset("value out of range".to_string())
                            })?;

                            dest.write_u32(offset_u32)?;
                        }
                    }
                });

                e.value_bytes = adjust_offsets;
            }
        }

        // write directory
        let first_ifd_offset = self.write_ifd(&mut cloned_ifd)?;

        // write final location info
        let curr_pos = self.offset()?;

        self.writer.seek(SeekFrom::Start(self.first_idf_offset))?;

        if self.big_tiff {
            self.writer.write_u64(first_ifd_offset)?;
            self.writer.seek(SeekFrom::Start(curr_pos))?;
            self.writer.write_u64(0)?;
        } else {
            let offset_u32 = u32::value_from(first_ifd_offset)
                .map_err(|_err| Error::InvalidAsset("value out of range".to_string()))?; // get beginning of chunk which starts 4 bytes before label

            self.writer.write_u32(offset_u32)?;
            self.writer.seek(SeekFrom::Start(curr_pos))?;
            self.writer.write_u32(0)?;
        }
        self.writer.flush()?;
        Ok(())
    }

    fn clone_ifd_entries<R: Read + Seek + ?Sized>(
        &mut self,
        entries: &HashMap<u16, IfdEntry>,
        mut asset_reader: &mut R,
    ) -> Result<BTreeMap<u16, IfdClonedEntry>> {
        let mut target_ifd: BTreeMap<u16, IfdClonedEntry> = BTreeMap::new();

        for (tag, entry) in entries {
            let target_endianness = self.writer.endianness();

            // get bytes for tag
            let cnt = entry.value_count;
            let et = entry.entry_type;

            let entry_type = IFDEntryType::from_u16(et).ok_or(Error::UnsupportedType)?;

            // read IFD raw data in file native endian format
            let data = match entry_type {
                IFDEntryType::Byte
                | IFDEntryType::Sbyte
                | IFDEntryType::Undefined
                | IFDEntryType::Ascii => {
                    let num_bytes = usize::value_from(cnt)
                        .map_err(|_err| Error::InvalidAsset("value out of range".to_string()))?;

                    let mut data = safe_vec(cnt, Some(0u8))?;

                    if num_bytes <= 4 || self.big_tiff && num_bytes <= 8 {
                        let offset_bytes = entry.value_offset.to_ne_bytes();
                        for (i, item) in offset_bytes.iter().take(num_bytes).enumerate() {
                            data[i] = *item;
                        }
                    } else {
                        // move to start of data
                        asset_reader.seek(SeekFrom::Start(decode_offset(
                            entry.value_offset,
                            target_endianness,
                            self.big_tiff,
                        )?))?;
                        asset_reader.read_exact(data.as_mut_slice())?;
                    }

                    data
                }
                IFDEntryType::Short => {
                    let num_shorts_x2 =
                        usize::value_from(cnt.checked_mul(2).ok_or_else(|| {
                            Error::InvalidAsset("value out of range".to_string())
                        })?)
                        .map_err(|_err| Error::InvalidAsset("value out of range".to_string()))?;

                    let mut data = safe_vec(num_shorts_x2 as u64, Some(0u8))?;

                    if num_shorts_x2 <= 4 || self.big_tiff && num_shorts_x2 <= 8 {
                        let offset_bytes = entry.value_offset.to_ne_bytes();
                        let mut offset_reader = Cursor::new(offset_bytes);

                        let mut w = Cursor::new(data.as_mut_slice());
                        for _i in 0..num_shorts_x2 / 2 {
                            let s = offset_reader.read_u16::<NativeEndian>()?; // read a short from offset
                            w.write_u16::<NativeEndian>(s)?; // write a short in output endian
                        }
                    } else {
                        // move to start of data
                        asset_reader.seek(SeekFrom::Start(decode_offset(
                            entry.value_offset,
                            target_endianness,
                            self.big_tiff,
                        )?))?;
                        asset_reader.read_exact(data.as_mut_slice())?;
                    }

                    data
                }
                IFDEntryType::Long | IFDEntryType::Ifd => {
                    let num_longs_x4 =
                        usize::value_from(cnt.checked_mul(4).ok_or_else(|| {
                            Error::InvalidAsset("value out of range".to_string())
                        })?)
                        .map_err(|_err| Error::InvalidAsset("value out of range".to_string()))?;

                    let mut data = safe_vec(num_longs_x4 as u64, Some(0u8))?;

                    if num_longs_x4 <= 4 || self.big_tiff && num_longs_x4 <= 8 {
                        let offset_bytes = entry.value_offset.to_ne_bytes();
                        let mut offset_reader = Cursor::new(offset_bytes);

                        let mut w = Cursor::new(data.as_mut_slice());
                        for _i in 0..num_longs_x4 / 4 {
                            let s = offset_reader.read_u32::<NativeEndian>()?; // read a long from offset
                            w.write_u32::<NativeEndian>(s)?; // write a long in output endian
                        }
                    } else {
                        // move to start of data
                        asset_reader.seek(SeekFrom::Start(decode_offset(
                            entry.value_offset,
                            target_endianness,
                            self.big_tiff,
                        )?))?;
                        asset_reader.read_exact(data.as_mut_slice())?;
                    }

                    data
                }
                IFDEntryType::Sshort => {
                    let num_sshorts_x2 =
                        usize::value_from(cnt.checked_mul(2).ok_or_else(|| {
                            Error::InvalidAsset("value out of range".to_string())
                        })?)
                        .map_err(|_err| Error::InvalidAsset("value out of range".to_string()))?;

                    let mut data = safe_vec(num_sshorts_x2 as u64, Some(0u8))?;

                    if num_sshorts_x2 <= 4 || self.big_tiff && num_sshorts_x2 <= 8 {
                        let offset_bytes = entry.value_offset.to_ne_bytes();
                        let mut offset_reader = Cursor::new(offset_bytes);

                        let mut w = Cursor::new(data.as_mut_slice());
                        for _i in 0..num_sshorts_x2 / 2 {
                            let s = offset_reader.read_i16::<NativeEndian>()?; // read a short from offset
                            w.write_i16::<NativeEndian>(s)?; // write a short in output endian
                        }
                    } else {
                        // move to start of data
                        asset_reader.seek(SeekFrom::Start(decode_offset(
                            entry.value_offset,
                            target_endianness,
                            self.big_tiff,
                        )?))?;
                        asset_reader.read_exact(data.as_mut_slice())?;
                    }

                    data
                }
                IFDEntryType::Slong => {
                    let num_slongs_x4 =
                        usize::value_from(cnt.checked_mul(4).ok_or_else(|| {
                            Error::InvalidAsset("value out of range".to_string())
                        })?)
                        .map_err(|_err| Error::InvalidAsset("value out of range".to_string()))?;

                    let mut data = safe_vec(num_slongs_x4 as u64, Some(0u8))?;

                    if num_slongs_x4 <= 4 || self.big_tiff && num_slongs_x4 <= 8 {
                        let offset_bytes = entry.value_offset.to_ne_bytes();
                        let mut offset_reader = Cursor::new(offset_bytes);

                        let mut w = Cursor::new(data.as_mut_slice());
                        for _i in 0..num_slongs_x4 / 4 {
                            let s = offset_reader.read_i32::<NativeEndian>()?; // read a slong from offset
                            w.write_i32::<NativeEndian>(s)?; // write a slong in output endian
                        }
                    } else {
                        // move to start of data
                        asset_reader.seek(SeekFrom::Start(decode_offset(
                            entry.value_offset,
                            target_endianness,
                            self.big_tiff,
                        )?))?;
                        asset_reader.read_exact(data.as_mut_slice())?;
                    }

                    data
                }
                IFDEntryType::Float => {
                    let num_floats_x4 =
                        usize::value_from(cnt.checked_mul(4).ok_or_else(|| {
                            Error::InvalidAsset("value out of range".to_string())
                        })?)
                        .map_err(|_err| Error::InvalidAsset("value out of range".to_string()))?;

                    let mut data = safe_vec(num_floats_x4 as u64, Some(0u8))?;

                    if num_floats_x4 <= 4 || self.big_tiff && num_floats_x4 <= 8 {
                        let offset_bytes = entry.value_offset.to_ne_bytes();
                        let mut offset_reader = Cursor::new(offset_bytes);

                        let mut w = Cursor::new(data.as_mut_slice());
                        for _i in 0..num_floats_x4 / 4 {
                            let s = offset_reader.read_f32::<NativeEndian>()?; // read a float from offset
                            w.write_f32::<NativeEndian>(s)?; // write a float in output endian
                        }
                    } else {
                        // move to start of data
                        asset_reader.seek(SeekFrom::Start(decode_offset(
                            entry.value_offset,
                            target_endianness,
                            self.big_tiff,
                        )?))?;
                        asset_reader.read_exact(data.as_mut_slice())?;
                    }

                    data
                }
                IFDEntryType::Rational
                | IFDEntryType::Srational
                | IFDEntryType::Slong8
                | IFDEntryType::Double
                | IFDEntryType::Long8
                | IFDEntryType::Ifd8 => {
                    // move to start of data
                    asset_reader.seek(SeekFrom::Start(decode_offset(
                        entry.value_offset,
                        target_endianness,
                        self.big_tiff,
                    )?))?;

                    asset_reader.read_to_vec(cnt * 8)?
                }
            };

            target_ifd.insert(
                *tag,
                IfdClonedEntry {
                    entry_tag: *tag,
                    entry_type: entry_type as u16,
                    value_count: cnt,
                    value_bytes: data,
                },
            );
        }

        Ok(target_ifd)
    }
}

fn tiff_clone_with_tags<R: Read + Seek + ?Sized, W: Read + Write + Seek + ?Sized>(
    writer: &mut W,
    asset_reader: &mut R,
    tiff_tags: Vec<IfdClonedEntry>,
) -> Result<()> {
    let (mut tiff_tree, page_0, endianness, big_tiff) = map_tiff(asset_reader)?;

    let mut bo = ByteOrdered::new(writer, endianness);

    let mut tc = TiffCloner::new(endianness, big_tiff, &mut bo)?;

    for t in tiff_tags {
        tc.add_target_tag(t);
    }

    tc.clone_tiff(&mut tiff_tree, page_0, asset_reader)?;

    Ok(())
}
fn add_required_tags_to_stream(
    input_stream: &mut dyn CAIRead,
    output_stream: &mut dyn CAIReadWrite,
) -> Result<()> {
    let tiff_io = TiffIO {};

    match tiff_io.read_cai(input_stream) {
        Ok(_) => {
            // just clone
            input_stream.rewind()?;
            output_stream.rewind()?;
            std::io::copy(input_stream, output_stream)?;
            Ok(())
        }
        Err(Error::JumbfNotFound) => {
            // allocate enough bytes so that value is not stored in offset field
            let some_bytes = vec![0u8; 10];
            let tio = TiffIO {};
            tio.write_cai(input_stream, output_stream, &some_bytes)
        }
        Err(e) => Err(e),
    }
}

fn get_cai_data<R>(mut asset_reader: &mut R) -> Result<Vec<u8>>
where
    R: Read + Seek + ?Sized,
{
    let (tiff_tree, page_0, e, big_tiff) = map_tiff(asset_reader)?;

    let first_ifd = &tiff_tree[page_0].data;

    let cai_ifd_entry = first_ifd.get_tag(C2PA_TAG).ok_or(Error::JumbfNotFound)?;

    // make sure data type is for unstructured data
    if cai_ifd_entry.entry_type != C2PA_FIELD_TYPE {
        return Err(Error::InvalidAsset(
            "Ifd entry for C2PA must be type UNDEFINED(7)".to_string(),
        ));
    }

    // move read point to start of entry
    let decoded_offset = decode_offset(cai_ifd_entry.value_offset, e, big_tiff)?;
    asset_reader.seek(SeekFrom::Start(decoded_offset))?;

    let data = asset_reader
        .read_to_vec(cai_ifd_entry.value_count)
        .map_err(|_err| Error::InvalidAsset("TIFF/DNG out of range".to_string()))?;

    Ok(data)
}

fn get_xmp_data<R>(mut asset_reader: &mut R) -> Option<Vec<u8>>
where
    R: Read + Seek + ?Sized,
{
    let (tiff_tree, page_0, e, big_tiff) = map_tiff(asset_reader).ok()?;
    let first_ifd = &tiff_tree[page_0].data;

    let xmp_ifd_entry = first_ifd.get_tag(XMP_TAG)?;
    // make sure the tag type is correct
    if IFDEntryType::from_u16(xmp_ifd_entry.entry_type)? != IFDEntryType::Byte {
        return None;
    }

    // move read point to start of entry
    let decoded_offset = decode_offset(xmp_ifd_entry.value_offset, e, big_tiff).ok()?;
    asset_reader.seek(SeekFrom::Start(decoded_offset)).ok()?;

    asset_reader.read_to_vec(xmp_ifd_entry.value_count).ok()
}
pub struct TiffIO {}

impl CAIReader for TiffIO {
    fn read_cai(&self, asset_reader: &mut dyn CAIRead) -> Result<Vec<u8>> {
        let cai_data = get_cai_data(asset_reader)?;
        Ok(cai_data)
    }

    fn read_xmp(&self, asset_reader: &mut dyn CAIRead) -> Option<String> {
        let xmp_data = get_xmp_data(asset_reader)?;
        String::from_utf8(xmp_data).ok()
    }
}

impl AssetIO for TiffIO {
    fn asset_patch_ref(&self) -> Option<&dyn AssetPatch> {
        Some(self)
    }

    fn read_cai_store(&self, asset_path: &std::path::Path) -> Result<Vec<u8>> {
        let mut reader = std::fs::File::open(asset_path)?;

        self.read_cai(&mut reader)
    }

    fn save_cai_store(&self, asset_path: &std::path::Path, store_bytes: &[u8]) -> Result<()> {
        let mut input_stream = std::fs::OpenOptions::new()
            .read(true)
            .open(asset_path)
            .map_err(Error::IoError)?;

        let mut temp_file = tempfile_builder("c2pa_temp")?;

        self.write_cai(&mut input_stream, &mut temp_file, store_bytes)?;

        // copy temp file to asset
        rename_or_move(temp_file, asset_path)
    }

    fn get_object_locations(
        &self,
        asset_path: &std::path::Path,
    ) -> Result<Vec<crate::asset_io::HashObjectPositions>> {
        let mut input_stream =
            std::fs::File::open(asset_path).map_err(|_err| Error::EmbeddingError)?;

        self.get_object_locations_from_stream(&mut input_stream)
    }

    fn remove_cai_store(&self, asset_path: &std::path::Path) -> Result<()> {
        let mut input_file = std::fs::File::open(asset_path)?;

        let mut temp_file = tempfile_builder("c2pa_temp")?;

        self.remove_cai_store_from_stream(&mut input_file, &mut temp_file)?;

        // copy temp file to asset
        rename_or_move(temp_file, asset_path)
    }

    fn new(_asset_type: &str) -> Self
    where
        Self: Sized,
    {
        TiffIO {}
    }

    fn get_handler(&self, asset_type: &str) -> Box<dyn AssetIO> {
        Box::new(TiffIO::new(asset_type))
    }

    fn get_reader(&self) -> &dyn CAIReader {
        self
    }

    fn get_writer(&self, asset_type: &str) -> Option<Box<dyn CAIWriter>> {
        if SUPPORTED_WRITER_TYPES.contains(&asset_type) {
            Some(Box::new(TiffIO::new(asset_type)))
        } else {
            None
        }
    }

    fn remote_ref_writer_ref(&self) -> Option<&dyn RemoteRefEmbed> {
        Some(self)
    }

    fn composed_data_ref(&self) -> Option<&dyn ComposedManifestRef> {
        Some(self)
    }

    fn supported_types(&self) -> &[&str] {
        &SUPPORTED_TYPES
    }
}

impl CAIWriter for TiffIO {
    fn write_cai(
        &self,
        input_stream: &mut dyn CAIRead,
        output_stream: &mut dyn CAIReadWrite,
        store_bytes: &[u8],
    ) -> Result<()> {
        let l = u64::value_from(store_bytes.len())
            .map_err(|_err| Error::InvalidAsset("value out of range".to_string()))?;

        let entry = IfdClonedEntry {
            entry_tag: C2PA_TAG,
            entry_type: C2PA_FIELD_TYPE,
            value_count: l,
            value_bytes: store_bytes.to_vec(),
        };

        tiff_clone_with_tags(output_stream, input_stream, vec![entry])
    }

    fn get_object_locations_from_stream(
        &self,
        input_stream: &mut dyn CAIRead,
    ) -> Result<Vec<HashObjectPositions>> {
        let len = stream_len(input_stream)?;
        let vec_cap = usize::value_from(len)
            .map_err(|_err| Error::InvalidAsset("value out of range".to_owned()))?;
        let output_buf: Vec<u8> = Vec::with_capacity(vec_cap + 100);

        let mut output_stream = Cursor::new(output_buf);

        add_required_tags_to_stream(input_stream, &mut output_stream)?;
        output_stream.rewind()?;

        let (idfs, first_idf_token, e, big_tiff) = map_tiff(&mut output_stream)?;

        let cai_ifd_entry = match idfs[first_idf_token].data.get_tag(C2PA_TAG) {
            Some(ifd) => ifd,
            None => return Ok(Vec::new()),
        };

        // make sure data type is for unstructured data
        if cai_ifd_entry.entry_type != C2PA_FIELD_TYPE {
            return Err(Error::InvalidAsset(
                "Ifd entry for C2PA must be type UNKNOWN(7)".to_string(),
            ));
        }

        let decoded_offset = decode_offset(cai_ifd_entry.value_offset, e, big_tiff)?;
        let manifest_offset = usize::value_from(decoded_offset)
            .map_err(|_err| Error::InvalidAsset("TIFF/DNG out of range".to_string()))?;
        let manifest_len = usize::value_from(cai_ifd_entry.value_count)
            .map_err(|_err| Error::InvalidAsset("TIFF/DNG out of range".to_string()))?;

        Ok(vec![HashObjectPositions {
            offset: manifest_offset,
            length: manifest_len,
            htype: HashBlockObjectType::Cai,
        }])
    }

    fn remove_cai_store_from_stream(
        &self,
        input_stream: &mut dyn CAIRead,
        output_stream: &mut dyn CAIReadWrite,
    ) -> Result<()> {
        let (mut idfs, page_0, e, big_tiff) = map_tiff(input_stream)?;

        let mut bo = ByteOrdered::new(output_stream, e);
        let mut tc = TiffCloner::new(e, big_tiff, &mut bo)?;

        idfs[page_0].data.entries.remove(&C2PA_TAG);
        tc.clone_tiff(&mut idfs, page_0, input_stream)?;
        Ok(())
    }
}

impl AssetPatch for TiffIO {
    fn patch_cai_store(&self, asset_path: &std::path::Path, store_bytes: &[u8]) -> Result<()> {
        let mut asset_io = OpenOptions::new()
            .write(true)
            .read(true)
            .create(false)
            .open(asset_path)?;

        let (tiff_tree, page_0, e, big_tiff) = map_tiff(&mut asset_io)?;

        let first_ifd = &tiff_tree[page_0].data;

        let cai_ifd_entry = first_ifd.get_tag(C2PA_TAG).ok_or(Error::JumbfNotFound)?;

        // make sure data type is for unstructured data
        if cai_ifd_entry.entry_type != C2PA_FIELD_TYPE {
            return Err(Error::InvalidAsset(
                "Ifd entry for C2PA must be type UNKNOWN(7)".to_string(),
            ));
        }

        let manifest_len: usize = usize::value_from(cai_ifd_entry.value_count)
            .map_err(|_err| Error::InvalidAsset("TIFF/DNG out of range".to_string()))?;

        if store_bytes.len() == manifest_len {
            // move read point to start of entry
            let decoded_offset = decode_offset(cai_ifd_entry.value_offset, e, big_tiff)?;
            asset_io.seek(SeekFrom::Start(decoded_offset))?;

            asset_io.write_all(store_bytes)?;
            Ok(())
        } else {
            Err(Error::InvalidAsset(
                "patch_cai_store store size mismatch.".to_string(),
            ))
        }
    }
}

impl RemoteRefEmbed for TiffIO {
    #[allow(unused_variables)]
    fn embed_reference(
        &self,
        asset_path: &Path,
        embed_ref: crate::asset_io::RemoteRefEmbedType,
    ) -> Result<()> {
        match embed_ref {
            crate::asset_io::RemoteRefEmbedType::Xmp(manifest_uri) => {
                let output_buf = Vec::new();
                let mut output_stream = Cursor::new(output_buf);

                // block so that source file is closed after embed
                {
                    let mut source_stream = std::fs::File::open(asset_path)?;
                    self.embed_reference_to_stream(
                        &mut source_stream,
                        &mut output_stream,
                        RemoteRefEmbedType::Xmp(manifest_uri),
                    )?;
                }

                // write will replace exisiting contents
                output_stream.rewind()?;
                std::fs::write(asset_path, output_stream.into_inner())?;
                Ok(())
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
                let xmp = match self.get_reader().read_xmp(source_stream) {
                    Some(xmp) => add_provenance(&xmp, &manifest_uri)?,
                    None => {
                        let xmp = MIN_XMP.to_string();
                        add_provenance(&xmp, &manifest_uri)?
                    }
                };

                let l = u64::value_from(xmp.len())
                    .map_err(|_err| Error::InvalidAsset("value out of range".to_string()))?;

                let entry = IfdClonedEntry {
                    entry_tag: XMP_TAG,
                    entry_type: IFDEntryType::Byte as u16,
                    value_count: l,
                    value_bytes: xmp.as_bytes().to_vec(),
                };
                tiff_clone_with_tags(output_stream, source_stream, vec![entry])
            }
            crate::asset_io::RemoteRefEmbedType::StegoS(_) => Err(Error::UnsupportedType),
            crate::asset_io::RemoteRefEmbedType::StegoB(_) => Err(Error::UnsupportedType),
            crate::asset_io::RemoteRefEmbedType::Watermark(_) => Err(Error::UnsupportedType),
        }
    }
}

impl ComposedManifestRef for TiffIO {
    // Return entire CAI block as Vec<u8>
    fn compose_manifest(&self, manifest_data: &[u8], _format: &str) -> Result<Vec<u8>> {
        Ok(manifest_data.to_vec())
    }
}

#[cfg(test)]
pub mod tests {
    #![allow(clippy::panic)]
    #![allow(clippy::unwrap_used)]

    use core::panic;

    use super::*;
    use crate::utils::{io_utils::tempdirectory, test::temp_dir_path};
    #[test]
    fn test_read_write_manifest() {
        let data = "some data";

        let source = crate::utils::test::fixture_path("TUSCANY.TIF");

        let temp_dir = tempdirectory().unwrap();
        let output = temp_dir_path(&temp_dir, "test.tif");

        std::fs::copy(source, &output).unwrap();

        let tiff_io = TiffIO {};

        // save data to tiff
        tiff_io.save_cai_store(&output, data.as_bytes()).unwrap();

        // read data back
        let loaded = tiff_io.read_cai_store(&output).unwrap();

        assert_eq!(&loaded, data.as_bytes());
    }

    #[test]
    fn test_write_xmp() {
        let data = "some data";

        let source = crate::utils::test::fixture_path("TUSCANY.TIF");

        let temp_dir = tempdirectory().unwrap();
        let output = temp_dir_path(&temp_dir, "test.tif");

        std::fs::copy(source, &output).unwrap();

        let tiff_io = TiffIO {};

        // save data to tiff
        let eh = tiff_io.remote_ref_writer_ref().unwrap();
        eh.embed_reference(&output, RemoteRefEmbedType::Xmp(data.to_string()))
            .unwrap();

        // read data back
        let mut output_stream = std::fs::File::open(&output).unwrap();
        let xmp = tiff_io.read_xmp(&mut output_stream).unwrap();
        let loaded = crate::utils::xmp_inmemory_utils::extract_provenance(&xmp).unwrap();

        assert_eq!(&loaded, data);
    }

    #[test]
    fn test_remove_manifest() {
        let data = "some data";

        let source = crate::utils::test::fixture_path("TUSCANY.TIF");

        let temp_dir = tempdirectory().unwrap();
        let output = temp_dir_path(&temp_dir, "test.tif");

        std::fs::copy(source, &output).unwrap();

        let tiff_io = TiffIO {};

        // first make sure that calling this without a manifest does not error
        tiff_io.remove_cai_store(&output).unwrap();

        // save data to tiff
        tiff_io.save_cai_store(&output, data.as_bytes()).unwrap();

        // read data back
        let loaded = tiff_io.read_cai_store(&output).unwrap();

        assert_eq!(&loaded, data.as_bytes());

        tiff_io.remove_cai_store(&output).unwrap();

        match tiff_io.read_cai_store(&output) {
            Err(Error::JumbfNotFound) => (),
            _ => panic!("should be no C2PA store"),
        }
    }

    #[test]
    fn test_get_object_location() {
        let data = "some data";

        let source = crate::utils::test::fixture_path("TUSCANY.TIF");

        let temp_dir = tempdirectory().unwrap();
        let output = temp_dir_path(&temp_dir, "test.tif");

        std::fs::copy(source, &output).unwrap();

        let tiff_io = TiffIO {};

        // save data to tiff
        tiff_io.save_cai_store(&output, data.as_bytes()).unwrap();

        // read data back
        let loaded = tiff_io.read_cai_store(&output).unwrap();

        assert_eq!(&loaded, data.as_bytes());

        let mut success = false;
        if let Ok(locations) = tiff_io.get_object_locations(&output) {
            for op in locations {
                if op.htype == HashBlockObjectType::Cai {
                    let mut of = std::fs::File::open(&output).unwrap();

                    let mut manifests_buf: Vec<u8> = vec![0u8; op.length];
                    of.seek(SeekFrom::Start(op.offset as u64)).unwrap();
                    of.read_exact(manifests_buf.as_mut_slice()).unwrap();
                    if crate::hash_utils::vec_compare(&manifests_buf, data.as_bytes()) {
                        success = true;
                    }
                }
            }
        }
        assert!(success);
    }

    #[test]
    fn test_overflow_clone_ifd_entries() {
        let data = [
            0x49, 0x49, 0x2b, 0x00, 0x08, 0x00, 0x00, 0x00, 0x31, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x49,
            0x49, 0x2a, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0xf9, 0x00,
            0x00, 0x00, 0x00, 0x05, 0x00, 0x07, 0x00, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, //
            // entry
            //
            0x00, 0x00, // entry_tag
            0x04, 0x00, // entry_type
            0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x80, // value_count (cnt)
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // value_offset
            //
            // entry
            //
            0x00, 0x00, // entry_tag
            0x04, 0x00, // entry_type
            0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x80, // value_count (cnt)
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // value_offset
            //
            // ...
            //
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00,
        ];

        let mut stream = Cursor::new(&data);

        let tiff_io = TiffIO {};

        let locations = tiff_io.get_object_locations_from_stream(&mut stream);
        assert!(matches!(locations, Err(Error::InvalidAsset(_))));
    }

    /*  disable until I find smaller DNG
    #[test]
    fn test_read_write_dng_manifest() {
        let data = "some data";

        let source = crate::utils::test::fixture_path("test.DNG");
        //let source = crate::utils::test::fixture_path("sample1.dng");

        let temp_dir = tempdirectory().unwrap();
        let output = temp_dir_path(&temp_dir, "test.DNG");

        std::fs::copy(&source, &output).unwrap();

        let tiff_io = TiffIO {};

        // save data to tiff
        tiff_io.save_cai_store(&output, data.as_bytes()).unwrap();

        // read data back
        println!("Reading TIFF");
        let loaded = tiff_io.read_cai_store(&output).unwrap();

        assert_eq!(&loaded, data.as_bytes());
    }
    #[test]
    fn test_read_write_dng_parse() {
        //let data = "some data";

        let source = crate::utils::test::fixture_path("test.DNG");
        let mut f = std::fs::File::open(&source).unwrap();

        let (idfs, token, _endianness, _big_tiff) = map_tiff(&mut f).unwrap();

        println!("IFD {}", idfs[token].data.entry_cnt);
    }
    */
}
