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
    convert::TryFrom,
    io::{Read, Seek, SeekFrom, Write},
    mem::size_of,
    num::Wrapping,
    str::from_utf8,
};

use byteorder::{BigEndian, ReadBytesExt, WriteBytesExt};

use crate::error::{Error, Result};

/// Types for supporting fonts in any container.
/// Four-character tag which names a font table.
#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SfntTag {
    pub data: [u8; 4],
}

#[allow(dead_code)] // TBD - Is creating some UTs sufficient to quicken/animate this code?
impl SfntTag {
    /// Construct a new SfntTag with the given value.
    pub fn new(source_data: [u8; 4]) -> Self {
        Self { data: source_data }
    }

    /// Read a new SfntTag from the given source.
    pub fn new_from_reader<T: Read + Seek + ?Sized>(reader: &mut T) -> Result<Self> {
        Ok(Self::new([
            reader.read_u8()?,
            reader.read_u8()?,
            reader.read_u8()?,
            reader.read_u8()?,
        ]))
    }

    /// Serialize this tag data to the given writer.
    pub fn write<TDest: Write + ?Sized>(&self, destination: &mut TDest) -> Result<()> {
        destination.write_all(&self.data)?;
        Ok(())
    }
}

impl std::fmt::Display for SfntTag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}{}{}{}",
            self.data[0], self.data[1], self.data[2], self.data[3]
        )
    }
}

impl std::fmt::Debug for SfntTag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}{}{}{}",
            self.data[0], self.data[1], self.data[2], self.data[3]
        )
    }
}

/// Font storage frequently requires padding things to four-byte boundaries.
pub fn round_up_to_four(size: usize) -> usize {
    (size + 3) & (!3)
}

/// 32-bit font-format identification magic number.
///
/// Note that Embedded OpenType and MicroType Express formats cannot be detected
/// with a simple magic-number sniff. Conceivably, EOT could be dealt with as a
/// variation on SFNT, but MTX will needs more exotic handling.
pub enum Magic {
    /// 'OTTO' - OpenType
    OpenType = 0x4f54544f,
    /// FIXED 1.0 - TrueType (or possibly v1.0 Embedded OpenType)
    TrueType = 0x00010000,
    /// 'typ1' - PostScript Type 1
    PostScriptType1 = 0x74797031,
    /// 'true' - TrueType fonts for OS X / iOS
    AppleTrue = 0x74727565,
    /// 'wOFF' - WOFF 1.0
    Woff = 0x774f4646,
    /// 'wOF2' - WOFF 2.0
    Woff2 = 0x774f4632,
}

/// Tags for the font tables we care about.

/// Tag for the 'C2PA' table.
#[allow(dead_code)]
pub const C2PA_TABLE_TAG: SfntTag = SfntTag { data: *b"C2PA" };

/// Tag for the 'head' table in a font.
#[allow(dead_code)]
pub const HEAD_TABLE_TAG: SfntTag = SfntTag { data: *b"head" };

/// Spec-mandated value for 'head'::magicNumber
#[allow(dead_code)]
pub const HEAD_MAGIC_NUMBER: u32 = 0xb1b0afba;

/// Used to attempt conversion from u32 to a Magic value.
impl TryFrom<u32> for Magic {
    type Error = crate::error::Error;

    /// Tries to convert from u32 to a valid font version.
    fn try_from(v: u32) -> core::result::Result<Self, Self::Error> {
        match v {
            ot if ot == Magic::OpenType as u32 => Ok(Magic::OpenType),
            tt if tt == Magic::TrueType as u32 => Ok(Magic::TrueType),
            t1 if t1 == Magic::PostScriptType1 as u32 => Ok(Magic::PostScriptType1),
            at if at == Magic::AppleTrue as u32 => Ok(Magic::AppleTrue),
            w1 if w1 == Magic::Woff as u32 => Ok(Magic::Woff),
            w2 if w2 == Magic::Woff2 as u32 => Ok(Magic::Woff2),
            _unknown => Err(Error::FontUnknownMagic),
        }
    }
}

/// Function to assemble two u16s into a u32 - useful for check summing
// TBD - Supposedly the bytemuck crate, already in this project, can help us
// with stuff like this.
#[allow(dead_code)]
pub fn u32_from_u16_pair(hi: u16, lo: u16) -> Wrapping<u32> {
    Wrapping((hi as u32 * 65536) + lo as u32)
}

