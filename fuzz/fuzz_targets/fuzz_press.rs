//! Fuzz target for the press pipeline: arbitrary bytes → parse → compress →
//! serialize. The fuzzer feeds malformed/truncated PDFs and any panic in the
//! parse, image re-encode, or writer path is a finding.
//!
//! Run with: `cargo fuzz run fuzz_press -- -max_total_time=300 -timeout=30`
#![no_main]

use libfuzzer_sys::fuzz_target;

use presse::pdf::images::{QualityMode, compress_images, compress_images_with};
use presse::pdf::writer::{compress_and_save_pdf, recompress_flate};

const OUT: &str = "/tmp/presse-fuzz-out.pdf";

fuzz_target!(|data: &[u8]| {
    // Guard the decompression bomb: cap what we're willing to parse.
    if data.len() > 4 * 1024 * 1024 {
        return;
    }
    let Ok(mut doc) = lopdf::Document::load_mem(data) else {
        return; // rejected input — not a crash
    };
    // Exercise the calibrated `-ssim` path too (quality derived from the
    // target via the committed curve), then the plain `-q` path.
    compress_images_with(
        &mut doc,
        QualityMode::press(50, Some(0.72)),
        false,
        &presse::transcode::CpuTranscoder::default(),
        None,
        false,
        false,
    );
    let _ = compress_and_save_pdf(&mut doc, OUT, false);
    let Ok(mut doc2) = lopdf::Document::load_mem(data) else {
        return;
    };
    compress_images(&mut doc2, 50, false);
    let _ = compress_and_save_pdf(&mut doc2, OUT, false);
    // Exercise the `--jpeg-encoder` 4:2:0 codec path.
    let Ok(mut doc2b) = lopdf::Document::load_mem(data) else {
        return;
    };
    compress_images_with(
        &mut doc2b,
        QualityMode::fixed(50),
        false,
        &presse::transcode::CpuTranscoder::new(true),
        None,
        false,
        false,
    );
    let _ = compress_and_save_pdf(&mut doc2b, OUT, false);
    // Exercise the `--palette` candidate path (the indexed-candidate builder
    // and the median-cut quantizer run).
    let Ok(mut doc3) = lopdf::Document::load_mem(data) else {
        return;
    };
    compress_images_with(
        &mut doc3,
        QualityMode::fixed(50),
        false,
        &presse::transcode::CpuTranscoder::default(),
        None,
        true,
        false,
    );
    let _ = compress_and_save_pdf(&mut doc3, OUT, false);
    // Exercise the `--raster-classify` path (classifier, mask candidate,
    // CCITT G4 encoder, indexed routing) plus `--recompress-flate`.
    let Ok(mut doc4) = lopdf::Document::load_mem(data) else {
        return;
    };
    compress_images_with(
        &mut doc4,
        QualityMode::fixed(50),
        false,
        &presse::transcode::CpuTranscoder::new(true),
        None,
        true,
        true,
    );
    let _ = recompress_flate(&mut doc4);
    let _ = compress_and_save_pdf(&mut doc4, OUT, false);
});
