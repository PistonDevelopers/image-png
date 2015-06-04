mod stream;

pub use self::stream::{StreamingDecoder, Decoded, DecodingError};
use self::stream::{CHUNCK_BUFFER_SIZE, get_info};

use std::mem;
use std::io::{Read, Write};
use std::convert::AsRef;

use traits::{HasParameters, Parameter};
use common::{ColorType, BitDepth, Info, Transformations};
use filter::{unfilter, FilterType};
use chunk::IDAT;
use utils;

/*
pub enum InterlaceHandling {
    /// Outputs the raw rows
    RawRows,
    /// Fill missing the pixels from the existing ones
    Rectangle,
    /// Only fill the needed pixels
    Sparkle
}

impl Parameter<Reader> for InterlaceHandling {
    fn set_param(self, this: &mut Reader) {
        this.color_output = self
    }
}*/


impl<R: Read> Parameter<Decoder<R>> for Transformations {
    fn set_param(self, this: &mut Decoder<R>) {
        this.transform = self
    }
}


/// Output info
pub struct OutputInfo {
    pub width: u32,
    pub height: u32,
    pub color_type: ColorType,
    pub bit_depth: BitDepth,
    pub line_size: usize,
}

impl OutputInfo {
    /// Returns the size needed to hold a decoded frame
    pub fn buffer_size(&self) -> usize {
        self.line_size * self.height as usize
    }
}

/// PNG Decoder
pub struct Decoder<R: Read> {
    /// Reader
    r: R,
    /// Output transformations
    transform: Transformations,
}

impl<R: Read> Decoder<R> {
    pub fn new(r: R) -> Decoder<R> {
        Decoder {
            r: r,
            transform: ::TRANSFORM_EXPAND | ::TRANSFORM_SCALE_16 | ::TRANSFORM_STRIP_16,
            
        }
    }
    
    /// Reads all meta data until the first IDAT chunk
    pub fn read_info(self) -> Result<(OutputInfo, Reader<R>), DecodingError> {
        let mut r = Reader::new(self.r, StreamingDecoder::new(), self.transform);
        try!(r.init());
        let (ct, bits) = r.output_color_type();
        let info = {
            let info = r.info();
            OutputInfo {
                width: info.width,
                height: info.height,
                color_type: ct,
                bit_depth: bits,
                line_size: r.output_line_size(info.width),
            }
        };
        Ok((info, r))
    }
}

impl<R: Read> HasParameters for Decoder<R> {}

/// PNG reader (mostly high-level interface)
///
/// Provides a high level that iterates over lines or whole images.
pub struct Reader<R: Read> {
    r: R,
    d: StreamingDecoder,
    eof: bool,
    /// Read buffer
    buf: Vec<u8>,
    /// Buffer position
    pos: usize,
    /// Buffer length
    end: usize,
    bpp: usize,
    rowlen: usize,
    adam7: Option<utils::Adam7Iterator>,
    /// Previous raw line
    prev: Vec<u8>,
    /// Current raw line
    current: Vec<u8>,
    /// Output transformations
    transform: Transformations,
    /// Processed line
    processed: Vec<u8>
}

macro_rules! get_info(
    ($this:expr) => {
        get_info(&$this.d).unwrap()
    }
);

impl<R: Read> Reader<R> {
    /// Creates a new PNG reader
    fn new(r: R, d: StreamingDecoder, t: Transformations) -> Reader<R> {
        Reader {
            r: r,
            d: d,
            eof: false,
            buf: vec![0; CHUNCK_BUFFER_SIZE],
            pos: 0,
            end: 0,
            bpp: 0,
            rowlen: 0,
            adam7: None,
            prev: Vec::new(),
            current: Vec::new(),
            transform: t,
            processed: Vec::new()
        }
    }
    
    /// Reads all meta data until the first IDAT chunk
    fn init(&mut self) -> Result<(), DecodingError> {
        use Decoded::*;
        if let Some(_) = get_info(&self.d) {
            Ok(())
        } else {
            loop {
                match try!(self.decode_next()) {
                    Some(ChunkBegin(_, IDAT)) => break,
                    None => return Err(DecodingError::Format(
                        "IDAT chunk missing".into()
                    )),
                    _ => (),
                }
            }
            self.allocate_out_buf();
            let info = match get_info(&self.d) {
                Some(info) => info,
                None => return Err(DecodingError::Format(
                  "IHDR chunk missing".into()
                ))
            };
            self.bpp = info.bytes_per_pixel();
            self.rowlen = info.raw_row_length();
            if info.interlaced {
                self.adam7 = Some(utils::Adam7Iterator::new(info.width, info.height))
            }
            self.prev = vec![0; self.rowlen];
            Ok(())
        }
    }
    