/// Function to get high-order u32 from given u64
#[allow(dead_code)]
pub fn u32_from_u64_hi(big: u64) -> Wrapping<u32> {
    Wrapping(((big & 0xffffffff00000000) >> 32) as u32)
}

/// Function to get low-order u32 from given u64
#[allow(dead_code)]
pub fn u32_from_u64_lo(big: u64) -> Wrapping<u32> {
    Wrapping((big & 0x00000000ffffffff) as u32)
}

/// 'C2PA' font table - in storage
#[derive(Debug, Default)]
#[repr(C, packed(4))] // As defined by the C2PA spec.
#[allow(non_snake_case)] // As named by the C2PA spec.
pub struct TableC2PARaw {
    pub majorVersion: u16,
    pub minorVersion: u16,
    pub activeManifestUriOffset: u32,
    pub activeManifestUriLength: u16,
    pub reserved: u16,
    pub manifestStoreOffset: u32,
    pub manifestStoreLength: u32,
}

impl TableC2PARaw {
    pub fn new() -> Self {
        Self {
            majorVersion: 0,
            minorVersion: 1,
            ..Default::default()
        }
    }

    pub fn new_from_reader<T: Read + Seek + ?Sized>(reader: &mut T) -> Result<Self> {
        Ok(Self {
            majorVersion: reader.read_u16::<BigEndian>()?,
            minorVersion: reader.read_u16::<BigEndian>()?,
            activeManifestUriOffset: reader.read_u32::<BigEndian>()?,
            activeManifestUriLength: reader.read_u16::<BigEndian>()?,
            reserved: reader.read_u16::<BigEndian>()?,
            manifestStoreOffset: reader.read_u32::<BigEndian>()?,
            manifestStoreLength: reader.read_u32::<BigEndian>()?,
        })
    }

    pub fn write<TDest: Write + ?Sized>(&mut self, destination: &mut TDest) -> Result<()> {
        destination.write_u16::<BigEndian>(self.majorVersion)?;
        destination.write_u16::<BigEndian>(self.minorVersion)?;
        destination.write_u32::<BigEndian>(self.activeManifestUriOffset)?;
        destination.write_u16::<BigEndian>(self.activeManifestUriLength)?;
        destination.write_u16::<BigEndian>(self.reserved)?;
        destination.write_u32::<BigEndian>(self.manifestStoreOffset)?;
        destination.write_u32::<BigEndian>(self.manifestStoreLength)?;
        Ok(())
    }
}

/// 'C2PA' font table - after loading from storage
#[derive(Clone, Debug)]
pub struct TableC2PA {
    /// Major version of the C2PA table record
    pub major_version: u16,
    /// Minor version of the C2PA table record
    pub minor_version: u16,
    /// Optional URI to an active manifest
    pub active_manifest_uri: Option<String>,
    /// Optional embedded manifest store
    pub manifest_store: Option<Vec<u8>>,
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
    pub fn checksum(&self) -> Wrapping<u32> {
        // // Serialize self to a throwaway stream
        // let mut stream = Cursor::new(Vec::new());
        // match self.write(&mut stream) {
        //     Ok(()) => (),
        //     Err(error) => return Err(error),
        // }
        // // Compute checksum of stream
        // stream.seek(SeekFrom::Start(0)).unwrap();
        // let mut cksum: u32 = 0;
        // while stream.get_ref().len() > 4 {
        //     let ckword: u32 = stream.read_u32::<BigEndian>()?;
        //     cksum += ckword;
        // }
        // if stream.get_ref().len() > 0 {
        //     let mut ckfrag: u32 = 0;
        //     let mut factor: u32 = 256 * 256 * 256;
        //     while stream.get_ref().len() > 0 {
        //         let ckbyte = stream.read_u8()?;
        //         ckfrag += ckbyte as u32 * factor;
        //         factor /= 256;
        //     }
        //     cksum += ckfrag;
        // }
        Wrapping(0x12345678)
    }

    /// Size of this table if it were serialized right now
    pub fn len(&self) -> usize {
        size_of::<TableC2PARaw>()
            + match &self.active_manifest_uri {
                Some(uri) => uri.len(),
                None => 0,
            }
            + match &self.manifest_store {
                Some(store) => store.len(),
                None => 0,
            }
    }

