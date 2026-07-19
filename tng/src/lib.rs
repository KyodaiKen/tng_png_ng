use std::ffi::c_void;
use std::io::{Cursor, Read, Write};
use rayon::prelude::*;

// --- 1. CONSTANTS & SIGNATURES ---

pub const TNG_MAGIC: [u8; 8] = [137, 84, 78, 71, 13, 10, 26, 10]; // \x89TNG\r\n\x1a\n

pub const CHUNK_IHDR: [u8; 4] = *b"IHDR";
pub const CHUNK_THDR: [u8; 4] = *b"THDR";
pub const CHUNK_TDAT: [u8; 4] = *b"TDAT";
pub const CHUNK_TIDX: [u8; 4] = *b"tIDX";
pub const CHUNK_IEND: [u8; 4] = *b"IEND";

// Compression Codes
pub const COMP_DEFLATE: u8  = 0x00;
pub const COMP_ZSTD: u8     = 0x10;
pub const COMP_BROTLI: u8   = 0x11;
pub const COMP_LZMA: u8     = 0x12;
pub const COMP_FFV1_GOLOMB: u8 = 0x13; // Placeholder
pub const COMP_FFV1_RANGE:  u8 = 0x14; // Placeholder
pub const COMP_FFV1_STT:    u8 = 0x15; // Placeholder

// Filter Codes
pub const FILT_NONE: u8    = 0x00;
pub const FILT_SUB: u8     = 0x01;
pub const FILT_UP: u8     = 0x02;
pub const FILT_AVG: u8     = 0x03;
pub const FILT_PAETH: u8   = 0x04;
pub const FILT_FFV1: u8    = 0x10; // Placeholder

// --- 2. C-COMPATIBLE STRUCTURES & CALLBACKS ---

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct TngImageInfo {
    pub width: u32,
    pub height: u32,
    pub bit_depth: u8,
    pub color_type: u8, // 2 = RGB, 6 = RGBA
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct TngTileConfig {
    pub tile_width: u32,
    pub tile_height: u32,
    pub num_tiles_x: u32,
    pub num_tiles_y: u32,
    pub edge_width: u32,
    pub edge_height: u32,
    pub threads: u32,
    pub compression_algo: u8,
    pub filter_algo: u8,
}

pub type TngReadCallback = unsafe extern "C" fn(user_data: *mut c_void, buffer: *mut u8, size: usize) -> usize;
pub type TngWriteCallback = unsafe extern "C" fn(user_data: *mut c_void, buffer: *const u8, size: usize) -> usize;

// --- 3. OPAQUE HANDLES ---

pub struct TngEncoder {
    pub info: TngImageInfo,
    pub config: TngTileConfig,
    pub bytes_per_pixel: usize,
}

pub struct TngDecoder {
    pub info: Option<TngImageInfo>,
    pub config: Option<TngTileConfig>,
    pub bytes_per_pixel: usize,
}

// --- 4. UTILITY FUNCTIONS (CRC32 & IMAGING) ---

fn bytes_per_pixel(color_type: u8, bit_depth: u8) -> usize {
    let channels = match color_type {
        0 => 1, // Grayscale
        2 => 3, // RGB
        3 => 1, // Palette
        4 => 2, // Grayscale + Alpha
        6 => 4, // RGBA
        _ => 4,
    };
    ((channels * bit_depth as usize) + 7) / 8
}

fn crc32(type_bytes: &[u8; 4], data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    let update = |mut c: u32, byte: u8| -> u32 {
        c ^= byte as u32;
        for _ in 0..8 {
            if (c & 1) != 0 {
                c = (c >> 1) ^ 0xEDB8_8320;
            } else {
                c >>= 1;
            }
        }
        c
    };
    for &b in type_bytes { crc = update(crc, b); }
    for &b in data { crc = update(crc, b); }
    crc ^ 0xFFFF_FFFFu32
}

// --- 5. PNG FILTER ENGINE (IN-PLACE ZERO ALLOCATION) ---

fn paeth_predictor(a: u8, b: u8, c: u8) -> u8 {
    let p = a as i32 + b as i32 - c as i32;
    let pa = (p - a as i32).abs();
    let pb = (p - b as i32).abs();
    let pc = (p - c as i32).abs();
    if pa <= pb && pa <= pc { a } else if pb <= pc { b } else { c }
}

fn apply_png_filter(src: &[u8], width: usize, height: usize, bpp: usize, filter: u8) -> Vec<u8> {
    let stride = width * bpp;
    let mut out = vec![0u8; src.len()];

    for y in 0..height {
        let row_start = y * stride;
        for x in 0..stride {
            let idx = row_start + x;
            let left = if x >= bpp { src[idx - bpp] } else { 0 };
            let up = if y > 0 { src[idx - stride] } else { 0 };
            let up_left = if y > 0 && x >= bpp { src[idx - stride - bpp] } else { 0 };

            out[idx] = match filter {
                FILT_SUB   => src[idx].wrapping_sub(left),
                FILT_UP    => src[idx].wrapping_sub(up),
                FILT_AVG   => src[idx].wrapping_sub(((left as u16 + up as u16) / 2) as u8),
                FILT_PAETH => src[idx].wrapping_sub(paeth_predictor(left, up, up_left)),
                _          => src[idx],
            };
        }
    }
    out
}

// --- 6. CORE CHUNK COMPRESSION WORKERS ---

fn compress_tile_payload(data: &[u8], width: usize, height: usize, bpp: usize, algo: u8, filter: u8) -> Vec<u8> {
    let mut chunk_data = Vec::with_capacity(data.len() / 2);
    chunk_data.push(algo);
    chunk_data.push(filter);

    let filtered = if filter < 0x10 {
        apply_png_filter(data, width, height, bpp, filter)
    } else {
        data.to_vec()
    };

    match algo {
        COMP_ZSTD => {
            let compressed = zstd::stream::encode_all(Cursor::new(&filtered), 11).unwrap_or_default();
            chunk_data.extend_from_slice(&compressed);
        }
        COMP_BROTLI => {
            let mut writer = brotli::CompressorWriter::new(&mut chunk_data, 4096, 5, 22);
            writer.write_all(&filtered).unwrap();
        }
        COMP_LZMA => {
            let mut compressed = Vec::new();
            let mut encoder = xz2::write::XzEncoder::new(&mut compressed, 3);
            encoder.write_all(&filtered).unwrap();
            encoder.finish().unwrap();
            chunk_data.extend_from_slice(&compressed);
        }
        COMP_DEFLATE => {
            let mut encoder = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::fast());
            encoder.write_all(&filtered).unwrap();
            let compressed = encoder.finish().unwrap_or_default();
            chunk_data.extend_from_slice(&compressed);
        }
        _ => {
            chunk_data.extend_from_slice(&filtered);
        }
    }
    chunk_data
}