    pub fn info(&self) -> &Info {
        get_info!(self)
    } 
    
    /// Decodes the next frame into `buf`
    pub fn next_frame(&mut self, buf: &mut [u8]) -> Result<(), DecodingError> {
        // TODO 16 bit
        let (color_type, _) = self.output_color_type();
        let width = get_info!(self).width;
        if buf.len() < self.output_buffer_size() {
            return Err(DecodingError::Other(
                "supplied buffer is too small to hold the image".into()
            ))
        }
        if get_info!(self).interlaced {
             while let Some((row, adam7)) = try!(self.next_interlaced_row()) {
                 let (pass, line, _) = adam7.unwrap();
                 let bytes = color_type.samples() as u8;
                 utils::expand_pass(buf, width * bytes as u32, row, pass, line, bytes);
             }
        } else {
            let mut len = 0;
            while let Some(row) = try!(self.next_row()) {
                len += try!((&mut buf[len..]).write(row));
            }
        }
        Ok(())
    }
    
    /// Returns the next processed row of the image
    pub fn next_row(&mut self) -> Result<Option<&[u8]>, DecodingError> {
        self.next_interlaced_row().map(|v| v.map(|v| v.0))
    }
    
    /// Returns the next processed row of the image
    pub fn next_interlaced_row(&mut self) -> Result<Option<(&[u8], Option<(u8, u32, u32)>)>, DecodingError> {
        use common::ColorType::*;
        let transform = self.transform;
        let (color_type, bit_depth, trns) = {
            let info = get_info!(self);
            (info.color_type, info.bit_depth as u8, info.trns.is_some())
        };
        if transform == ::TRANSFORM_IDENTITY {
            self.next_raw_interlaced_row()
        } else {
            // swap buffer to circumvent borrow issues
            let mut buffer = mem::replace(&mut self.processed, Vec::new());
            let (got_next, adam7) = if let Some((row, adam7)) = try!(self.next_raw_interlaced_row()) {
                try!((&mut buffer[..]).write(row));
                (true, adam7)
            } else {
                (false, None)
            };
            // swap back
            let _ = mem::replace(&mut self.processed, buffer);
            if got_next {
                let old_len = self.processed.len();
                if let Some((_, _, width)) = adam7 {
                    let width = self.line_size(width);
                    self.processed.resize(width, 0);
                }
                let mut len = self.processed.len();
                if transform.contains(::TRANSFORM_EXPAND) {
                    match color_type {
                        Indexed => {
                            self.expand_paletted()
                        }
                        Grayscale | GrayscaleAlpha if bit_depth < 8 => self.expand_gray_u8(),
                        Grayscale | RGB if trns => {
                            let channels = color_type.samples();
                            let trns = get_info!(self).trns.as_ref().unwrap();
                            if bit_depth == 8 {
                                utils::expand_trns_line(&mut self.processed, &*trns, channels);
                            } else {
                                utils::expand_trns_line16(&mut self.processed, &*trns, channels);
                            }
                        },
                        _ => ()
                    }
                }
                if bit_depth == 16 && transform.intersects(::TRANSFORM_SCALE_16 | ::TRANSFORM_STRIP_16) {
                    len /= 2;
                    for i in 0..len {
                        self.processed[i] = self.processed[2 * i];
                    }
                }
                self.processed.resize(old_len, 0); // Interlace handling
                Ok(Some((
                    &self.processed[..len],
                    adam7
                )))
            } else {
                Ok(None)
            }
        }
    }
    
