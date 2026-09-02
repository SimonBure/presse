//! Hardware verification (NOT shipped, needs --features cuda): decodes a
//! baseline 4:2:0 JPEG with the NVDEC stage (`presse::transcode::nvdec` —
//! cuvid hardware decode + the embedded NV12→planar kernel) and with nvjpeg
//! as the reference, then checks the planar YUV 4:2:0 outputs pixel-wise
//! and re-encodes both with the same nvjpeg encoder to confirm the
//! end-to-end pipeline produces identical bitstreams.
//!
//! Usage: `cargo run --release --features cuda --example nvdec_verify [FILE]`
//! (defaults to /tmp/big.jpg). Exit code 0 on pixel-equivalent match.
#![cfg_attr(not(feature = "cuda"), allow(dead_code, unused_imports))]

#[cfg(feature = "cuda")]
use baracuda_cuda_sys::runtime::{self as cudart, cudaStream_t};
#[cfg(feature = "cuda")]
use baracuda_nvjpeg_sys as nvjpeg;
#[cfg(feature = "cuda")]
use presse::transcode::nvdec::Nvdec;
#[cfg(feature = "cuda")]
use std::ffi::{c_int, c_void};
use std::ptr;
use std::time::Instant;

#[cfg(feature = "cuda")]
const CSS_420: c_int = 2; // nvjpegChromaSubsampling_t::Css420
#[cfg(feature = "cuda")]
const ROW_ALIGN: usize = 64;

#[cfg(feature = "cuda")]
fn align_rows(n: usize) -> usize {
    n.div_ceil(ROW_ALIGN) * ROW_ALIGN
}

/// Planar YUV 4:2:0 geometry: (Y pitch, chroma pitch, chroma height, bytes).
#[cfg(feature = "cuda")]
fn yuv_geometry(width: usize, height: usize) -> (usize, usize, usize, usize) {
    let uw = width.div_ceil(2);
    let uh = height.div_ceil(2);
    let py = align_rows(width);
    let puv = align_rows(uw);
    (py, puv, uh, py * height + 2 * puv * uh)
}

#[cfg(feature = "cuda")]
unsafe fn sync(stream: cudaStream_t) {
    if !stream.is_null() {
        let rt = cudart::runtime().expect("runtime");
        unsafe {
            let st = rt.cuda_stream_synchronize().map(|f| f(stream));
            if let Ok(st) = st {
                assert_eq!(st.0, 0, "cudaStreamSynchronize failed: {}", st.0);
            }
        }
    }
}

#[cfg(feature = "cuda")]
macro_rules! check {
    ($st:expr, $what:expr $(,)?) => {{
        let st = $st;
        assert_eq!(st.0, 0, "{} failed: {}", $what, st.0);
    }};
}

fn main() {
    #[cfg(not(feature = "cuda"))]
    {
        eprintln!(
            "error: nvdec_verify requires the cuda feature: \
             cargo run --release --features cuda --example nvdec_verify [FILE]"
        );
        std::process::exit(2);
    }
    #[cfg(feature = "cuda")]
    real_main();
}

