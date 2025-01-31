use byteorder::{LittleEndian, ReadBytesExt};
use std::io::{Cursor, Seek, SeekFrom};
use thiserror::Error;

use log::{log, Level};

pub type CrsResult<T> = std::result::Result<T, CrsError>;

// horizontal and optional vertical crs
pub type EPSG = (u16, Option<u16>);

#[derive(Error, Debug)]
pub enum CrsError {
    #[error("No crs vlrs")]
    NoCrs,
    #[error("User Defined crs, not implemented")]
    UserDefinedCrs,
    #[error("Wkt vlr found, but not able to parse")]
    UnreadableWktCrs,
    #[error("Geotiff vlr found, but not able to parse")]
    UnreadableGeotiffCrs,
    #[error("Invalid key for geotiff data")]
    UndefinedDataForGeoTiffKey(u16),
    #[error("The crs parser does not handle geotiff ascii and string defined CRS's")]
    UnimplementedForGeoTiffAsciiAndStringData(GeoTiffData),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub fn parse_las_crs(header: &las::Header) -> CrsResult<EPSG> {
    // ordered by record_id
    let mut crs_vlrs = [None, None, None, None];

    for vlr in header.all_vlrs() {
        if let ("lasf_projection", 2112 | 34735 | 34736 | 34737) =
            (vlr.user_id.to_lowercase().as_str(), vlr.record_id)
        {
            let pos = match vlr.record_id {
                2112 => 0,
                34735 => 1,
                34736 => 2,
                34737 => 3,
                _ => unreachable!(),
            };

            crs_vlrs[pos] = Some(vlr.data.clone());
        }
    }

    if crs_vlrs[0].is_some() {
        if !header.has_wkt_crs() {
            log!(
                Level::Warn,
                "WKT CRS VLR found, but header says it does not exists"
            );
        }
        get_wkt_epsg(crs_vlrs[0].clone().unwrap())
    } else if crs_vlrs[1].is_some() {
        if header.has_wkt_crs() {
            log!(
                Level::Warn,
                "No WKT CRS VLR found but header says it exists"
            );
        }
        get_geotiff_epsg(crs_vlrs)
    } else {
        if header.has_wkt_crs() {
            log!(
                Level::Warn,
                "No WKT CRS VLR found but header says it exists"
            );
        }
        Err(CrsError::NoCrs)
    }
}

/// find the epsg code located at the end of the WKT string
fn get_wkt_epsg(bytes: Vec<u8>) -> CrsResult<EPSG> {
    let mut epsg_code = 0;
    let mut has_code_started = false;
    let mut power = 0;
    for (i, byte) in bytes.into_iter().rev().enumerate() {
        if (48..=57).contains(&byte) {
            // the byte is an ASCII encoded number
            has_code_started = true;

            epsg_code += 10_u16.pow(power) * (byte - 48) as u16;
            power += 1;
        } else if has_code_started {
            break;
        }
        if i > 7 {
            // the code should be a 4 or 5 digit number starting at pos 2 or 3 from behind
            // meaning that if i has reached 8 something is wrong
            return Err(CrsError::UnreadableWktCrs);
        }
    }
    Ok((epsg_code, None))
}

/// Gets the EPSG code in the geotiff crs vlrs
/// returns a tuple containing the horizontal code and the optional vertical code
fn get_geotiff_epsg(vlrs: [Option<Vec<u8>>; 4]) -> CrsResult<EPSG> {
    let mut main_vlr = Cursor::new(vlrs[1].clone().unwrap());

    let double_vlr = vlrs[2].clone();
    let ascii_vlr = vlrs[3].clone();

    main_vlr.read_u16::<LittleEndian>()?; // always 1
    main_vlr.read_u16::<LittleEndian>()?; // always 1
    main_vlr.read_u16::<LittleEndian>()?; // always 0
    let num_keys = main_vlr.read_u16::<LittleEndian>()?;

    let crs_data = GeoTiffCRS::read_from(main_vlr, double_vlr, ascii_vlr, num_keys)?;

    let mut out = (None, None);
    for entry in crs_data.entries {
        match entry.id {
            // 3072 and 2048 should not co-exist, but might both be combined with 4096
            // 1024 should always exist
            1024 => match entry.data {
                GeoTiffData::U16(0) => return Err(CrsError::UnreadableGeotiffCrs),
                GeoTiffData::U16(1) => (), // projected crs
                GeoTiffData::U16(2) => (), // geographic coordinates
                GeoTiffData::U16(3) => (), // geographic + a vertical crs
                GeoTiffData::U16(32_767) => return Err(CrsError::UserDefinedCrs),
                p => return Err(CrsError::UnimplementedForGeoTiffAsciiAndStringData(p)),
            },
            3072 => {
                // projected crs
                if let GeoTiffData::U16(v) = entry.data {
                    out.0 = Some(v);
                } else {
                    // should probably add support for this
                    return Err(CrsError::UndefinedDataForGeoTiffKey(3072));
                }
            }
            2048 => {
                // geodetic crs
                if let GeoTiffData::U16(v) = entry.data {
                    out.0 = Some(v);
                } else {
                    // should probably add support for this
                    return Err(CrsError::UndefinedDataForGeoTiffKey(2048));
                }
            }
            4096 => {
                // vertical crs
                if let GeoTiffData::U16(v) = entry.data {
                    out.1 = Some(v);
                } else {
                    // should probably add support for this
                    return Err(CrsError::UndefinedDataForGeoTiffKey(4096));
                }
            }
            _ => (), // the rest are descriptions and units.
        }
    }
    if out.0.is_none() {
        return Err(CrsError::UnreadableGeotiffCrs);
    }
    Ok((out.0.unwrap(), out.1))
}

#[derive(Debug)]
pub struct GeoTiffCRS {
    pub entries: Vec<GeoTiffKeyEntry>,
}

impl GeoTiffCRS {
    fn read_from(
        mut main_vlr: Cursor<Vec<u8>>,
        double_vlr: Option<Vec<u8>>,
        ascii_vlr: Option<Vec<u8>>,
        count: u16,
    ) -> CrsResult<Self> {
        let mut entries = Vec::with_capacity(count as usize);
        for _ in 0..count {
            entries.push(GeoTiffKeyEntry::read_from(
                &mut main_vlr,
                &double_vlr,
                &ascii_vlr,
            )?);
        }
        Ok(GeoTiffCRS { entries })
    }
}

#[derive(Debug)]
pub struct GeoTiffKeyEntry {
    id: u16,
    data: GeoTiffData,
}

#[derive(Debug)]
pub enum GeoTiffData {
    U16(u16),
    String(String),
    Doubles(Vec<f64>),
}

impl GeoTiffKeyEntry {
    fn read_from(
        main_vlr: &mut Cursor<Vec<u8>>,
        double_vlr: &Option<Vec<u8>>,
        ascii_vlr: &Option<Vec<u8>>,
    ) -> CrsResult<Self> {
        let id = main_vlr.read_u16::<LittleEndian>()?;
        let location = main_vlr.read_u16::<LittleEndian>()?;
        let count = main_vlr.read_u16::<LittleEndian>()?;
        let offset = main_vlr.read_u16::<LittleEndian>()?;
        let data = match location {
            0 => GeoTiffData::U16(offset),
            34736 => {
                let mut cursor =
                    Cursor::new(double_vlr.as_ref().ok_or(CrsError::UnreadableGeotiffCrs)?);
                cursor.seek(SeekFrom::Start(offset as u64 * 8_u64))?; // 8 is the byte size of a f64 and offset is not a byte offset but an index
                let mut doubles = Vec::with_capacity(count as usize);
                for _ in 0..count {
                    doubles.push(cursor.read_f64::<LittleEndian>()?);
                }
                GeoTiffData::Doubles(doubles)
            }
            34737 => {
                let mut cursor =
                    Cursor::new(ascii_vlr.as_ref().ok_or(CrsError::UnreadableGeotiffCrs)?);
                cursor.seek(SeekFrom::Start(offset as u64))?; // no need to multiply the index as the byte size of char is 1
                let mut string = String::with_capacity(count as usize);
                for _ in 0..count {
                    string.push(cursor.read_u8()? as char);
                }
                GeoTiffData::String(string)
            }
            _ => return Err(CrsError::UndefinedDataForGeoTiffKey(id)),
        };
        Ok(GeoTiffKeyEntry { id, data })
    }
}