    /// Creates a new C2PA table from the given stream.
    pub fn make_from_reader<T: Read + Seek + ?Sized>(
        reader: &mut T,
        offset: u64,
        size: usize,
    ) -> core::result::Result<TableC2PA, Error> {
        if size < size_of::<TableC2PARaw>() {
            Err(Error::FontLoadError)
        } else {
            let mut active_manifest_uri: Option<String> = None;
            let mut manifest_store: Option<Vec<u8>> = None;
            // Read the initial fixed-sized portion of the table
            reader.seek(SeekFrom::Start(offset))?;
            let raw_table = TableC2PARaw::new_from_reader(reader)?;
            // Check parameters
            if size
                < size_of::<TableC2PARaw>()
                    + raw_table.activeManifestUriLength as usize
                    + raw_table.manifestStoreLength as usize
            {
                return Err(Error::FontLoadError);
            }
            // If a remote manifest URI is present, unpack it from the remaining
            // data in the table.
            if raw_table.activeManifestUriLength > 0 {
                let mut uri_bytes: Vec<u8> = vec![0; raw_table.activeManifestUriLength as usize];
                reader.seek(SeekFrom::Start(
                    offset + raw_table.activeManifestUriOffset as u64,
                ))?;
                reader.read_exact(&mut uri_bytes)?;
                active_manifest_uri = Some(
                    from_utf8(&uri_bytes)
                        .map_err(|_e| Error::FontLoadError)?
                        .to_string(),
                );
            }
            if raw_table.manifestStoreLength > 0 {
                let mut mani_bytes: Vec<u8> = vec![0; raw_table.manifestStoreLength as usize];
                reader.seek(SeekFrom::Start(
                    offset + raw_table.manifestStoreOffset as u64,
                ))?;
                reader.read_exact(&mut mani_bytes)?;
                manifest_store = Some(mani_bytes);
            }
            // Return our record
            Ok(TableC2PA {
                major_version: raw_table.majorVersion,
                minor_version: raw_table.minorVersion,
                active_manifest_uri,
                manifest_store,
            })
        }
    }

    /// Get the manifest store data if available
    pub fn get_manifest_store(&self) -> Option<&[u8]> {
        self.manifest_store.as_deref()
    }

    /// Serialize this C2PA table to the given writer.
    pub fn write<TDest: Write + ?Sized>(&self, destination: &mut TDest) -> Result<()> {
        // Set up the structured data
        let mut raw_table = TableC2PARaw::new();
        // If a remote URI is present, prepare to store it.
        if let Some(uri_string) = self.active_manifest_uri.as_ref() {
            raw_table.activeManifestUriOffset = size_of::<TableC2PARaw>() as u32;
            raw_table.activeManifestUriLength = uri_string.len() as u16;
        }
        // If a local store is present, prepare to store it.
        if let Some(manifest_store) = self.manifest_store.as_ref() {
            raw_table.manifestStoreOffset =
                size_of::<TableC2PARaw>() as u32 + raw_table.activeManifestUriLength as u32;
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
            major_version: 1,
            minor_version: 4,
            active_manifest_uri: Default::default(),
            manifest_store: Default::default(),
        }
    }
}

/// 'head' font table. For now, there is no need for a 'raw' variant, since only
/// byte-swapping is needed.
#[derive(Debug, Default)]
#[repr(C, packed(1))]
// As defined by Open Font Format / OpenType (though we don't as yet directly
// support exotics like FIXED).
#[allow(non_snake_case)] // As named by Open Font Format / OpenType.
pub struct TableHead {
    pub majorVersion: u16,
    pub minorVersion: u16,
    pub fontRevision: u32,
    pub checksumAdjustment: u32,
    pub magicNumber: u32,
    pub flags: u16,
    pub unitsPerEm: u16,
    pub created: i64,
    pub modified: i64,
    pub xMin: i16,
    pub yMin: i16,
    pub xMax: i16,
    pub yMax: i16,
    pub macStyle: u16,
    pub lowestRecPPEM: u16,
    pub fontDirectionHint: i16,
    pub indexToLocFormat: i16,
    pub glyphDataFormat: i16,
}

