#[macro_use]
mod macros;
mod cli;
mod format;
mod img;
mod pdf;
mod press;
mod transcode;

// Rayon workers constantly allocate and drop temporary buffers during image
// transcoding; mimalloc has per-thread caches that avoid contention on the
// system allocator. It builds against musl given a musl C toolchain
// (musl-gcc), so the release artifact gets it on every target.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::path::Path;

use format::{Format, detect};
use pdf::builder::image_to_pdf;
use pdf::images::compress_images;
use pdf::merger::merge;
use pdf::reader::{
    get_compression_ratio_in_percent, get_pdf_size_in_kilobytes, load_input_as_pdf, map_file,
};
use pdf::writer::{compress_and_save_pdf, save_pdf};
use press::{PdfOptions, PressOptions, Pressed, Presser};

use clap::Parser;
use cli::args::{
    Cli, Commands, CompressionMode, resolve_convert_format, resolve_convert_output,
    resolve_convert_path_output, resolve_merge_path_output, resolve_press_output,
    resolve_press_path_output,
};
use img::convert::convert_image;

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
            keep_metadata,
            verbose,
        } => {
            // Flags that only mean something for PDFs, as typed (before the
            // presets expand). Image inputs ignore them, with one warning,
            // so a mixed batch never fails because of them.
            let pdf_only: Vec<&str> = [
                (dpi.is_some(), "--dpi"),
                (raster_classify, "--raster-classify"),
                (recompress_flate, "--recompress-flate"),
                (dedup, "--dedup"),
                (zopfli, "--zopfli"),
                (font_subset, "--font-subset"),
                (jbig2, "--jbig2"),
                (jpeg2000, "--jpeg2000"),
                (mrc, "--mrc"),
                (compression != CompressionMode::Fast, "--compression"),
            ]
            .into_iter()
            .filter_map(|(set, name)| set.then_some(name))
            .collect();
            let mut warned_pdf_only = false;

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
            let presser = Presser::new(
                PressOptions {
                    quality,
                    ssim,
                    palette,
                    keep_metadata,
                    pdf: PdfOptions {
                        dpi,
                        raster_classify,
                        recompress_flate,
                        dedup,
                        zopfli,
                        font_subset,
                        jbig2,
                        jpeg2000,
                        mrc,
                    },
                    verbose,
                },
                acceleration,
                jpeg_encoder,
            )?;
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
                let data = match map_file(file_path) {
                    Ok(data) => data,
                    Err(e) => {
                        eprintln!("Skipping {}: {}", file_path.display(), e);
                        continue;
                    }
                };
                let image_format = detect(&data).filter(|f| *f != Format::Pdf);
                if image_format.is_some() && !pdf_only.is_empty() && !warned_pdf_only {
                    eprintln!(
                        "warning: {} only apply to PDFs; ignored for images",
                        pdf_only.join(", ")
                    );
                    warned_pdf_only = true;
                }

                // Unmap the input before writing (the output may be the input
                // file itself). PDFs release it right after parsing: lopdf
                // copies what it needs, and compression can take a while.
                let pressed: Result<Pressed, Box<dyn std::error::Error>> = if image_format.is_some()
                {
                    let pressed = presser.press(&data);
                    drop(data);
                    pressed
                } else {
                    let doc = lopdf::Document::load_mem(&data);
                    drop(data);
                    doc.map(|doc| Pressed::Pdf(Box::new(presser.press_pdf(doc))))
                        .map_err(Into::into)
                };
                let pressed = match pressed {
                    Ok(pressed) => pressed,
                    Err(e) => {
                        eprintln!("Skipping {}: {}", file_path.display(), e);
                        continue;
                    }
                };

                let output = match image_format {
                    Some(f) => {
                        let ext = file_path.extension().and_then(|e| e.to_str());
                        resolve_press_output(file_path, &output, ext.unwrap_or(f.extension()))
                    }
                    None => resolve_press_path_output(file_path, &output),
                };
                let kept = match pressed {
                    Pressed::Pdf(mut doc) => {
                        compress_and_save_pdf(&mut doc, output.to_str().unwrap(), verbose)?;
                        None
                    }
                    Pressed::Image(bytes) => {
                        std::fs::write(&output, bytes)?;
                        None
                    }
                    Pressed::Unchanged(reason) => {
                        if !same_file(file_path, &output) {
                            std::fs::copy(file_path, &output)?;
                        }
                        Some(reason)
                    }
                };

                // Compression summary
                if verbose {
                    let original_size =
                        get_pdf_size_in_kilobytes(file_path.to_str().unwrap()).unwrap();
                    let compressed_size =
                        get_pdf_size_in_kilobytes(output.to_str().unwrap()).unwrap();
                    let compression_ratio =
                        get_compression_ratio_in_percent(original_size, compressed_size);
                    let note = kept
                        .map(|reason| format!(", original kept: {reason}"))
                        .unwrap_or_default();
                    bar.println(format!(
                        "{}kB → {}kB ({:.2}% compression){}",
                        original_size, compressed_size, compression_ratio, note
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
            to,
            merge: do_merge,
            compress,
            quality,
            verbose,
        } => {
            // Output format: the -o extension, else --to, else PDF (the
            // historical behavior).
            let target = match resolve_convert_format(&output, to) {
                Ok(target) => target,
                Err(e) => {
                    eprintln!("Error: {e}");
                    std::process::exit(1);
                }
            };
            if target != Format::Pdf {
                convert_to_images(
                    &input, &output, target, do_merge, compress, quality, verbose,
                )?;
            } else if do_merge {
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

                let out = Taken::new(&input).claim(&resolve_merge_path_output(&output, compress));
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

                let mut taken = Taken::new(&input);
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

                    let out = taken.claim(&resolve_convert_path_output(path, &output));
                    if compress {
                        compress_and_save_pdf(&mut doc, out.to_str().unwrap(), verbose)?;
                    } else {
                        save_pdf(&mut doc, out.to_str().unwrap())?;
                    }
                    taken.record(&out);
                }
            }
        }
    }

    Ok(())
}