    /// Returns the color type and the number of bits per sample
    /// of the data returned by `Reader::next_row` and Reader::frames`.
    pub fn output_color_type(&mut self) -> (ColorType, BitDepth) {
        use common::ColorType::*;
        let t = self.transform;
        let info = get_info!(self);
        if t == ::TRANSFORM_IDENTITY {
            (info.color_type, info.bit_depth)
        } else {
            let bits = match info.bit_depth as u8 {
                16 if t.intersects(
                    ::TRANSFORM_SCALE_16 | ::TRANSFORM_STRIP_16
                ) => 8,
                _ if t.contains(::TRANSFORM_EXPAND) => 8,
                n => n 
            };
            let color_type = if t.contains(::TRANSFORM_EXPAND) {
                let has_trns = info.trns.is_some();
                match info.color_type {
                    Grayscale if has_trns => GrayscaleAlpha,
                    RGB if has_trns => RGBA,
                    Indexed if has_trns => RGBA,
                    Indexed => RGB,
                    ct => ct
                }
            } else {
                info.color_type
            };
            (color_type, BitDepth::from_u8(bits).unwrap())
        }
    }
    
    /// Returns the number of bytes required to hold a deinterlaced image frame
    /// that is decoded using the given input transformations.
    pub fn output_buffer_size(&self) -> usize {
        let (width, height) = get_info!(self).size();
        let size = self.output_line_size(width);
        size * height as usize
    }
    
    /// Returns the number of bytes required to hold a deinterlaced row.
    pub fn output_line_size(&self, width: u32) -> usize {
        let size = self.line_size(width);
        if get_info!(self).bit_depth as u8 == 16 && self.transform.intersects(
            ::TRANSFORM_SCALE_16 | ::TRANSFORM_STRIP_16
        ) {
            size / 2
        } else {
            size
        }
    }
    
    /// Returns the number of bytes required to decode a deinterlaced row.
    fn line_size(&self, width: u32) -> usize {
        use common::ColorType::*;
        let t = self.transform;
        let info = get_info!(self);
        let trns = info.trns.is_some();
        // TODO 16 bit
        let bits = match info.color_type {
            Indexed if trns && t.contains(::TRANSFORM_EXPAND) => 4 * 8,
            Indexed if t.contains(::TRANSFORM_EXPAND) => 3 * 8,
            RGB if trns && t.contains(::TRANSFORM_EXPAND) => 4 * 8,
            Grayscale if trns && t.contains(::TRANSFORM_EXPAND) => 2 * 8,
            Grayscale if t.contains(::TRANSFORM_EXPAND) => 1 * 8,
            GrayscaleAlpha if t.contains(::TRANSFORM_EXPAND) => 2 * 8,
            // divide by 2 as it will get mutiplied by two later
            _ if info.bit_depth as u8 == 16 => info.bits_per_pixel() / 2,
            _ => info.bits_per_pixel()
        }
        * width as usize
        * if info.bit_depth as u8 == 16 { 2 } else { 1 };
        let len = bits / 8;
        let extra = bits % 8;
        len + match extra { 0 => 0, _ => 1 }
    }
    
    fn allocate_out_buf(&mut self) {
        let width = get_info!(self).width;
        self.processed = vec![0; self.line_size(width)]
    }
    
    fn expand_gray_u8(&mut self) {
        let info = get_info!(self);
        let rescale = true;
        let scaling_factor = if rescale {
            (255)/((1u16 << info.bit_depth as u8) - 1) as u8
        } else {
            1
        };
        if let Some(ref trns) = info.trns {
            utils::unpack_bits(&mut self.processed, 2, info.bit_depth as u8, |pixel, chunk| {
                if pixel == trns[0] {
                    chunk[1] = 0
                } else {
                    chunk[1] = 0xFF
                }
                chunk[0] = pixel * scaling_factor
            })
        } else {
            utils::unpack_bits(&mut self.processed, 1, info.bit_depth as u8, |val, chunk| {
                chunk[0] = val * scaling_factor
            })
        }
    }
    
    fn expand_paletted(&mut self) {
        let info = get_info!(self);
        let palette = info.palette.as_ref().unwrap_or_else(|| panic!());
        if let Some(ref trns) = info.trns {
            utils::unpack_bits(&mut self.processed, 4, info.bit_depth as u8, |i, chunk| {
                let (rgb, a) = (
                    // TODO prevent panic!
                    &palette[3*i as usize..3*i as usize+3],
                    *trns.get(i as usize).unwrap_or(&0xFF)
                );
                chunk[0] = rgb[0];
                chunk[1] = rgb[1];
                chunk[2] = rgb[2];
                chunk[3] = a;
            });
        } else {
            utils::unpack_bits(&mut self.processed, 3, info.bit_depth as u8, |i, chunk| {
                let rgb = &palette[3*i as usize..3*i as usize+3];
                chunk[0] = rgb[0];
                chunk[1] = rgb[1];
                chunk[2] = rgb[2];
            })
        }
    }
    