impl TableHead {
    /// Creates a `head` table from the given stream.
    pub fn make_from_reader<T: Read + Seek + ?Sized>(
        reader: &mut T,
        offset: u64,
        size: usize,
    ) -> core::result::Result<TableHead, Error> {
        reader.seek(SeekFrom::Start(offset))?;
        let actual_size = size_of::<TableHead>();
        if size != actual_size {
            Err(Error::FontLoadError)
        } else {
            let head = Ok(Self {
                // 0x00
                majorVersion: reader.read_u16::<BigEndian>()?,
                minorVersion: reader.read_u16::<BigEndian>()?,
                // 0x04
                fontRevision: reader.read_u32::<BigEndian>()?,
                // 0x08
                checksumAdjustment: reader.read_u32::<BigEndian>()?,
                // TBD - FontLoadError when head magic is bad?
                magicNumber: reader.read_u32::<BigEndian>()?,
                // 0x10
                flags: reader.read_u16::<BigEndian>()?,
                unitsPerEm: reader.read_u16::<BigEndian>()?,
                // 0x14
                created: reader.read_i64::<BigEndian>()?,
                // 0x1c
                modified: reader.read_i64::<BigEndian>()?,
                // 0x24
                xMin: reader.read_i16::<BigEndian>()?,
                yMin: reader.read_i16::<BigEndian>()?,
                // 0x28
                xMax: reader.read_i16::<BigEndian>()?,
                yMax: reader.read_i16::<BigEndian>()?,
                // 0x2c
                macStyle: reader.read_u16::<BigEndian>()?,
                lowestRecPPEM: reader.read_u16::<BigEndian>()?,
                // 0x30
                fontDirectionHint: reader.read_i16::<BigEndian>()?,
                indexToLocFormat: reader.read_i16::<BigEndian>()?,
                // 0x34
                glyphDataFormat: reader.read_i16::<BigEndian>()?,
                // 0x36 - 54 bytes
                // TBD - Two bytes of padding to get to 56/0x38. Should we
                // seek/discard two more bytes, just to leave the stream in a
                // known state? Be nice if we didn't have to.
                //   1. On the one hand, whoever's invoking us could more-
                //      efficiently mess around with the offsets and padding.
                //   B. On the other, for the .write() code, we definitely push
                //      the "pad *yourself* up to four, impl!" approach
                //   III. Likewise the .checksum() code (although, because this
                //        is a simple checksum, the matter is moot; it doesn't
                //        matter whether we add '0_u16' to the total.
                //   IIII. (On clocks, IIII is a permissible Roman numeral) But
                //      what about that "simple" '.len()' call? Should it
                //      include the two pad bytes?
                // For now, the surrounding code doesn't care how the read
                // stream is left, so we don't do anything, since that is simplest.
            });
            let _two_bytes_from_54_to_56 = reader.read_i16::<BigEndian>()?;
            head
        }
    }

    /// Compute the checksum
    pub fn checksum(&self) -> Wrapping<u32> {
        // 0x00
        u32_from_u16_pair(self.majorVersion, self.minorVersion)
            // 0x04
            + Wrapping(self.fontRevision)
            // 0x08
            + Wrapping(self.checksumAdjustment)
            + Wrapping(self.magicNumber)
            // 0x10
            + u32_from_u16_pair(self.flags, self.unitsPerEm)
            // 0x14
            + u32_from_u64_hi(self.created as u64)
            + u32_from_u64_lo(self.created as u64)
            // 0x1c
            + u32_from_u64_hi(self.modified as u64)
            + u32_from_u64_lo(self.modified as u64)
            // 0x24
            + u32_from_u16_pair(self.xMin as u16, self.yMin as u16)
            // 0x28
            + u32_from_u16_pair(self.xMax as u16, self.yMax as u16)
            // 0x2c
            + u32_from_u16_pair(self.macStyle, self.lowestRecPPEM)
            // 0x30
            + u32_from_u16_pair(self.fontDirectionHint as u16, self.indexToLocFormat as u16)
            // 0x34
            + u32_from_u16_pair(self.glyphDataFormat as u16, 0_u16/*padpad*/)
        // 0x38
    }

    /// Size of this table if it were serialized right now
    pub fn len(&self) -> usize {
        // TBD - Is this called? Should it include the pad?
        size_of::<Self>()
    }

