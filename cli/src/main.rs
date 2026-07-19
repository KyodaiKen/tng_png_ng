use anstream::println;
use chrono::Local;
use clap::{Parser, Subcommand};
use owo_colors::OwoColorize;
use std::ffi::c_void;
use std::fs::File;
use std::io::{Read, Write};
use std::process;

// Added FILT_UP to your imports
use tng::{
    tng_decoder_close, tng_decoder_init, tng_decoder_process, tng_encoder_close, tng_encoder_init,
    tng_encoder_process, TngImageInfo, TngTileConfig, COMP_BROTLI, COMP_DEFLATE, COMP_LZMA,
    COMP_ZSTD, FILT_AVG, FILT_NONE, FILT_PAETH, FILT_SUB, FILT_UP,
};

// --- LOGGING MACROS ---
// Fixed macro body syntax for expression positions by removing terminal semicolons
macro_rules! log_info {
    ($($arg:tt)*) => {
        println!("[{}] [{}] {}", Local::now().format("%H:%M:%S").bold().blue(), "INFO".bold().green(), format!($($arg)*))
    };
}

macro_rules! log_err {
    ($($arg:tt)*) => {
        println!("[{}] [{}] {}", Local::now().format("%H:%M:%S").bold().blue(), "FAIL".bold().red(), format!($($arg)*))
    };
}

#[derive(Parser, Debug)]
#[command(name = "tng-cli", version = "0.1.0", author, about = "TNG Codec Frontend", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    Encode {
        #[arg(help = "Input PPM/PAM file path")]
        input: String,
        #[arg(help = "Output TNG file path")]
        output: String,
        #[arg(long, default_value_t = 480)]
        tile_w: u32,
        #[arg(long, default_value_t = 480)]
        tile_h: u32,
        #[arg(long, default_value_t = 32)]
        threads: u32,
        #[arg(long, default_value = "zstd")]
        comp: String,
        #[arg(long, default_value = "paeth")]
        filter: String,
    },
    Decode {
        #[arg(help = "Input TNG file path")]
        input: String,
        #[arg(help = "Output RAW pixel file path")]
        output: String,
    },
}

struct StreamContext<'a> {
    input_stream: Box<dyn Read + 'a>,
    output_stream: Box<dyn Write + 'a>,
}

unsafe extern "C" fn cli_read_callback(
    user_data: *mut c_void,
    buffer: *mut u8,
    size: usize,
) -> usize {
    if user_data.is_null() || buffer.is_null() || size == 0 {
        return 0;
    }
    let ctx = unsafe { &mut *(user_data as *mut StreamContext) };
    let destination_slice = unsafe { std::slice::from_raw_parts_mut(buffer, size) };

    let mut total_bytes_read = 0;
    while total_bytes_read < size {
        match ctx.input_stream.read(&mut destination_slice[total_bytes_read..]) {
            Ok(0) => break,
            Ok(n) => total_bytes_read += n,
            Err(_) => return 0,
        }
    }
    total_bytes_read
}

unsafe extern "C" fn cli_write_callback(
    user_data: *mut c_void,
    buffer: *const u8,
    size: usize,
) -> usize {
    if user_data.is_null() || buffer.is_null() || size == 0 {
        return 0;
    }
    let ctx = unsafe { &mut *(user_data as *mut StreamContext) };
    let source_slice = unsafe { std::slice::from_raw_parts(buffer, size) };

    match ctx.output_stream.write_all(source_slice) {
        Ok(_) => size,
        Err(_) => 0,
    }
}

fn main() {
    let cli = Cli::parse();
    match cli.command {
        Commands::Encode { input, output, tile_w, tile_h, threads, comp, filter } => {
            execute_encode_pipeline(input, output, tile_w, tile_h, threads, comp, filter);
        }
        Commands::Decode { input, output } => {
            execute_decode_pipeline(input, output);
        }
    }
}

