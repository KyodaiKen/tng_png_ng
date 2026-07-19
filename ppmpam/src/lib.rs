use std::ffi::CStr;
use std::fs::File;
use std::io::{self, Read};
use std::os::raw::{c_char, c_void};
use std::ptr;

---

### FFI Compatibility Types & Metadata

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Unknown = 0,
    PpmAscii = 3,
    PpmBinary = 6,
    Pam = 7,
}

#[repr(C)]
pub struct PpmPamMetadata {
    pub format: Format,
    pub width: u32,
    pub height: u32,
    pub maxval: u32,
    pub depth: u32,
    pub tuple_type: [c_char; 64],
}

/// Core Decoder handle state tracking. Holds the polymorphic byte stream reader
/// and tracks streaming progress row-by-row.
pub struct PpmPamDecoder {
    reader: Box<dyn Read>,
    metadata: PpmPamMetadata,
    current_row: u32,
    bytes_per_row: usize,
}

/// Defines the C-FFI safe streaming callback function signature
pub type PpmPamReadCallback = unsafe extern "C" fn(user_data: *mut c_void, buf: *mut u8, len: usize) -> usize;

struct StreamContext {
    cb: PpmPamReadCallback,
    user_data: *mut c_void,
}

impl Read for StreamContext {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        // Explicit unsafe block for FFI callback execution as enforced by Rust 2024
        let bytes_read = unsafe { (self.cb)(self.user_data, buf.as_mut_ptr(), buf.len()) };
        Ok(bytes_read)
    }
}

---

### Internal Header Parsing Implementation

/// Internal helper to pull next space-separated alphanumeric token from text-based headers
fn next_ppm_token(r: &mut dyn Read) -> Result<String, String> {
    let mut token = Vec::new();
    let mut in_comment = false;

    loop {
        let mut b = [0u8; 1];
        if r.read_exact(&mut b).is_err() {
            if !token.is_empty() {
                break;
            }
            return Err("Unexpected End-of-File reached while decoding header".to_string());
        }
        let c = b[0];

        if in_comment {
            if c == b'\n' || c == b'\r' {
                in_comment = false;
            }
            continue;
        }

        if c == b'#' {
            in_comment = true;
            continue;
        }

        if c.is_ascii_whitespace() {
            if !token.is_empty() {
                break; // The whitespace byte is successfully consumed here
            }
            continue;
        }

        token.push(c);
    }

    String::from_utf8(token).map_err(|_| "Header encoding is not valid UTF-8".to_string())
}

fn parse_headers(mut reader: Box<dyn Read>) -> Result<PpmPamDecoder, String> {
    let mut magic = [0u8; 2];
    reader.read_exact(&mut magic).map_err(|_| "Failed to verify image magic bytes".to_string())?;

    let format = match &magic {
        b"P3" => Format::PpmAscii,
        b"P6" => Format::PpmBinary,
        b"P7" => Format::Pam,
        _ => return Err("Invalid format identifier: signature must be P3, P6, or P7".to_string()),
    };

    let mut metadata = PpmPamMetadata {
        format,
            width: 0,
            height: 0,
            maxval: 0,
            depth: 0,
            tuple_type: [0; 64],
    };

    if format == Format::PpmAscii || format == Format::PpmBinary {
        metadata.depth = 3; // PPM maps natively to standard RGB
        let w_str = next_ppm_token(&mut reader)?;
        let h_str = next_ppm_token(&mut reader)?;
        let m_str = next_ppm_token(&mut reader)?;

        metadata.width = w_str.parse::<u32>().map_err(|_| "Invalid width integer".to_string())?;
        metadata.height = h_str.parse::<u32>().map_err(|_| "Invalid height integer".to_string())?;
        metadata.maxval = m_str.parse::<u32>().map_err(|_| "Invalid maxval integer".to_string())?;
    } else {
        // Parse Netpbm PAM format specifications line-by-line
        let mut line = Vec::new();
        loop {
            line.clear();
            loop {
                let mut b = [0u8; 1];
                reader.read_exact(&mut b).map_err(|_| "Abrupt EOF inside PAM header declarations".to_string())?;
                if b[0] == b'\n' {
                    break;
                }
                if b[0] != b'\r' {
                    line.push(b[0]);
                }
            }
            let line_str = String::from_utf8_lossy(&line);
            let parts: Vec<&str> = line_str.split_whitespace().collect();
            if parts.is_empty() {
                continue;
            }

            match parts[0] {
                "WIDTH" => {
                    if parts.len() < 2 { return Err("Malformed WIDTH configuration".to_string()); }
                    metadata.width = parts[1].parse::<u32>().map_err(|_| "Invalid PAM width values".to_string())?;
                }
                "HEIGHT" => {
                    if parts.len() < 2 { return Err("Malformed HEIGHT configuration".to_string()); }
                    metadata.height = parts[1].parse::<u32>().map_err(|_| "Invalid PAM height values".to_string())?;
                }
                "DEPTH" => {
                    if parts.len() < 2 { return Err("Malformed DEPTH configuration".to_string()); }
                    metadata.depth = parts[1].parse::<u32>().map_err(|_| "Invalid PAM depth layout".to_string())?;
                }
                "MAXVAL" => {
                    if parts.len() < 2 { return Err("Malformed MAXVAL configuration".to_string()); }
                    metadata.maxval = parts[1].parse::<u32>().map_err(|_| "Invalid PAM maxval range".to_string())?;
                }
                "TUPLTYPE" => {
                    if parts.len() < 2 { return Err("Malformed TUPLTYPE attributes".to_string()); }
                    let val = parts[1];
                    let bytes = val.as_bytes();
                    let len = bytes.len().min(63);
                    for (dest, &src) in metadata.tuple_type.iter_mut().zip(bytes.iter().take(len)) {
                        *dest = src as c_char;
                    }
                }
                "ENDHDR" => break,
                _ => {} // Discard comments or unknown fields
            }
        }
    }

    let bytes_per_sample = if metadata.maxval > 255 { 2 } else { 1 };
    let bytes_per_row = (metadata.width as usize) * (metadata.depth as usize) * bytes_per_sample;

    Ok(PpmPamDecoder {
        reader,
       metadata,
       current_row: 0,
       bytes_per_row,
    })
}