/// `convert` to JPEG, PNG or WebP: one output file per input.
fn convert_to_images(
    input: &[std::path::PathBuf],
    output: &Option<std::path::PathBuf>,
    target: Format,
    merge: bool,
    compress: bool,
    quality: u8,
    verbose: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if merge {
        eprintln!("Error: --merge needs PDF output");
        std::process::exit(1);
    }
    if compress {
        eprintln!(
            "warning: --compress applies to PDF output only; ignored (-q sets the JPEG quality)"
        );
    }
    // Same multi-file/-o guard as the PDF path.
    if input.len() > 1
        && let Some(path) = output
        && !path.is_dir()
        && !path.to_str().unwrap().ends_with('/')
    {
        eprintln!("Error: -o must be a directory when converting multiple images");
        std::process::exit(1);
    }
    let kb = |n: usize| format!("{:.1} kB", n as f64 / 1000.0);

    let mut taken = Taken::new(input);
    for path in input {
        let converted = std::fs::read(path)
            .map_err(Into::into)
            .and_then(|source| convert_image(&source, target, quality).map(|c| (source, c)));
        let (source, converted) = match converted {
            Ok(pair) => pair,
            Err(e) => {
                eprintln!("Skipping {}: {}", path.display(), e);
                continue;
            }
        };
        let out = taken.claim(&resolve_convert_output(path, output, target.extension()));
        std::fs::write(&out, &converted)?;
        taken.record(&out);
        if converted.len() > source.len() {
            eprintln!(
                "warning: {} ({}) is larger than {} ({})",
                out.display(),
                kb(converted.len()),
                path.display(),
                kb(source.len())
            );
        }
        verbose!(
            verbose,
            "{} → {} ({} → {})",
            path.display(),
            out.display(),
            kb(source.len()),
            kb(converted.len())
        );
    }
    Ok(())
}

/// Identifies an existing file whatever the path spelling (`./`, symlinks,
/// letter case on case-insensitive disks).
#[cfg(unix)]
type FileId = (u64, u64);

#[cfg(unix)]
fn file_id(path: &Path) -> Option<FileId> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.dev(), meta.ino()))
}

#[cfg(not(unix))]
type FileId = std::path::PathBuf;

#[cfg(not(unix))]
fn file_id(path: &Path) -> Option<FileId> {
    path.canonicalize().ok()
}

/// True when both paths name the same existing file.
fn same_file(a: &Path, b: &Path) -> bool {
    file_id(a).is_some_and(|id| file_id(b) == Some(id))
}

/// Files a `convert` run must not write over: its inputs and the outputs it
/// already wrote. Any other existing file is overwritten, as before, so a
/// re-run produces the same names.
struct Taken {
    inputs: std::collections::HashSet<FileId>,
    written: std::collections::HashSet<FileId>,
}

impl Taken {
    fn new(inputs: &[std::path::PathBuf]) -> Self {
        Self {
            inputs: inputs.iter().filter_map(|p| file_id(p)).collect(),
            written: Default::default(),
        }
    }

    /// `wanted`, or when it is taken the first free `<stem>_1.<ext>`,
    /// `<stem>_2.<ext>`… next to it.
    fn claim(&self, wanted: &Path) -> std::path::PathBuf {
        let free = |p: &Path| {
            file_id(p).is_none_or(|id| !self.inputs.contains(&id) && !self.written.contains(&id))
        };
        if free(wanted) {
            return wanted.to_path_buf();
        }
        let numbered = |n: u32| {
            let mut name = wanted.file_stem().unwrap_or_default().to_os_string();
            name.push(format!("_{n}"));
            if let Some(ext) = wanted.extension() {
                name.push(".");
                name.push(ext);
            }
            wanted.with_file_name(name)
        };
        let out = (1..)
            .map(numbered)
            .find(|p| free(p))
            .expect("some numbered name is free");
        let why = if file_id(wanted).is_some_and(|id| self.inputs.contains(&id)) {
            "is one of the inputs"
        } else {
            "was already written by this run"
        };
        eprintln!(
            "warning: {} {why}; writing {} instead",
            wanted.display(),
            out.display()
        );
        out
    }

    /// Mark a file this run just wrote.
    fn record(&mut self, written: &Path) {
        if let Some(id) = file_id(written) {
            self.written.insert(id);
        }
    }
}