fn decompress_tile_payload_inplace(
    payload: &[u8],
    width: usize,
    height: usize,
    bpp: usize,
    out_buf: &mut [u8],
) -> Result<(), &'static str> {
    if payload.len() < 2 { return Err("Invalid TDAT payload length"); }
    let algo = payload[0];
    let filter = payload[1];
    let compressed_body = &payload[2..];
    let expected_len = width * height * bpp;

    let target = &mut out_buf[..expected_len];

    match algo {
        COMP_ZSTD => {
            let mut decoder = zstd::Decoder::new(compressed_body).map_err(|_| "ZSTD setup failed")?;
            decoder.read_exact(target).map_err(|_| "ZSTD extraction failed")?;
        }
        COMP_BROTLI => {
            let mut reader = brotli::Decompressor::new(compressed_body, 4096);
            reader.read_exact(target).map_err(|_| "Brotli extraction failed")?;
        }
        COMP_LZMA => {
            let mut decoder = xz2::read::XzDecoder::new(compressed_body);
            decoder.read_exact(target).map_err(|_| "LZMA extraction failed")?;
        }
        COMP_DEFLATE => {
            let mut decoder = flate2::read::DeflateDecoder::new(compressed_body);
            decoder.read_exact(target).map_err(|_| "Deflate extraction failed")?;
        }
        _ => {
            if compressed_body.len() < expected_len { return Err("Raw payload size mismatch"); }
            target.copy_from_slice(&compressed_body[..expected_len]);
        }
    }

    if filter < 0x10 {
        let stride = width * bpp;
        for y in 0..height {
            let row_start = y * stride;
            for x in 0..stride {
                let idx = row_start + x;
                let left = if x >= bpp { target[idx - bpp] } else { 0 };
                let up = if y > 0 { target[idx - stride] } else { 0 };
                let up_left = if y > 0 && x >= bpp { target[idx - stride - bpp] } else { 0 };

                target[idx] = match filter {
                    FILT_SUB   => target[idx].wrapping_add(left),
                    FILT_UP    => target[idx].wrapping_add(up),
                    FILT_AVG   => target[idx].wrapping_add(((left as u16 + up as u16) / 2) as u8),
                    FILT_PAETH => target[idx].wrapping_add(paeth_predictor(left, up, up_left)),
                    _          => target[idx],
                };
            }
        }
    }
    Ok(())
}