---

### Rust Implementation API

impl PpmPamDecoder {
    pub fn read_scanlines(&mut self, num_scanlines: u32, out_buf: &mut [u8]) -> Result<u32, String> {
        if self.current_row >= self.metadata.height {
            return Ok(0);
        }

        let lines_to_read = num_scanlines.min(self.metadata.height - self.current_row);
        let bytes_per_sample = if self.metadata.maxval > 255 { 2 } else { 1 };
        let samples_per_row = (self.metadata.width * self.metadata.depth) as usize;
        let total_bytes_needed = lines_to_read as usize * self.bytes_per_row;

        if out_buf.len() < total_bytes_needed {
            return Err("Target layout output buffer capacity is too small".to_string());
        }

        match self.metadata.format {
            Format::PpmBinary | Format::Pam => {
                self.reader.read_exact(&mut out_buf[..total_bytes_needed])
                .map_err(|_| "Failed reading target binary stream payload".to_string())?;
            }
            Format::PpmAscii => {
                let mut buf_idx = 0;
                for _ in 0..(lines_to_read as usize * samples_per_row) {
                    let token = next_ppm_token(&mut self.reader)?;
                    let val = token.parse::<u32>().map_err(|_| "Invalid integer token inside ASCII pixel matrix".to_string())?;
                    if bytes_per_sample == 1 {
                        out_buf[buf_idx] = val as u8;
                        buf_idx += 1;
                    } else {
                        let be_bytes = (val as u16).to_be_bytes();
                        out_buf[buf_idx] = be_bytes[0];
                        out_buf[buf_idx + 1] = be_bytes[1];
                        buf_idx += 2;
                    }
                }
            }
            Format::Unknown => return Err("Processing an unrecognized file stream layout".to_string()),
        }

        self.current_row += lines_to_read;
        Ok(lines_to_read)
    }
}

---

### Public C-FFI Interface Boundary

#[unsafe(no_mangle)]
pub unsafe extern "C" fn ppmpam_open_file(
    filename: *const c_char,
    out_meta: *mut PpmPamMetadata,
) -> *mut PpmPamDecoder {
    if filename.is_null() || out_meta.is_null() {
        return ptr::null_mut();
    }

    let c_str = unsafe { CStr::from_ptr(filename) };
    let path_str = match c_str.to_str() {
        Ok(s) => s,
        Err(_) => return ptr::null_mut(),
    };

    let file = match File::open(path_str) {
        Ok(f) => f,
        Err(_) => return ptr::null_mut(),
    };

    match parse_headers(Box::new(file)) {
        Ok(decoder) => {
            unsafe { *out_meta = decoder.metadata };
            Box::into_raw(Box::new(decoder))
        }
        Err(_) => ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn ppmpam_open_stream(
    cb: PpmPamReadCallback,
    user_data: *mut c_void,
    out_meta: *mut PpmPamMetadata,
) -> *mut PpmPamDecoder {
    if out_meta.is_null() {
        return ptr::null_mut();
    }

    let ctx = StreamContext { cb, user_data };

    match parse_headers(Box::new(ctx)) {
        Ok(decoder) => {
            unsafe { *out_meta = decoder.metadata };
            Box::into_raw(Box::new(decoder))
        }
        Err(_) => ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn ppmpam_read_scanlines(
    decoder: *mut PpmPamDecoder,
    num_scanlines: u32,
    out_buf: *mut u8,
    out_buf_len: usize,
    out_lines_read: *mut u32,
) -> i32 {
    if decoder.is_null() || out_buf.is_null() || out_lines_read.is_null() {
        return -1; // Null pointer parameter provided
    }

    let dec = unsafe { &mut *decoder };
    let slice = unsafe { std::slice::from_raw_parts_mut(out_buf, out_buf_len) };

    match dec.read_scanlines(num_scanlines, slice) {
        Ok(lines) => {
            unsafe { *out_lines_read = lines };
            0 // Success status code
        }
        Err(_) => -2, // Buffer error or stream corrupted
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn ppmpam_close(decoder: *mut PpmPamDecoder) -> i32 {
    if decoder.is_null() {
        return -1;
    }
    // Drop execution ownership to reclaim the context safely
    let _ = unsafe { Box::from_raw(decoder) };
    0
}