    /// Returns the next raw row of the image
    fn next_raw_interlaced_row(&mut self) -> Result<Option<(&[u8], Option<(u8, u32, u32)>)>, DecodingError> {
        if self.eof {
            return Ok(None)
        }
        let _ = get_info!(self);
        let bpp = self.bpp;
        let (rowlen, passdata) = if let Some(ref mut adam7) = self.adam7 {
            let last_pass = adam7.current_pass();
            if let Some((pass, line, len)) = adam7.next() {
                let rowlen = get_info!(self).raw_row_length_from_width(len);
                if last_pass != pass {
                    self.prev.clear();
                    for _ in 0..rowlen {
                        self.prev.push(0);
                    }   
                }
                (rowlen, Some((pass, line, len)))
            } else {
                return Ok(None)
            }
        } else {
            (self.rowlen, None)
        };
        loop {
            if self.current.len() >= rowlen {
                if let Some(filter) = FilterType::from_u8(self.current[0]) {
                    unfilter(filter, bpp, &self.prev[1..rowlen], &mut self.current[1..rowlen]);
                    utils::copy_memory(&self.current[..rowlen], &mut self.prev[..rowlen]);
                    // TODO optimize
                    self.current = self.current[rowlen..].into();
                    return Ok(
                        Some((
                            &self.prev[1..rowlen],
                            passdata
                        ))
                    )
                } else {
                    return Err(DecodingError::Format(
                        format!("invalid filter method ({})", self.current[0]).into()
                    ))
                }
            } else {
                let val = try!(decode_next(
                    &mut self.r, &mut self.d, &mut self.pos,
                    &mut self.end, &mut self.buf
                ));
                match val {
                    Some(Decoded::ImageData(data)) => {
                        //self.current.extend(data.iter().map(|&v| v));
                        self.current.push_all(data);
                    },
                    None => {
                        if self.current.len() > 0 {
                            return Err(DecodingError::Format(
                              "file truncated".into()
                            ))
                        } else {
                            self.eof = true;
                            return Ok(None)
                        }
                    }
                    _ => ()
                }
            }
        }
    }
    
    /// Returns the next decoded block (low-level)
    pub fn decode_next(&mut self) -> Result<Option<Decoded>, DecodingError> {
        decode_next(
            &mut self.r, &mut self.d, &mut self.pos,
            &mut self.end, &mut self.buf
        )
    }
}

/// Free function form of Reader::decode_next to circumvent borrow issues
fn decode_next<'a, R: Read>(
    r: &mut R, d: &'a mut StreamingDecoder,
    pos: &mut usize, end: &mut usize, buf: &mut [u8])
-> Result<Option<Decoded<'a>>, DecodingError> {
    loop {
        if pos == end {
            *end = try!(r.read(buf));
            *pos = 0;
        }
        match try!(d.update(&buf[*pos..*end])) {
            (n, Decoded::Nothing) => *pos += n,
            (_, Decoded::ImageEnd) => return Ok(None),
            (n, result) => {
                *pos += n;
                return Ok(Some(unsafe {
                    // This transmute just casts the lifetime away. See comment
                    // in StreamingDecoder::update for more information.
                    mem::transmute::<Decoded, Decoded>(result)
                }))
            }
        }
    }
}

#[cfg(test)]
mod test {
    extern crate test;
    
    use std::fs::File;
    use std::io::Read;
    
    use super::Decoder;
    use HasParameters;
    
    #[bench]
    fn bench_big(b: &mut test::Bencher) {
        let mut data = Vec::new();
        File::open("/Users/nwinter/Desktop/Ducati_side_shadow.png").unwrap().read_to_end(&mut data).unwrap();
        let mut decoder = Decoder::new(&*data);
        decoder.set(::TRANSFORM_IDENTITY);
        let (info, mut decoder) = decoder.read_info().unwrap();
        let mut image = vec![0; info.buffer_size()];
        b.iter(|| {
            let mut decoder = Decoder::new(&*data);
            decoder.set(::TRANSFORM_IDENTITY);
            let (_, mut decoder) = decoder.read_info().unwrap();
            test::black_box(decoder.next_frame(&mut image));
        });
        b.bytes = info.buffer_size() as u64
    }
}