fn execute_encode_pipeline(
    input_path: String,
    output_path: String,
    tile_w: u32,
    tile_h: u32,
    threads: u32,
    comp_str: String,
    filter_str: String,
) {
    let comp_algo = match comp_str.to_lowercase().as_str() {
        "deflate" => COMP_DEFLATE,
        "zstd" => COMP_ZSTD,
        "brotli" => COMP_BROTLI,
        "lzma" => COMP_LZMA,
        _ => { log_err!("Unsupported compressor: {}", comp_str); process::exit(1); }
    };

    let filter_algo = match filter_str.to_lowercase().as_str() {
        "none" => FILT_NONE,
        "sub" => FILT_SUB,
        "up" => FILT_UP,
        "avg" => FILT_AVG,
        "paeth" => FILT_PAETH,
        _ => { log_err!("Unsupported filter type: {}", filter_str); process::exit(1); }
    };

    log_info!("Initializing PPMPAM Decoder for input: {}", input_path.bold());

    let input_file = match File::open(&input_path) {
        Ok(f) => f,
        Err(e) => { log_err!("Failed to open input: {}", e); process::exit(1); }
    };

    let ppm_decoder = match ppmpam::Decoder::new(input_file) {
        Ok(decoder) => decoder,
        Err(e) => { log_err!("PPMPAM header parse failure: {:?}", e); process::exit(1); }
    };

    let ppm_info = ppm_decoder.info();
    let width = ppm_info.width;
    let height = ppm_info.height;
    let color_type = ppm_info.color_type;
    let bit_depth = ppm_info.bit_depth;

    let num_tiles_x = (width + tile_w - 1) / tile_w;
    let num_tiles_y = (height + tile_h - 1) / tile_h;

    let mut edge_width = width % tile_w;
    if edge_width == 0 { edge_width = tile_w; }

    let mut edge_height = height % tile_h;
    if edge_height == 0 { edge_height = tile_h; }

    log_info!("Dimensions : {}x{} ({} bpp)", width, height, bit_depth);
    log_info!("Grid Alloc : {}x{} Matrix Tiles ({} threads)", num_tiles_x, num_tiles_y, threads);

    let output_file = match File::create(&output_path) {
        Ok(f) => f,
        Err(e) => { log_err!("Failed to create output: {}", e); process::exit(1); }
    };

    let mut context = StreamContext {
        input_stream: Box::new(ppm_decoder),
        output_stream: Box::new(output_file),
    };

    let img_info = TngImageInfo { width, height, bit_depth, color_type };
    let tile_config = TngTileConfig {
        tile_width: tile_w,
        tile_height: tile_h,
        num_tiles_x,
        num_tiles_y,
        edge_width,
        edge_height,
        threads,
        compression_algo: comp_algo,
        filter_algo,
    };

    unsafe {
        let encoder = tng_encoder_init(&img_info, &tile_config);
        if encoder.is_null() {
            log_err!("Fatal error during FFI state initialization");
            process::exit(1);
        }

        let user_data_ptr = &mut context as *mut StreamContext as *mut c_void;

        log_info!("Starting parallel encoding cascade...");
        let result_code = tng_encoder_process(
            encoder,
            cli_read_callback,
            cli_write_callback,
            user_data_ptr,
        );

        tng_encoder_close(encoder);

        if result_code == 0 {
            log_info!("Encoding pipeline completed successfully: {}", output_path.green());
        } else {
            log_err!("Compression error. Return Code: {}", result_code);
            process::exit(2);
        }
    }
}