// --- 7. HELPER STREAMING CHUNK WRITERS ---

unsafe fn emit_chunk(write_cb: TngWriteCallback, user_data: *mut c_void, tag: [u8; 4], data: &[u8]) -> bool {
    let len_bytes = (data.len() as u32).to_be_bytes();
    if unsafe { write_cb(user_data, len_bytes.as_ptr(), 4) } != 4 { return false; }
    if unsafe { write_cb(user_data, tag.as_ptr(), 4) } != 4 { return false; }
    if !data.is_empty() && unsafe { write_cb(user_data, data.as_ptr(), data.len()) } != data.len() { return false; }
    let crc_val = crc32(&tag, data);
    let crc_bytes = crc_val.to_be_bytes();
    if unsafe { write_cb(user_data, crc_bytes.as_ptr(), 4) } != 4 { return false; }
    true
}

// --- 8. PUBLIC C-FFI API INTERFACE ---

#[unsafe(no_mangle)]
pub unsafe extern "C" fn tng_encoder_init(info: *const TngImageInfo, config: *const TngTileConfig) -> *mut TngEncoder {
    if info.is_null() || config.is_null() { return std::ptr::null_mut(); }
    let img_info = unsafe { *info };
    let tile_conf = unsafe { *config };
    let bpp = bytes_per_pixel(img_info.color_type, img_info.bit_depth);

    let encoder = Box::new(TngEncoder { info: img_info, config: tile_conf, bytes_per_pixel: bpp });
    Box::into_raw(encoder)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn tng_encoder_process(
    encoder_ptr: *mut TngEncoder,
    read_cb: TngReadCallback,
    write_cb: TngWriteCallback,
    user_data: *mut c_void,
) -> i32 {
    if encoder_ptr.is_null() { return -1; }
    let enc = unsafe { &*encoder_ptr };

    if unsafe { write_cb(user_data, TNG_MAGIC.as_ptr(), 8) } != 8 { return -2; }

    let mut ihdr_payload = Vec::with_capacity(13);
    ihdr_payload.extend_from_slice(&enc.info.width.to_be_bytes());
    ihdr_payload.extend_from_slice(&enc.info.height.to_be_bytes());
    ihdr_payload.push(enc.info.bit_depth);
    ihdr_payload.push(enc.info.color_type);
    ihdr_payload.push(0x00);
    ihdr_payload.push(0x00);
    ihdr_payload.push(0x00);
    if unsafe { !emit_chunk(write_cb, user_data, CHUNK_IHDR, &ihdr_payload) } { return -3; }

    let mut thdr_payload = Vec::with_capacity(24);
    thdr_payload.extend_from_slice(&enc.config.num_tiles_x.to_be_bytes());
    thdr_payload.extend_from_slice(&enc.config.num_tiles_y.to_be_bytes());
    thdr_payload.extend_from_slice(&enc.config.tile_width.to_be_bytes());
    thdr_payload.extend_from_slice(&enc.config.tile_height.to_be_bytes());
    thdr_payload.extend_from_slice(&enc.config.edge_width.to_be_bytes());
    thdr_payload.extend_from_slice(&enc.config.edge_height.to_be_bytes());
    if unsafe { !emit_chunk(write_cb, user_data, CHUNK_THDR, &thdr_payload) } { return -4; }

    let pool = rayon::ThreadPoolBuilder::new().num_threads(enc.config.threads as usize).build().unwrap();
    let mut tracking_index = Vec::with_capacity((enc.config.num_tiles_x * enc.config.num_tiles_y) as usize);
    let row_stride = enc.info.width as usize * enc.bytes_per_pixel;

    for ty in 0..enc.config.num_tiles_y {
        let current_tile_height = if ty == enc.config.num_tiles_y - 1 { enc.config.edge_height } else { enc.config.tile_height } as usize;
        let chunk_buffer_len = current_tile_height * row_stride;
        let mut tile_row_pixels = vec![0u8; chunk_buffer_len];

        let read_bytes = unsafe { read_cb(user_data, tile_row_pixels.as_mut_ptr(), chunk_buffer_len) };
        if read_bytes != chunk_buffer_len { return -5; }

        let mut indices: Vec<u32> = (0..enc.config.num_tiles_x).collect();
        let compressed_row_tiles: Vec<Vec<u8>> = pool.install(|| {
            indices.par_iter_mut().map(|&mut tx| {
                let current_tile_width = if tx == enc.config.num_tiles_x - 1 { enc.config.edge_width } else { enc.config.tile_width } as usize;
                let mut extraction_buffer = vec![0u8; current_tile_width * current_tile_height * enc.bytes_per_pixel];

                for y in 0..current_tile_height {
                    let src_offset = (y * row_stride) + (tx as usize * enc.config.tile_width as usize * enc.bytes_per_pixel);
                    let dest_offset = y * current_tile_width * enc.bytes_per_pixel;
                    let run_length = current_tile_width * enc.bytes_per_pixel;
                    extraction_buffer[dest_offset..(dest_offset + run_length)]
                    .copy_from_slice(&tile_row_pixels[src_offset..(src_offset + run_length)]);
                }

                compress_tile_payload(
                    &extraction_buffer,
                    current_tile_width,
                    current_tile_height,
                    enc.bytes_per_pixel,
                    enc.config.compression_algo,
                    enc.config.filter_algo
                )
            }).collect()
        });

        for tile_bytes in compressed_row_tiles {
            tracking_index.push(tile_bytes.len() as u32);
            if unsafe { !emit_chunk(write_cb, user_data, CHUNK_TDAT, &tile_bytes) } { return -6; }
        }
    }

    let mut tidx_payload = Vec::with_capacity(tracking_index.len() * 4);
    for &size in &tracking_index {
        tidx_payload.extend_from_slice(&size.to_be_bytes());
    }
    if unsafe { !emit_chunk(write_cb, user_data, CHUNK_TIDX, &tidx_payload) } { return -7; }

    if unsafe { !emit_chunk(write_cb, user_data, CHUNK_IEND, &[]) } { return -8; }

    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn tng_encoder_close(encoder_ptr: *mut TngEncoder) {
    if !encoder_ptr.is_null() { unsafe { let _ = Box::from_raw(encoder_ptr); } }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn tng_decoder_init() -> *mut TngDecoder {
    let decoder = Box::new(TngDecoder { info: None, config: None, bytes_per_pixel: 0 });
    Box::into_raw(decoder)
}

#[unsafe(no_mangle)]
pub extern "C" fn tng_decoder_process(
    decoder_ptr: *mut TngDecoder,
    read_cb: TngReadCallback,
    write_cb: TngWriteCallback,
    user_data: *mut c_void,
) -> i32 {
    if decoder_ptr.is_null() { return -1; }
    let dec = unsafe { &mut *decoder_ptr };

    // 1. Validate File Signature
    let mut sig = [0u8; 8];
    unsafe {
        if read_cb(user_data, sig.as_mut_ptr(), 8) != 8 || sig != TNG_MAGIC { return -2; }
    }

    // 2. Parse Headers
    loop {
        let mut len_bytes = [0u8; 4];
        unsafe { if read_cb(user_data, len_bytes.as_mut_ptr(), 4) != 4 { return -3; } }
        let length = u32::from_be_bytes(len_bytes) as usize;

        let mut type_bytes = [0u8; 4];
        unsafe { if read_cb(user_data, type_bytes.as_mut_ptr(), 4) != 4 { return -4; } }

        let mut data_payload = vec![0u8; length];
        unsafe { if length > 0 && read_cb(user_data, data_payload.as_mut_ptr(), length) != length { return -5; } }

        let mut crc_bytes = [0u8; 4];
        unsafe { if read_cb(user_data, crc_bytes.as_mut_ptr(), 4) != 4 { return -6; } }

        match type_bytes {
            CHUNK_IHDR => {
                let width = u32::from_be_bytes([data_payload[0], data_payload[1], data_payload[2], data_payload[3]]);
                let height = u32::from_be_bytes([data_payload[4], data_payload[5], data_payload[6], data_payload[7]]);
                let bit_depth = data_payload[8];
                let color_type = data_payload[9];
                dec.bytes_per_pixel = bytes_per_pixel(color_type, bit_depth);
                dec.info = Some(TngImageInfo { width, height, bit_depth, color_type });
            }
            CHUNK_THDR => {
                dec.config = Some(TngTileConfig {
                    num_tiles_x: u32::from_be_bytes([data_payload[0], data_payload[1], data_payload[2], data_payload[3]]),
                                  num_tiles_y: u32::from_be_bytes([data_payload[4], data_payload[5], data_payload[6], data_payload[7]]),
                                  tile_width: u32::from_be_bytes([data_payload[8], data_payload[9], data_payload[10], data_payload[11]]),
                                  tile_height: u32::from_be_bytes([data_payload[12], data_payload[13], data_payload[14], data_payload[15]]),
                                  edge_width: u32::from_be_bytes([data_payload[16], data_payload[17], data_payload[18], data_payload[19]]),
                                  edge_height: u32::from_be_bytes([data_payload[20], data_payload[21], data_payload[22], data_payload[23]]),
                                  threads: 1, compression_algo: 0, filter_algo: 0,
                });
                break;
            }
            _ => continue,
        }
    }

    let info = match dec.info { Some(i) => i, None => return -9 };
    let conf = match dec.config { Some(c) => c, None => return -10 };

    // Initialize cross-platform persistent global thread context
    let _ = rayon::ThreadPoolBuilder::new().num_threads(4).build_global();
    let out_stride = info.width as usize * dec.bytes_per_pixel;

    let mut assembly_row_buffer = vec![0u8; conf.tile_height as usize * out_stride];
    let mut tile_batch = Vec::with_capacity(4);
    let mut current_tx = 0;
    let mut current_ty = 0;

    // Pre-allocate a single permanent workspace for the decoder threads
    let max_tile_bytes = conf.tile_width as usize * conf.tile_height as usize * dec.bytes_per_pixel;
    let mut thread_buffers = vec![vec![0u8; max_tile_bytes]; 4];

    loop {
        let mut len_bytes = [0u8; 4];
        unsafe { if read_cb(user_data, len_bytes.as_mut_ptr(), 4) != 4 { break; } }
        let length = u32::from_be_bytes(len_bytes) as usize;

        let mut type_bytes = [0u8; 4];
        unsafe { if read_cb(user_data, type_bytes.as_mut_ptr(), 4) != 4 { break; } }

        let mut data_payload = vec![0u8; length];
        unsafe { if length > 0 && read_cb(user_data, data_payload.as_mut_ptr(), length) != length { break; } }

        let mut crc_bytes = [0u8; 4];
        unsafe { if read_cb(user_data, crc_bytes.as_mut_ptr(), 4) != 4 { break; } }

        if type_bytes == CHUNK_TDAT {
            tile_batch.push((current_tx, data_payload));
            current_tx += 1;

            if tile_batch.len() == 4 || current_tx == conf.num_tiles_x as usize {
                let current_tile_height = if current_ty == conf.num_tiles_y - 1 { conf.edge_height } else { conf.tile_height } as usize;

                // Move items completely out of tile_batch to reuse vector container layout allocation
                let mut processing_vec = Vec::with_capacity(4);
                std::mem::swap(&mut tile_batch, &mut processing_vec);

                let (left_buffers, _) = thread_buffers.split_at_mut(processing_vec.len());

                // into_par_iter() consumes processing_vec, forcing the thread context to drop and destroy payload memory instantly
                let batch_results: Vec<(usize, Result<(), &'static str>)> = processing_vec
                .into_par_iter()
                .zip(left_buffers.par_iter_mut())
                .map(|((tx, payload), out_buf)| {
                    let current_tile_width = if tx as u32 == conf.num_tiles_x - 1 { conf.edge_width } else { conf.tile_width } as usize;
                    let res = decompress_tile_payload_inplace(&payload, current_tile_width, current_tile_height, dec.bytes_per_pixel, out_buf);
                    (tx, res)
                })
                .collect();

                // Blit reconstructed pixels directly from the static thread workspace cache
                for (idx, (tx, res)) in batch_results.into_iter().enumerate() {
                    if res.is_err() { return -12; }
                    let current_tile_width = if tx as u32 == conf.num_tiles_x - 1 { conf.edge_width } else { conf.tile_width } as usize;
                    let tile_pixels = &thread_buffers[idx];

                    for y in 0..current_tile_height {
                        let dest_offset = (y * out_stride) + (tx * conf.tile_width as usize * dec.bytes_per_pixel);
                        let src_offset = y * current_tile_width * dec.bytes_per_pixel;
                        let run_length = current_tile_width * dec.bytes_per_pixel;
                        assembly_row_buffer[dest_offset..(dest_offset + run_length)]
                        .copy_from_slice(&tile_pixels[src_offset..(src_offset + run_length)]);
                    }
                }
            }

            if current_tx == conf.num_tiles_x as usize {
                let current_tile_height = if current_ty == conf.num_tiles_y - 1 { conf.edge_height } else { conf.tile_height } as usize;
                let current_row_len = current_tile_height * out_stride;

                unsafe {
                    if write_cb(user_data, assembly_row_buffer.as_ptr(), current_row_len) != current_row_len {
                        return -13;
                    }
                }
                current_tx = 0;
                current_ty += 1;
            }
        } else if type_bytes == CHUNK_IEND {
            break;
        }
    }

    0
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn tng_decoder_close(decoder_ptr: *mut TngDecoder) {
    if !decoder_ptr.is_null() { unsafe { let _ = Box::from_raw(decoder_ptr); } }
}