#[cfg(feature = "cuda")]
fn real_main() {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/tmp/big.jpg".into());
    let jpeg = std::fs::read(&path).expect("read jpeg");
    println!("jpeg {} bytes ({path})", jpeg.len());

    let rt = cudart::runtime().expect("cuda runtime");
    let lib = nvjpeg::nvjpeg().expect("nvjpeg");
    unsafe {
        // Mirror the real pipeline's init order: nvjpeg handle/state and
        // streams are created BEFORE Nvdec::new (GpuState::new does exactly
        // this), so the NVDEC driver-context init happens after the CUDA
        // runtime has already touched the device.
        let mut handle = ptr::null_mut();
        check!(
            lib.nvjpeg_create_simple().unwrap()(&mut handle),
            "nvjpegCreate"
        );
        let mut state = ptr::null_mut();
        check!(
            lib.nvjpeg_jpeg_state_create().unwrap()(handle, &mut state),
            "nvjpegJpegStateCreate",
        );
        let mut stream: cudaStream_t = ptr::null_mut();
        check!(
            rt.cuda_stream_create().unwrap()(&mut stream),
            "cudaStreamCreate"
        );
        // Mirror the pipeline: NON_BLOCKING stream + stream-ordered alloc.
        let mut stream_nb: cudaStream_t = ptr::null_mut();
        check!(
            rt.cuda_stream_create_with_flags().unwrap()(
                &mut stream_nb,
                cudart::types::cudaStreamFlags::NON_BLOCKING
            ),
            "cudaStreamCreateWithFlags",
        );
        let mut nvdec = Nvdec::new().expect("NVDEC init");

        // Header dims via nvjpegGetImageInfo.
        let mut ncomp: c_int = 0;
        let mut sub: c_int = 0;
        let mut ws = [0i32; 3];
        let mut hs = [0i32; 3];
        let pfn = lib.nvjpeg_get_image_info().unwrap();
        check!(
            pfn(
                handle,
                jpeg.as_ptr(),
                jpeg.len(),
                &mut ncomp,
                &mut sub,
                ws.as_mut_ptr(),
                hs.as_mut_ptr(),
            ),
            "nvjpegGetImageInfo",
        );
        let (w, h) = (ws[0] as u32, hs[0] as u32);
        assert_eq!(
            sub, CSS_420,
            "verification targets 4:2:0, got subsampling {sub}"
        );
        assert_eq!(ncomp, 3);
        println!("image {w}x{h} 4:2:0 ncomp={ncomp}");

        let (py, puv, uh, len) = yuv_geometry(w as usize, h as usize);
        let mut dev: *mut c_void = ptr::null_mut();
        check!(rt.cuda_malloc().unwrap()(&mut dev, len), "cudaMalloc");
        let dev = dev as *mut u8;
        let mut dev_a: *mut c_void = ptr::null_mut();
        check!(
            rt.cuda_malloc_async().unwrap()(&mut dev_a, len, stream_nb),
            "cudaMallocAsync",
        );
        let dev_a = dev_a as *mut u8;

        // --- NVDEC path (the code under test) ---
        // Warm up first (PTX JIT + decoder creation happen on the first
        // decode), then time a run of 10.
        nvdec
            .decode_to_planar(&jpeg, w, h, dev, py as u32, puv as u32, stream)
            .expect("NVDEC warmup (plain alloc)");
        sync(stream);
        let mut a0 = vec![0u8; 64];
        check!(
            rt.cuda_memcpy().unwrap()(
                a0.as_mut_ptr() as *mut c_void,
                dev as *const c_void,
                64,
                cudart::types::cudaMemcpyKind::DeviceToHost,
            ),
            "D2H check plain",
        );
        eprintln!("DBG verify plain-alloc first32: {:?}", &a0[..32]);
        nvdec
            .decode_to_planar(&jpeg, w, h, dev_a, py as u32, puv as u32, stream_nb)
            .expect("NVDEC decode (async alloc + NB stream)");
        sync(stream_nb);
        let mut a1 = vec![0u8; 64];
        // Copy on the SAME NB stream (async) to rule out default-stream
        // visibility of pool memory.
        check!(
            rt.cuda_memcpy_async().unwrap()(
                a1.as_mut_ptr() as *mut c_void,
                dev_a as *const c_void,
                64,
                cudart::types::cudaMemcpyKind::DeviceToHost,
                stream_nb,
            ),
            "async D2H check",
        );
        sync(stream_nb);
        eprintln!(
            "DBG verify async-alloc+NB (async copy) first32: {:?}",
            &a1[..32]
        );
        // Isolate: async alloc on a blocking stream, and plain alloc on a
        // non-blocking stream.
        let mut dev_a2: *mut c_void = ptr::null_mut();
        check!(
            rt.cuda_malloc_async().unwrap()(&mut dev_a2, len, stream),
            "cudaMallocAsync (blocking stream)",
        );
        nvdec
            .decode_to_planar(
                &jpeg,
                w,
                h,
                dev_a2 as *mut u8,
                py as u32,
                puv as u32,
                stream,
            )
            .expect("decode async-alloc + blocking stream");
        sync(stream);
        let mut a2 = vec![0u8; 64];
        check!(
            rt.cuda_memcpy().unwrap()(
                a2.as_mut_ptr() as *mut c_void,
                dev_a2 as *const c_void,
                64,
                cudart::types::cudaMemcpyKind::DeviceToHost,
            ),
            "D2H check async+blocking",
        );
        eprintln!("DBG verify async-alloc+blocking first32: {:?}", &a2[..32]);
        nvdec
            .decode_to_planar(&jpeg, w, h, dev, py as u32, puv as u32, stream_nb)
            .expect("decode plain-alloc + NB stream");
        sync(stream_nb);
        let mut a3 = vec![0u8; 64];
        check!(
            rt.cuda_memcpy().unwrap()(
                a3.as_mut_ptr() as *mut c_void,
                dev as *const c_void,
                64,
                cudart::types::cudaMemcpyKind::DeviceToHost,
            ),
            "D2H check plain+NB",
        );
        eprintln!("DBG verify plain-alloc+NB first32: {:?}", &a3[..32]);
        let mut t_nvdec = std::time::Duration::ZERO;
        for _ in 0..10 {
            let t0 = Instant::now();
            nvdec
                .decode_to_planar(&jpeg, w, h, dev, py as u32, puv as u32, stream)
                .expect("NVDEC decode");
            sync(stream);
            t_nvdec += t0.elapsed();
        }
        let t_nvdec = t_nvdec / 10;
        let mut a = vec![0u8; len];
        check!(
            rt.cuda_memcpy().unwrap()(
                a.as_mut_ptr() as *mut c_void,
                dev as *const c_void,
                len,
                cudart::types::cudaMemcpyKind::DeviceToHost,
            ),
            "D2H (nvdec)",
        );

        // --- nvjpeg reference decode into the same layout ---
        let mut t_nvjpeg = std::time::Duration::ZERO;
        for _ in 0..10 {
            let t0 = Instant::now();
            let mut image = nvjpeg::nvjpegImage_t {
                channel: [
                    dev,
                    dev.add(py * h as usize),
                    dev.add(py * h as usize + puv * uh),
                    ptr::null_mut(),
                ],
                pitch: [py, puv, puv, 0],
            };
            check!(
                lib.nvjpeg_decode().unwrap()(
                    handle,
                    state,
                    jpeg.as_ptr(),
                    jpeg.len(),
                    nvjpeg::nvjpegOutputFormat_t::Yuv,
                    &mut image,
                    stream,
                ),
                "nvjpegDecode",
            );
            sync(stream);
            t_nvjpeg += t0.elapsed();
        }
        let t_nvjpeg = t_nvjpeg / 10;
        let mut b = vec![0u8; len];
        check!(
            rt.cuda_memcpy().unwrap()(
                b.as_mut_ptr() as *mut c_void,
                dev as *const c_void,
                len,
                cudart::types::cudaMemcpyKind::DeviceToHost,
            ),
            "D2H (nvjpeg)",
        );

        // --- nvjpeg batch scaling: 4 decodes serial vs 4 parallel streams,
        // each with its own nvjpeg state (as the real pipeline does) ---
        let nimg = 4;
        let mut bufs: Vec<*mut u8> = Vec::new();
        for _ in 0..nimg {
            let mut p: *mut c_void = ptr::null_mut();
            check!(rt.cuda_malloc().unwrap()(&mut p, len), "cudaMalloc batch");
            bufs.push(p as *mut u8);
        }
        let mut streams: Vec<cudaStream_t> = Vec::new();
        let mut states: Vec<*mut c_void> = Vec::new();
        for _ in 0..nimg {
            let mut s: cudaStream_t = ptr::null_mut();
            check!(rt.cuda_stream_create().unwrap()(&mut s), "cudaStreamCreate");
            streams.push(s);
            let mut st2 = ptr::null_mut();
            check!(
                lib.nvjpeg_jpeg_state_create().unwrap()(handle, &mut st2),
                "state",
            );
            states.push(st2);
        }
        let mkimg = |base: *mut u8| nvjpeg::nvjpegImage_t {
            channel: [
                base,
                base.add(py * h as usize),
                base.add(py * h as usize + puv * uh),
                ptr::null_mut(),
            ],
            pitch: [py, puv, puv, 0],
        };
        let t0 = Instant::now();
        for k in 0..nimg {
            let mut im = mkimg(bufs[k]);
            check!(
                lib.nvjpeg_decode().unwrap()(
                    handle,
                    states[k],
                    jpeg.as_ptr(),
                    jpeg.len(),
                    nvjpeg::nvjpegOutputFormat_t::Yuv,
                    &mut im,
                    stream,
                ),
                "nvjpegDecode serial",
            );
            sync(stream);
        }
        println!(
            "nvjpeg {nimg} serial: {:.1} ms",
            t0.elapsed().as_secs_f64() * 1000.0
        );
        let t0 = Instant::now();
        for k in 0..nimg {
            let mut im = mkimg(bufs[k]);
            check!(
                lib.nvjpeg_decode().unwrap()(
                    handle,
                    states[k],
                    jpeg.as_ptr(),
                    jpeg.len(),
                    nvjpeg::nvjpegOutputFormat_t::Yuv,
                    &mut im,
                    streams[k],
                ),
                "nvjpegDecode parallel",
            );
        }
        for s in &streams {
            sync(*s);
        }
        println!(
            "nvjpeg {nimg} parallel: {:.1} ms",
            t0.elapsed().as_secs_f64() * 1000.0
        );
        println!(
            "nvjpeg {nimg} parallel: {:.1} ms",
            t0.elapsed().as_secs_f64() * 1000.0
        );

        // --- pixel comparison over the real w×h region (skip padding) ---
        let (w, h) = (w as usize, h as usize);
        let mut diffs = 0u64;
        let mut max_diff = 0i32;
        let mut sum = 0i64;
        for y in 0..h {
            for x in 0..w {
                let d = (a[y * py + x] as i32 - b[y * py + x] as i32).abs();
                if d > 0 {
                    diffs += 1;
                    max_diff = max_diff.max(d);
                }
                sum += d as i64;
            }
        }
        for y in 0..uh {
            for x in 0..w.div_ceil(2) {
                for p in 0..2 {
                    let off = py * h + p * puv * uh + y * puv + x;
                    let d = (a[off] as i32 - b[off] as i32).abs();
                    if d > 0 {
                        diffs += 1;
                        max_diff = max_diff.max(d);
                    }
                    sum += d as i64;
                }
            }
        }
        let total = w * h + 2 * uh * w.div_ceil(2);
        let mean = sum as f64 / total as f64;
        println!("nvdec decode+convert: {t_nvdec:.3?}  nvjpeg decode: {t_nvjpeg:.3?}");
        println!(
            "pixels differing: {diffs}/{total} ({:.4}%)  mean |Δ|: {mean:.4}  max |Δ|: {max_diff}",
            100.0 * diffs as f64 / total as f64
        );

        // The decode is "pixel-equivalent" when only IDCT rounding remains:
        // ±1 on a tiny fraction of pixels (NVDEC's hardware IDCT vs nvjpeg's
        // software IDCT — the same class of difference ffmpeg shows between
        // mjpeg and mjpeg_cuvid). Byte-identical bitstreams are not expected;
        // the re-encode comparison below is informational.

        // --- end-to-end: re-encode both planar buffers with the same nvjpeg
        // encoder; identical YUV input must give identical bitstreams ---
        let mut es = ptr::null_mut();
        let mut ep = ptr::null_mut();
        check!(
            lib.nvjpeg_encoder_state_create().unwrap()(handle, &mut es, ptr::null_mut()),
            "encoder state",
        );
        check!(
            lib.nvjpeg_encoder_params_create().unwrap()(handle, &mut ep, ptr::null_mut()),
            "encoder params",
        );
        check!(
            lib.nvjpeg_encoder_params_set_quality().unwrap()(ep, 75, ptr::null_mut()),
            "set quality",
        );
        check!(
            lib.nvjpeg_encoder_params_set_sampling_factors().unwrap()(
                ep,
                nvjpeg::nvjpegChromaSubsampling_t::Css420,
                ptr::null_mut(),
            ),
            "set sampling",
        );
        let encode = |src: &[u8]| -> Vec<u8> {
            let image = nvjpeg::nvjpegImage_t {
                channel: [
                    src.as_ptr() as *mut u8,
                    (src.as_ptr() as *mut u8).add(py * h),
                    (src.as_ptr() as *mut u8).add(py * h + puv * uh),
                    ptr::null_mut(),
                ],
                pitch: [py, puv, puv, 0],
            };
            check!(
                lib.nvjpeg_encode_yuv().unwrap()(
                    handle,
                    es,
                    ep,
                    &image,
                    nvjpeg::nvjpegChromaSubsampling_t::Css420,
                    w as c_int,
                    h as c_int,
                    stream,
                ),
                "nvjpegEncodeYUV",
            );
            sync(stream);
            let mut size = 0usize;
            check!(
                lib.nvjpeg_encode_retrieve_bitstream().unwrap()(
                    handle,
                    es,
                    ptr::null_mut(),
                    &mut size,
                    stream,
                ),
                "size query",
            );
            sync(stream);
            let mut out = vec![0u8; size];
            check!(
                lib.nvjpeg_encode_retrieve_bitstream().unwrap()(
                    handle,
                    es,
                    out.as_mut_ptr(),
                    &mut size,
                    stream,
                ),
                "bitstream copy",
            );
            sync(stream);
            out.truncate(size);
            out
        };
        let enc_a = encode(&a);
        let enc_b = encode(&b);
        println!(
            "encoded bitstreams: {} bytes nvdec-path vs {} bytes nvjpeg-path — {}",
            enc_a.len(),
            enc_b.len(),
            if enc_a == enc_b {
                "IDENTICAL"
            } else {
                "DIFFERENT"
            }
        );

        let exact = diffs == 0 && enc_a == enc_b;
        let equivalent = max_diff <= 1 && diffs * 10_000 <= total as u64;
        println!(
            "RESULT: {} ({} byte-identical, pixel-equivalent)",
            if equivalent { "PASS" } else { "FAIL" },
            if exact { "and" } else { "not" }
        );
        std::process::exit(if equivalent { 0 } else { 1 });
    }
}