fn execute_decode_pipeline(input_path: String, mut output_path: String) {
    log_info!("Decompressing stream: {}", input_path.bold());

    let input_file = match File::open(&input_path) {
        Ok(f) => f,
        Err(e) => { log_err!("Target TNG file completely unreadable: {}", e); process::exit(1); }
    };

    // Buffer raw pixels internally so we can read headers and alter output destinations dynamically
    let mut pixel_buffer = Vec::new();

    unsafe {
        let decoder = tng_decoder_init();
        if decoder.is_null() {
            log_err!("Decoder context instantiation failure");
            process::exit(1);
        }

        {
            let mut context = StreamContext {
                input_stream: Box::new(input_file),
                output_stream: Box::new(&mut pixel_buffer),
            };

            let user_data_ptr = &mut context as *mut StreamContext as *mut c_void;
            let result_code = tng_decoder_process(
                decoder,
                cli_read_callback,
                cli_write_callback,
                user_data_ptr,
            );

            if result_code != 0 {
                tng_decoder_close(decoder);
                match result_code {
                    -99 => log_info!("{}", "Cleanly aborted: Encountered unknown critical chunk variant.".yellow()),
                    _ => log_err!("Sequence failed validation requirements. Code: {}", result_code),
                }
                process::exit(3);
            }
        }

        // Fetch image specifications parsed during runtime
        let decoder_ref = &*decoder;
        let img_info = match decoder_ref.info {
            Some(info) => info,
            None => {
                log_err!("Decoder missing image info metadata.");
                tng_decoder_close(decoder);
                process::exit(3);
            }
        };

        tng_decoder_close(decoder);

        // Parse target file extension rules
        let path_obj = std::path::Path::new(&output_path);
        let mut ext = path_obj.extension()
        .and_then(|os| os.to_str())
        .unwrap_or("")
        .to_lowercase();

        // 4 = Grayscale + Alpha, 6 = RGBA
        let has_alpha = img_info.color_type == 4 || img_info.color_type == 6;

        // Force convert extension to .pam and yield yellow warning if an alpha channel is found
        if has_alpha && ext == "ppm" {
            let mut path_buf = std::path::PathBuf::from(&output_path);
            path_buf.set_extension("pam");
            output_path = path_buf.to_string_lossy().into_owned();
            ext = "pam".to_string();

            println!(
                "[{}] [{}] {}",
                Local::now().format("%H:%M:%S").bold().blue(),
                     "WARN".bold().yellow(),
                     "Image contains an alpha channel. Changing output extension to .pam".yellow()
            );
        }

        // Open actual file destination
        let mut out_file = match File::create(&output_path) {
            Ok(f) => f,
            Err(e) => { log_err!("Target out path blocked: {}", e); process::exit(1); }
        };

        // Write specific Netpbm headers if file matches target extension flags
        if ext == "ppm" || ext == "pam" {
            let maxval = if img_info.bit_depth == 16 { 65535 } else { 255 };

            if ext == "pam" {
                let depth = match img_info.color_type {
                    0 => 1,
                    2 => 3,
                    4 => 2,
                    6 => 4,
                    _ => 3,
                };
                let tupltype = match img_info.color_type {
                    0 => "GRAYSCALE",
                    2 => "RGB",
                    4 => "GRAYSCALE_ALPHA",
                    6 => "RGB_ALPHA",
                    _ => "RGB",
                };
                let header = format!(
                    "P7\nWIDTH {}\nHEIGHT {}\nDEPTH {}\nMAXVAL {}\nTUPLTYPE {}\nENDHDR\n",
                    img_info.width, img_info.height, depth, maxval, tupltype
                );
                if let Err(e) = out_file.write_all(header.as_bytes()) {
                    log_err!("Failed to write PAM header: {}", e);
                    process::exit(1);
                }
            } else {
                let header = format!("P6\n{} {}\n{}\n", img_info.width, img_info.height, maxval);
                if let Err(e) = out_file.write_all(header.as_bytes()) {
                    log_err!("Failed to write PPM header: {}", e);
                    process::exit(1);
                }
            }
        }

        // Flush out raw pixel payload data
        if let Err(e) = out_file.write_all(&pixel_buffer) {
            log_err!("Failed to write pixel data payload to disk: {}", e);
            process::exit(1);
        }

        log_info!("Target extraction sequence completed successfully: {}", output_path.green());
    }
}
