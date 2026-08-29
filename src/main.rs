#[macro_use]
mod macros;
mod cli;
mod pdf;
mod transcode;

// Rayon workers constantly allocate and drop temporary buffers during image
// transcoding; mimalloc has per-thread caches that avoid contention on the
// system allocator. It builds against musl given a musl C toolchain
// (musl-gcc), so the release artifact gets it on every target.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use pdf::builder::image_to_pdf;
use pdf::images::{CompressOptions, QualityMode, compress_images, compress_images_opt};
use pdf::merger::merge;
use pdf::reader::{
    get_compression_ratio_in_percent, get_pdf_size_in_kilobytes, load_input_as_pdf, load_pdf,
};
use pdf::writer::{compress_and_save_pdf, recompress_flate as recompress_flate_streams, save_pdf};
use transcode::resolve;

use clap::Parser;
use cli::args::{
    Cli, Commands, CompressionMode, resolve_convert_path_output, resolve_merge_path_output,
    resolve_press_path_output,
};

use indicatif::{ProgressBar, ProgressStyle};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Press {
            input,
            output,
            quality,
            acceleration,
            dpi,
            ssim,
            palette,
            jpeg_encoder,
            recompress_flate,
            raster_classify,
            dedup,
            zopfli,
            font_subset,
            jbig2,
            jpeg2000,
            mrc,
            compression,
            verbose,
        } => {
            // `--compression` presets expand to the individual flags; the
            // user's own flags always win (a preset never *un*-sets one).
            let (dedup, zopfli, font_subset, jbig2, jpeg2000, mrc, recompress_flate) =
                match compression {
                    CompressionMode::Fast => (
                        dedup,
                        zopfli,
                        font_subset,
                        jbig2,
                        jpeg2000,
                        mrc,
                        recompress_flate,
                    ),
                    CompressionMode::Balanced => (
                        true,
                        zopfli,
                        font_subset,
                        jbig2,
                        jpeg2000,
                        mrc,
                        recompress_flate,
                    ),
                    CompressionMode::Small => (true, true, font_subset, jbig2, jpeg2000, mrc, true),
                    CompressionMode::Smallest => (true, true, true, true, true, true, true),
                };
            // The `optimize`-feature passes are default-off CLI flags; when
            // the binary was built without the feature, asking for them is
            // an explicit, actionable error instead of a silent no-op.
            #[cfg(not(feature = "optimize"))]
            if dedup || zopfli || font_subset || jbig2 || jpeg2000 || mrc {
                eprintln!(
                    "Error: --dedup/--zopfli/--font-subset/--jbig2/--jpeg2000/--mrc (and \
                     --compression {compression:?}) require a build with the `optimize` feature:\n\
                       cargo build --release --features optimize"
                );
                std::process::exit(1);
            }
            // Resolve the transcoding backend up front: requesting a backend
            // that was not compiled in is an explicit error; a compiled-in
            // backend whose driver is missing warns and falls back to CPU.
            // `--jpeg-encoder` selects the 4:2:0 codec for the CPU path.
            let transcoder = resolve(acceleration, jpeg_encoder)?;
            let bar = ProgressBar::new(input.len() as u64);
            bar.set_style(
                ProgressStyle::default_bar()
                    .template("{bar:40.cyan/blue} {pos}/{len} {eta}")
                    .unwrap(),
            );

            // Fail if multiple files + output are given & output is not a dir
            if input.len() > 1
                && let Some(ref path) = output
                && !path.is_dir()
                && !path.to_str().unwrap().ends_with('/')
            {
                eprintln!("Error: -o must be a directory when compressing multiple documents");
                std::process::exit(1);
            }

            // Create output dir if needed (once)
            if let Some(ref path) = output {
                if let Some(parent) = path.parent()
                    && !parent.as_os_str().is_empty()
                {
                    std::fs::create_dir_all(parent)?;
                }
                // If -o is a directory itself (ends with /)
                if path.to_str().unwrap().ends_with('/') {
                    std::fs::create_dir_all(path)?;
                }
            }

            for file_path in &input {
                // Loading the document
                let mut doc = match load_pdf(file_path.to_str().unwrap()) {
                    Ok(doc) => doc,
                    Err(e) => {
                        eprintln!("Skipping {}: {}", file_path.display(), e);
                        continue;
                    }
                };

                compress_images_opt(
                    &mut doc,
                    QualityMode::press(quality, ssim),
                    verbose,
                    &transcoder,
                    CompressOptions {
                        dpi,
                        palette,
                        classify: raster_classify,
                        jbig2,
                        jpeg2000,
                        mrc,
                    },
                );

                // `--dedup` (optimize feature): extend the duplicate-image
                // coalescing to every byte-identical non-image stream.
                if dedup {
                    let n = pdf::optimize::dedup_streams(&mut doc);
                    verbose!(verbose, "[dedup] coalesced {n} duplicate stream object(s)");
                }

                // `--recompress-flate` (qpdf-style): re-encode existing
                // Flate streams at the writer's level before saving.
                if recompress_flate {
                    let n = recompress_flate_streams(&mut doc);
                    verbose!(
                        verbose,
                        "[writer] recompressed {n} existing FlateDecode stream(s)"
                    );
                }

                // `--zopfli` (optimize feature): the same recompression with
                // the deliberately slower Zopfli search (strictly-smaller
                // gate, pure size win at CPU cost).
                if zopfli {
                    let n = pdf::optimize::recompress_flate_zopfli(&mut doc);
                    verbose!(verbose, "[zopfli] recompressed {n} FlateDecode stream(s)");
                }

                // `--font-subset` (optimize feature): subset embedded
                // TrueType/CFF fonts to the glyphs the content actually
                // uses; every font is skipped unless the used-glyph mapping
                // resolves exactly and the subset is strictly smaller.
                if font_subset {
                    let n = pdf::optimize::subset_fonts(&mut doc);
                    verbose!(verbose, "[fonts] subset {n} font(s)");
                }

                // Compressing the document
                let output = resolve_press_path_output(file_path, &output);
                compress_and_save_pdf(&mut doc, output.to_str().unwrap(), verbose)?;

                // Compression summary
                if verbose {
                    let original_size =
                        get_pdf_size_in_kilobytes(file_path.to_str().unwrap()).unwrap();
                    let compressed_size =
                        get_pdf_size_in_kilobytes(output.to_str().unwrap()).unwrap();
                    let compression_ratio =
                        get_compression_ratio_in_percent(original_size, compressed_size);
                    bar.println(format!(
                        "{}kB → {}kB ({:.2}% compression)",
                        original_size, compressed_size, compression_ratio
                    ));
                }

                bar.inc(1);
            }

            bar.finish_with_message("Done");
        }

        Commands::Merge {
            input,
            output,
            compress,
        } => {
            let mut documents = Vec::new();
            for path in &input {
                match load_input_as_pdf(path, false) {
                    Ok(d) => documents.push(d),
                    Err(e) => eprintln!("Skipping {}: {}", path.display(), e),
                }
            }

            // If compress => compress all inputs first then merge. If not, just merge.
            if compress {
                for doc in &mut documents {
                    compress_images(doc, 50, false);
                }
            }

            let output = resolve_merge_path_output(&output, compress);

            let mut merged = merge(documents)?;
            save_pdf(&mut merged, output.to_str().unwrap())?;
        }

        Commands::Convert {
            input,
            output,
            merge: do_merge,
            compress,
            quality,
            verbose,
        } => {
            if do_merge {
                let mut docs = Vec::new();
                for path in &input {
                    match image_to_pdf(path, verbose) {
                        Ok(d) => docs.push(d),
                        Err(e) => eprintln!("Skipping {}: {}", path.display(), e),
                    }
                }

                let mut merged = merge(docs)?;
                if compress {
                    compress_images(&mut merged, quality, verbose);
                }

                let out = resolve_merge_path_output(&output, compress);
                if compress {
                    compress_and_save_pdf(&mut merged, out.to_str().unwrap(), verbose)?;
                } else {
                    save_pdf(&mut merged, out.to_str().unwrap())?;
                }
            } else {
                // Same multi-file/-o guard as Press: -o must be a dir when >1 input.
                if input.len() > 1
                    && let Some(ref path) = output
                    && !path.is_dir()
                    && !path.to_str().unwrap().ends_with('/')
                {
                    eprintln!("Error: -o must be a directory when converting multiple images");
                    std::process::exit(1);
                }

                for path in &input {
                    let mut doc = match image_to_pdf(path, verbose) {
                        Ok(d) => d,
                        Err(e) => {
                            eprintln!("Skipping {}: {}", path.display(), e);
                            continue;
                        }
                    };

                    if compress {
                        compress_images(&mut doc, quality, verbose);
                    }

                    let out = resolve_convert_path_output(path, &output);
                    if compress {
                        compress_and_save_pdf(&mut doc, out.to_str().unwrap(), verbose)?;
                    } else {
                        save_pdf(&mut doc, out.to_str().unwrap())?;
                    }
                }
            }
        }
    }

    Ok(())
}