    /// Serialize this head table to the given writer.
    pub fn write<TDest: Write + ?Sized>(&self, destination: &mut TDest) -> Result<()> {
        // 0x00
        destination.write_u16::<BigEndian>(self.majorVersion)?;
        destination.write_u16::<BigEndian>(self.minorVersion)?;
        // 0x04
        destination.write_u32::<BigEndian>(self.fontRevision)?;
        // 0x08
        destination.write_u32::<BigEndian>(self.checksumAdjustment)?;
        destination.write_u32::<BigEndian>(self.magicNumber)?;
        // 0x10
        destination.write_u16::<BigEndian>(self.flags)?;
        destination.write_u16::<BigEndian>(self.unitsPerEm)?;
        // 0x14
        destination.write_i64::<BigEndian>(self.created)?;
        // 0x1c
        destination.write_i64::<BigEndian>(self.modified)?;
        // 0x24
        destination.write_i16::<BigEndian>(self.xMin)?;
        destination.write_i16::<BigEndian>(self.yMin)?;
        // 0x28
        destination.write_i16::<BigEndian>(self.xMax)?;
        destination.write_i16::<BigEndian>(self.yMax)?;
        // 0x2c
        destination.write_u16::<BigEndian>(self.macStyle)?;
        destination.write_u16::<BigEndian>(self.lowestRecPPEM)?;
        // 0x30
        destination.write_i16::<BigEndian>(self.fontDirectionHint)?;
        destination.write_i16::<BigEndian>(self.indexToLocFormat)?;
        // 0x34
        destination.write_i16::<BigEndian>(self.glyphDataFormat)?;
        // 0x36
        destination.write_u16::<BigEndian>(0_u16)?;
        // 0x38 - two bytes to get 54-byte 'head' up to nice round 56 bytes
        Ok(())
    }
}

/// Generic font table with unknown contents.
#[derive(Debug)]
pub struct TableUnspecified {
    pub data: Vec<u8>,
}

/// Any font table.
impl TableUnspecified {
    /// Creates an unspecified table from the given stream.
    pub fn make_from_reader<T: Read + Seek + ?Sized>(
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

    /// Compute the checksum
    pub fn checksum(&self) -> Wrapping<u32> {
        // TBD - this should never be called though - we only need to alter C2PA
        // and head - should we panic!(), or just implement?
        Wrapping(0x19283746)
    }

    /// Size of this table if it were serialized right now
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// Serialize this table data to the given writer.
    pub fn write<TDest: Write + ?Sized>(&self, destination: &mut TDest) -> Result<()> {
        destination
            .write_all(&self.data[..])
            .map_err(|_e| Error::FontSaveError)?;
        let limit = self.data.len() % 4;
        if limit > 0 {
            let pad: [u8; 3] = [0, 0, 0];
            destination
                .write_all(&pad[0..(4 - limit)])
                .map_err(|_e| Error::FontSaveError)?;
        }
        Ok(())
    }
}

/// Possible tables
#[derive(Debug)]
pub enum Table {
    /// 'C2PA' table
    C2PA(TableC2PA),
    /// 'head' table
    Head(TableHead),
    /// any other table
    Unspecified(TableUnspecified),
}

// TBD - This looks sort of like the CRTP from C++; do we want a Trait here
// that *both* table *and* its value-types implement?
impl Table {
    pub fn len(&self) -> usize {
        match self {
            Table::C2PA(c2pa) => c2pa.len(),
            Table::Head(head) => head.len(),
            Table::Unspecified(un) => un.len(),
        }
    }

    pub fn checksum(&self) -> Wrapping<u32> {
        match self {
            Table::C2PA(c2pa) => c2pa.checksum(),
            Table::Head(head) => head.checksum(),
            Table::Unspecified(un) => un.checksum(),
        }
    }
}

/// All the serialization structures so far have been defined using native
/// Rust types; should we go all-out in the other direction, and establish a
/// layer of "font" types (FWORD, FIXED, etc.)?

/// SFNT header, from the OpenType spec.
///
/// This SFNT type is also referenced by WOFF formats, so it is defined here for
/// common use.
#[derive(Copy, Clone, Debug)]
#[repr(C, packed(4))] // As defined by the OpenType spec.
#[allow(dead_code, non_snake_case)] // As defined by the OpenType spec.
pub struct SfntHeader {
    pub sfntVersion: u32,
    pub numTables: u16,
    pub searchRange: u16,
    pub entrySelector: u16,
    pub rangeShift: u16,
}

/// SFNT Table Directory Entry, from the OpenType spec.
///
/// This SFNT type is also referenced by WOFF formats, so it is defined here for
/// common use.
#[derive(Copy, Clone, Debug)]
#[repr(C, packed(4))] // As defined by the OpenType spec.
#[allow(dead_code, non_snake_case)] // As defined by the OpenType spec.
pub struct SfntTableDirEntry {
    pub tag: SfntTag,
    pub checksum: u32,
    pub offset: u32,
    pub length: u32,
}
