//! NVIDIA nvJPEG backend (`--acceleration cuda`, feature = "cuda").
//!
//! Uses [`baracuda-nvjpeg-sys`]'s dynamic loader, which dlopens
//! `libnvjpeg.so`/`nvjpeg64.dll` at runtime — no CUDA toolkit needed to
//! build, no link-time driver dependency. Any failure (library missing,
//! driver missing, context/VRAM errors, per-stream errors) surfaces as
//! [`TranscodeError`] and the caller's [`FallbackTranscoder`] degrades to
//! the CPU backend.
//!
//! The backend is a dedicated GPU consumer thread fed by the rayon
//! producers through an mpsc queue: the CPU (small streams, grayscale and
//! raw-RGB encodes, stream hashing, lopdf work) and the GPU (large JPEG
//! re-encodes) run concurrently instead of one engine waiting on the other.
//!
//! A batch of up to [`BATCH_MAX`] images is fanned out across that many
//! per-slot CUDA streams, and every image is *device-resident*: `nvjpegDecode`
//! writes directly to device memory (planar YUV) and `nvjpegEncodeYUV` reads
//! back from that same device memory — no decoded pixel crosses the PCIe
//! bus (a host round-trip design moves ~26 MB per 17.5 MP image in *and*
//! back out for every image). Only the compressed bitstreams come back to
//! the host, through a reusable pinned-memory pool.
//!
//! The handle uses the GPU-hybrid backend (`NVJPEG_BACKEND_GPU_HYBRID`,
//! entropy decode on the GPU), created via `nvjpegCreateEx` because the V1
//! `nvjpegCreate` rejects it on current drivers — see the measured
//! same-session A/B at the creation site. The nvJPEG hardware decoder
//! (`NVJPEG_BACKEND_HARDWARE`) is not reachable through `nvjpegCreateEx` or
//! `nvjpegDecoderCreate` on current drivers, and
//! `nvjpegDecodeBatchedInitialize` returns `NVJPEG_STATUS_BAD_PARAMETER` on
//! every backend and batch size here — neither is usable, so decode is
//! per-image on per-slot streams.
//!
//! `/DeviceGray` streams never reach the GPU — nvJPEG has no single-channel
//! input format, so the CPU backend encodes them (which also keeps the
//! output valid for the stream's color space).
//!
//! **Teardown is deliberate:** the nvjpeg handles, states, params, and CUDA
//! streams in [`GpuState`] are never explicitly destroyed — the no-op
//! [`Drop for GpuState`](GpuState) documents why (the C++ destroy functions
//! can throw across the FFI boundary). Do not add a destructor.
//!
//! **NVDEC decode stage:** baseline 4:2:0 JPEGs are decoded on the dedicated
//! hardware engine through the Video Codec SDK (libnvcuvid, [`super::nvdec`])
//! instead of nvJPEG's entropy-decode kernels, then converted NV12 → planar
//! YUV on device and handed to the same `nvjpegEncodeYUV` encode path.
//! Progressive / 4:2:2 / 4:4:4 JPEGs keep the nvJPEG decode. The stage is
//! optional: if it fails to initialize, decode stays on nvJPEG.

use baracuda_cuda_sys::runtime::types::cudaStreamFlags;
use baracuda_cuda_sys::runtime::{self as cudart, cudaError_t, cudaStream_t};
use baracuda_nvjpeg_sys as nvjpeg;

use crate::transcode::nvdec::{self, Nvdec};
use crate::transcode::{ImageRef, ImageTranscoder, Input, TranscodeError, jpeg_components};
use std::ffi::{c_int, c_void};
use std::ptr;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;
use std::time::Duration;

/// `nvjpegInputFormat_t` for the interleaved-RGB fallback encode.
const NVJPEG_INPUT_RGBI: c_int = 5; // interleaved RGB

/// `nvjpegChromaSubsampling_t` values returned by `nvjpegGetImageInfo`.
const CSS_444: c_int = 0;
const CSS_422: c_int = 1;
const CSS_420: c_int = 2;

/// Build optimized Huffman tables for every encode (1) or use the standard
/// tables (0).
///
/// The optimized-table pass runs on the CPU, so disabling it is 12–25%
/// faster wall time on many-image documents and ~0 on single large images,
/// at the cost of ~7–8% larger output. Speed is the point of the GPU
/// backend, so it defaults off.
const OPTIMIZED_HUFFMAN: c_int = 0;

/// Row pitch alignment for device decode/encode buffers. nvjpeg's GPU
/// kernels are written against the caller's pitch; 64-byte rows keep them
/// off the slow path (and avoid odd-pitch edge cases on device output).
const ROW_ALIGN: usize = 64;

/// Maximum images processed concurrently. The decode/encoder-state pools,
/// the per-slot streams, and the pinned output pool are all sized to this.
const BATCH_MAX: usize = 16;

/// Maximum total decoded YUV bytes per batch. Keeps peak device memory
/// bounded on small-VRAM cards when a batch contains several huge photos
/// (16 × 17.5 MP × 1.5 B/px would be ~420 MB).
const BATCH_BUDGET_BYTES: usize = 384 * 1024 * 1024;

/// How long the consumer waits for more jobs to fill a batch after the
/// first one. Jobs arrive from 16 rayon workers in bursts, so this is
/// usually zero; the bound just prevents a lone job from being processed
/// alone when producers are briefly busy elsewhere.
const COALESCE_TIMEOUT: Duration = Duration::from_micros(300);

fn check(status: nvjpeg::nvjpegStatus_t) -> Result<(), TranscodeError> {
    if status.0 == 0 {
        Ok(())
    } else {
        Err(TranscodeError::Gpu(format!("nvjpeg status {}", status.0)))
    }
}

fn check_cuda(status: cudaError_t) -> Result<(), TranscodeError> {
    if status.0 == 0 {
        Ok(())
    } else {
        Err(TranscodeError::Gpu(format!("cuda error {}", status.0)))
    }
}

fn align_rows(n: usize) -> usize {
    n.div_ceil(ROW_ALIGN) * ROW_ALIGN
}

/// Host memory that is pinned (`cudaHostAlloc`) for DMA-direct copies when
/// possible, falling back to the heap when pinned allocation is unavailable.
/// Pinned memory is what makes the async pipeline fast: pageable buffers
/// would be staged through the driver anyway.
enum HostBuf {
    Pinned { ptr: *mut u8, len: usize },
    Heap(Vec<u8>),
}

impl HostBuf {
    fn new(len: usize) -> Self {
        if let Ok(rt) = cudart::runtime() {
            let mut p: *mut c_void = ptr::null_mut();
            let ok = unsafe {
                rt.cuda_host_alloc()
                    .map(|f| f(&mut p, len, 0))
                    .is_ok_and(|st| st.0 == 0 && !p.is_null())
            };
            if ok {
                return HostBuf::Pinned {
                    ptr: p as *mut u8,
                    len,
                };
            }
        }
        HostBuf::Heap(vec![0u8; len])
    }

    fn as_mut_ptr(&mut self) -> *mut u8 {
        match self {
            HostBuf::Pinned { ptr, .. } => *ptr,
            HostBuf::Heap(v) => v.as_mut_ptr(),
        }
    }

    fn as_slice(&self) -> &[u8] {
        match self {
            HostBuf::Pinned { ptr, len } => unsafe { std::slice::from_raw_parts(*ptr, *len) },
            HostBuf::Heap(v) => v.as_slice(),
        }
    }
}

impl Drop for HostBuf {
    fn drop(&mut self) {
        if let HostBuf::Pinned { ptr, .. } = self
            && let Ok(rt) = cudart::runtime()
            && let Ok(f) = rt.cuda_free_host()
        {
            unsafe {
                let _ = f(*ptr as *mut c_void);
            }
        }
    }
}

/// Device memory holding one decoded image.
///
/// Allocated with the stream-ordered allocator (`cudaMallocAsync`) when the
/// driver supports it — allocation and free are enqueued on the stream
/// instead of taking the driver lock — with plain `cudaMalloc`/`cudaFree`
/// as the fallback. The buffer must outlive the encode that reads it, which
/// the caller guarantees by keeping it alive across `nvjpegEncodeYUV`.
struct DeviceBuf {
    ptr: *mut u8,
    stream: cudaStream_t,
    async_alloc: bool,
}

impl Drop for DeviceBuf {
    fn drop(&mut self) {
        if self.ptr.is_null() {
            return;
        }
        let ptr = self.ptr as *mut c_void;
        unsafe {
            if self.async_alloc {
                if let Ok(rt) = cudart::runtime()
                    && let Ok(f) = rt.cuda_free_async()
                {
                    let _ = f(ptr, self.stream);
                }
            } else if let Ok(rt) = cudart::runtime()
                && let Ok(f) = rt.cuda_free()
            {
                let _ = f(ptr);
            }
        }
    }
}

/// One image queued for the GPU consumer thread. `input` is owned so the
/// consumer never borrows from a rayon worker's stack.
struct Job {
    input: JobInput,
    quality: u8,
    reply: Sender<Result<Vec<u8>, TranscodeError>>,
}

enum JobInput {
    /// JPEG stream bytes (a `/DCTDecode` image stream).
    Jpeg(Vec<u8>),
    /// Raw interleaved RGB pixels of a non-JPEG stream.
    Rgb {
        width: u32,
        height: u32,
        bytes: Vec<u8>,
    },
}

/// Create a dedicated non-blocking CUDA stream; `null` (the synchronous
/// default stream) when the CUDA runtime cannot be loaded.
fn create_stream() -> cudaStream_t {
    if let Ok(rt) = cudart::runtime() {
        let mut stream: cudaStream_t = ptr::null_mut();
        let ok = unsafe {
            rt.cuda_stream_create_with_flags()
                .map(|f| f(&mut stream, cudaStreamFlags::NON_BLOCKING))
                .is_ok_and(|st| st.0 == 0)
        };
        if ok {
            return stream;
        }
        // Older runtimes: a plain stream is still fine — we never use the
        // legacy default stream, so there is nothing to desynchronize from.
        let ok = unsafe {
            rt.cuda_stream_create()
                .map(|f| f(&mut stream))
                .is_ok_and(|st| st.0 == 0)
        };
        if ok {
            return stream;
        }
    }
    ptr::null_mut()
}

/// Wait for everything enqueued on the stream (no-op for the null stream,
/// whose calls are synchronous).
unsafe fn sync_stream(stream: cudaStream_t) -> Result<(), TranscodeError> {
    if stream.is_null() {
        return Ok(());
    }
    let pfn = cudart::runtime()
        .map_err(|e| TranscodeError::Gpu(format!("cuda runtime: {e}")))?
        .cuda_stream_synchronize()
        .map_err(|e| TranscodeError::Gpu(format!("cudaStreamSynchronize: {e}")))?;
    check_cuda(unsafe { pfn(stream) })
}

/// True when the stream-ordered allocator (`cudaMallocAsync`) works on this
/// driver, so device buffers can be allocated and freed on the stream.
unsafe fn stream_ordered_alloc_ok(stream: cudaStream_t) -> bool {
    let Ok(rt) = cudart::runtime() else {
        return false;
    };
    let Ok(alloc) = rt.cuda_malloc_async() else {
        return false;
    };
    let Ok(free) = rt.cuda_free_async() else {
        return false;
    };
    let mut p: *mut c_void = ptr::null_mut();
    if unsafe { alloc(&mut p, 1, stream) }.0 != 0 || p.is_null() {
        return false;
    }
    let ok = unsafe { free(p, stream) }.0 == 0;
    let _ = rt.cuda_stream_synchronize().map(|s| unsafe { s(stream) });
    ok
}

/// YUV plane geometry: (Y pitch, chroma pitch, chroma height, total bytes)
/// for an image at the given chroma subsampling. Pitches are 64-byte
/// aligned for nvjpeg's device kernels.
fn yuv_geometry(width: usize, height: usize, subsampling: c_int) -> (usize, usize, usize, usize) {
    let (uw, uh) = match subsampling {
        CSS_422 => (width.div_ceil(2), height),
        CSS_420 => (width.div_ceil(2), height.div_ceil(2)),
        _ => (width, height),
    };
    let py = align_rows(width);
    let puv = align_rows(uw);
    (py, puv, uh, py * height + 2 * puv * uh)
}

fn css_enum(subsampling: c_int) -> nvjpeg::nvjpegChromaSubsampling_t {
    match subsampling {
        CSS_422 => nvjpeg::nvjpegChromaSubsampling_t::Css422,
        CSS_444 => nvjpeg::nvjpegChromaSubsampling_t::Css444,
        _ => nvjpeg::nvjpegChromaSubsampling_t::Css420,
    }
}

/// One YUV-device-path image in a batch: position in the job slice, image
/// dimensions, chroma subsampling, and whether the decode stage is NVDEC
/// hardware (baseline 4:2:0) instead of nvjpeg.
#[derive(Clone, Copy)]
struct YuvJob {
    idx: usize,
    w: u32,
    h: u32,
    sub: c_int,
    nvdec: bool,
}

/// Group YUV-eligible jobs into batches bounded by count and by total
/// decoded bytes (so a batch of huge photos cannot blow VRAM).
fn yuv_chunks(items: &[YuvJob]) -> Vec<&[YuvJob]> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut bytes = 0usize;
    for (k, &job) in items.iter().enumerate() {
        let size = yuv_geometry(job.w as usize, job.h as usize, job.sub).3;
        if k - start >= BATCH_MAX || (bytes > 0 && bytes + size > BATCH_BUDGET_BYTES) {
            out.push(&items[start..k]);
            start = k;
            bytes = 0;
        }
        bytes += size;
    }
    if start < items.len() {
        out.push(&items[start..]);
    }
    out
}

/// All nvjpeg/CUDA state, owned by the single GPU consumer thread. Raw
/// handles are never touched from more than one thread at a time.
struct GpuState {
    lib: &'static nvjpeg::Nvjpeg,
    /// GPU-hybrid handle (fallback: default): decode and every encode.
    handle: nvjpeg::nvjpegHandle_t,
    /// One decode state per batch slot (single-image decode, device output).
    decode_states: Vec<nvjpeg::nvjpegJpegState_t>,
    /// One encoder state + params per batch slot, plus one extra for the
    /// per-image fallback path. Independent states keep every in-flight
    /// bitstream valid.
    encoder_states: Vec<nvjpeg::nvjpegEncoderState_t>,
    encoder_params: Vec<nvjpeg::nvjpegEncoderParams_t>,
    /// One non-blocking CUDA stream per batch slot.
    streams: Vec<cudaStream_t>,
    async_alloc: bool,
    /// Optional NVDEC hardware-decode stage (baseline 4:2:0 JPEGs). None
    /// when libnvcuvid/PTX init failed — decode then stays on nvjpeg.
    nvdec: Option<Nvdec>,
    /// Reusable pinned output buffers (one per batch slot).
    pinned: Vec<HostBuf>,
    pinned_cap: Vec<usize>,
}

impl Drop for GpuState {
    fn drop(&mut self) {
        // Intentionally a no-op — do not "fix" this into a real destructor.
        //
        // nvjpeg's destroy functions (nvjpegDestroy, nvjpegJpegStateDestroy,
        // nvjpegEncoderStateDestroy, nvjpegEncoderParamsDestroy) are C++
        // underneath and can throw through the `extern "C"` boundary when the
        // driver is degraded (observed with nvjpegEncoderParamsDestroy after a
        // failed encode: the C++ `terminate` aborts the process and is
        // uncatchable from Rust). presse is a short-lived CLI, so every handle
        // and CUDA stream in this struct is deliberately left to the OS, which
        // reclaims the context and driver allocations at process exit.
        //
        // The Drops that *are* implemented (HostBuf, DeviceBuf) are plain C
        // runtime calls — cudaFreeHost / cudaFree(Async) — which return an
        // error code and cannot throw; only the nvjpeg C++ objects above are
        // never explicitly destroyed.
    }
}

impl GpuState {
    fn new() -> Result<Self, TranscodeError> {
        let lib = nvjpeg::nvjpeg()
            .map_err(|e| TranscodeError::Unavailable(format!("nvJPEG library: {e}")))?;

        unsafe {
            // GPU-hybrid backend via `nvjpegCreateEx` — the V1 `nvjpegCreate`
            // rejects this backend with `NVJPEG_STATUS_BAD_PARAMETER` on
            // current drivers (CUDA 13.x). Same-session A/B on an RTX 4080
            // SUPER shows no significant difference from the default backend
            // (photos20 0.285 vs 0.289 s, photos60 0.476 vs 0.465 s, mixed
            // 0.374 vs 0.366 s), so the documented device-output backend is
            // used; the default (`nvjpegCreateSimple`) is the fallback on
            // older nvJPEG builds.
            let mut handle = ptr::null_mut();
            let mut gpu_hybrid = false;
            if let Ok(pfn) = lib.nvjpeg_create_ex() {
                let st = pfn(
                    nvjpeg::nvjpegBackend_t::GpuHybrid,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    0,
                    &mut handle,
                );
                gpu_hybrid = st.0 == 0 && !handle.is_null();
                if !gpu_hybrid {
                    eprintln!(
                        "warning: nvjpegCreateEx(GPU-hybrid) failed (status {}); \
                         using the default backend",
                        st.0
                    );
                }
            }
            if !gpu_hybrid {
                let pfn = lib
                    .nvjpeg_create_simple()
                    .map_err(|e| TranscodeError::Unavailable(format!("nvjpegCreateSimple: {e}")))?;
                check(pfn(&mut handle))?;
            }

            let mut decode_states = Vec::with_capacity(BATCH_MAX);
            for _ in 0..BATCH_MAX {
                let mut st = ptr::null_mut();
                let pfn = lib.nvjpeg_jpeg_state_create().map_err(|e| {
                    TranscodeError::Unavailable(format!("nvjpegJpegStateCreate: {e}"))
                })?;
                check(pfn(handle, &mut st))?;
                decode_states.push(st);
            }

            let mut encoder_states = Vec::with_capacity(BATCH_MAX + 1);
            let mut encoder_params = Vec::with_capacity(BATCH_MAX + 1);
            for _ in 0..=BATCH_MAX {
                let mut es = ptr::null_mut();
                let pfn = lib.nvjpeg_encoder_state_create().map_err(|e| {
                    TranscodeError::Unavailable(format!("nvjpegEncoderStateCreate: {e}"))
                })?;
                check(pfn(handle, &mut es, ptr::null_mut()))?;
                let mut ep = ptr::null_mut();
                let pfn = lib.nvjpeg_encoder_params_create().map_err(|e| {
                    TranscodeError::Unavailable(format!("nvjpegEncoderParamsCreate: {e}"))
                })?;
                check(pfn(handle, &mut ep, ptr::null_mut()))?;
                encoder_states.push(es);
                encoder_params.push(ep);
            }

            let mut streams = Vec::with_capacity(BATCH_MAX);
            for _ in 0..BATCH_MAX {
                streams.push(create_stream());
            }
            let async_alloc = stream_ordered_alloc_ok(streams[0]);

            // NVDEC is a decode accelerator, never a correctness dependency:
            // when it cannot initialize (missing libnvcuvid, driver too old
            // for the embedded PTX, context errors) the backend keeps running
            // on the nvjpeg decode path. `PRESSE_NO_NVDEC=1` forces the
            // nvjpeg path for A/B benchmarking/debugging.
            let nvdec = if std::env::var_os("PRESSE_NO_NVDEC").is_some() {
                eprintln!("warning: PRESSE_NO_NVDEC=1 — decode stage stays on nvJPEG");
                None
            } else {
                match Nvdec::new() {
                    Ok(n) => Some(n),
                    Err(e) => {
                        eprintln!(
                            "warning: NVDEC hardware decode unavailable ({e}); \
                                   decoding baseline 4:2:0 via nvJPEG"
                        );
                        None
                    }
                }
            };

            Ok(Self {
                lib,
                handle,
                decode_states,
                encoder_states,
                encoder_params,
                streams,
                async_alloc,
                nvdec,
                pinned: Vec::new(),
                pinned_cap: Vec::new(),
            })
        }
    }

    /// Read the JPEG frame header (dimensions, component count, chroma
    /// subsampling). Cheap: it only parses the header, no decode work.
    unsafe fn image_info(&self, data: &[u8]) -> Result<(u32, u32, c_int, c_int), TranscodeError> {
        let mut n_components: c_int = 0;
        let mut subsampling: c_int = 0;
        let mut widths = [0i32; 3];
        let mut heights = [0i32; 3];
        let pfn = self
            .lib
            .nvjpeg_get_image_info()
            .map_err(|e| TranscodeError::Unavailable(format!("nvjpegGetImageInfo: {e}")))?;
        check(unsafe {
            pfn(
                self.handle,
                data.as_ptr(),
                data.len(),
                &mut n_components,
                &mut subsampling,
                widths.as_mut_ptr(),
                heights.as_mut_ptr(),
            )
        })?;
        Ok((
            widths[0] as u32,
            heights[0] as u32,
            subsampling,
            n_components,
        ))
    }

    /// Allocate a device buffer on the given stream: stream-ordered
    /// (`cudaMallocAsync`) when available, plain `cudaMalloc` otherwise.
    unsafe fn alloc_device(
        &self,
        len: usize,
        stream: cudaStream_t,
    ) -> Result<DeviceBuf, TranscodeError> {
        let mut p: *mut c_void = ptr::null_mut();
        if self.async_alloc {
            let pfn = cudart::runtime()
                .map_err(|e| TranscodeError::Gpu(format!("cuda runtime: {e}")))?
                .cuda_malloc_async()
                .map_err(|e| TranscodeError::Gpu(format!("cudaMallocAsync: {e}")))?;
            check_cuda(unsafe { pfn(&mut p, len, stream) })?;
        } else {
            let pfn = cudart::runtime()
                .map_err(|e| TranscodeError::Gpu(format!("cuda runtime: {e}")))?
                .cuda_malloc()
                .map_err(|e| TranscodeError::Gpu(format!("cudaMalloc: {e}")))?;
            check_cuda(unsafe { pfn(&mut p, len) })?;
        }
        if p.is_null() {
            return Err(TranscodeError::Gpu("cudaMalloc returned null".into()));
        }
        Ok(DeviceBuf {
            ptr: p as *mut u8,
            stream,
            async_alloc: self.async_alloc,
        })
    }

    /// Allocate a device buffer with plain `cudaMalloc`/`cudaFree`.
    ///
    /// The NVDEC conversion kernel is launched through the CUDA *driver* API
    /// (cuLaunchKernel with the embedded PTX), and its stores into
    /// stream-ordered pool memory (`cudaMallocAsync`) silently do not land on
    /// this driver — the buffer reads back all zeros, while runtime-launched
    /// kernels (nvjpeg) write the same pool memory fine. NVDEC decode targets
    /// therefore bypass the pool. (Measured cost of the plain alloc/free pair
    /// is ~50–100 µs per image.)
    unsafe fn alloc_device_plain(&self, len: usize) -> Result<DeviceBuf, TranscodeError> {
        let mut p: *mut c_void = ptr::null_mut();
        let pfn = cudart::runtime()
            .map_err(|e| TranscodeError::Gpu(format!("cuda runtime: {e}")))?
            .cuda_malloc()
            .map_err(|e| TranscodeError::Gpu(format!("cudaMalloc: {e}")))?;
        check_cuda(unsafe { pfn(&mut p, len) })?;
        if p.is_null() {
            return Err(TranscodeError::Gpu("cudaMalloc returned null".into()));
        }
        Ok(DeviceBuf {
            ptr: p as *mut u8,
            stream: ptr::null_mut(),
            async_alloc: false,
        })
    }

    /// Configure encoder params for one slot. Fresh params carry an invalid
    /// default subsampling state and make the encode calls return
    /// `NVJPEG_STATUS_INVALID_PARAMETER` (observed on CUDA 13.3), so the
    /// sampling factors must be set explicitly before every encode.
    unsafe fn set_encoder_params(
        &self,
        quality: u8,
        state_idx: usize,
    ) -> Result<(), TranscodeError> {
        let params = self.encoder_params[state_idx];
        let pfn = self.lib.nvjpeg_encoder_params_set_quality().map_err(|e| {
            TranscodeError::Unavailable(format!("nvjpegEncoderParamsSetQuality: {e}"))
        })?;
        check(unsafe { pfn(params, quality as c_int, ptr::null_mut()) })?;

        let pfn = self
            .lib
            .nvjpeg_encoder_params_set_sampling_factors()
            .map_err(|e| {
                TranscodeError::Unavailable(format!("nvjpegEncoderParamsSetSamplingFactors: {e}"))
            })?;
        check(unsafe {
            pfn(
                params,
                nvjpeg::nvjpegChromaSubsampling_t::Css420,
                ptr::null_mut(),
            )
        })?;

        let pfn = self
            .lib
            .nvjpeg_encoder_params_set_optimized_huffman()
            .map_err(|e| {
                TranscodeError::Unavailable(format!("nvjpegEncoderParamsSetOptimizedHuffman: {e}"))
            })?;
        check(unsafe { pfn(params, OPTIMIZED_HUFFMAN, ptr::null_mut()) })?;
        Ok(())
    }

    /// Enqueue a JPEG decode whose output lands directly in device memory
    /// (planar YUV, 64-byte row pitches) on slot `stream_idx`'s stream.
    unsafe fn enqueue_decode_to_device(
        &self,
        data: &[u8],
        width: usize,
        height: usize,
        subsampling: c_int,
        dev: &DeviceBuf,
        stream_idx: usize,
    ) -> Result<(), TranscodeError> {
        let (py, puv, uh, _) = yuv_geometry(width, height, subsampling);
        let base = dev.ptr;
        let mut image = nvjpeg::nvjpegImage_t {
            channel: unsafe {
                [
                    base,
                    base.add(py * height),
                    base.add(py * height + puv * uh),
                    ptr::null_mut(),
                ]
            },
            pitch: [py, puv, puv, 0],
        };
        let pfn = self
            .lib
            .nvjpeg_decode()
            .map_err(|e| TranscodeError::Unavailable(format!("nvjpegDecode: {e}")))?;
        check(unsafe {
            pfn(
                self.handle,
                self.decode_states[stream_idx],
                data.as_ptr(),
                data.len(),
                nvjpeg::nvjpegOutputFormat_t::Yuv,
                &mut image,
                self.streams[stream_idx],
            )
        })
    }

    /// Enqueue a JPEG encode that reads planar YUV straight from device
    /// memory into `encoder_states[state_idx]` on `stream_idx`'s stream.
    #[allow(clippy::too_many_arguments)] // raw-FFI wrapper; arguments are inherent
    unsafe fn enqueue_encode_yuv(
        &self,
        width: u32,
        height: u32,
        subsampling: c_int,
        dev: &DeviceBuf,
        quality: u8,
        state_idx: usize,
        stream_idx: usize,
    ) -> Result<(), TranscodeError> {
        let (w, h) = (width as usize, height as usize);
        let (py, puv, uh, _) = yuv_geometry(w, h, subsampling);
        let base = dev.ptr;
        let image = nvjpeg::nvjpegImage_t {
            channel: unsafe {
                [
                    base,
                    base.add(py * h),
                    base.add(py * h + puv * uh),
                    ptr::null_mut(),
                ]
            },
            pitch: [py, puv, puv, 0],
        };
        unsafe {
            self.set_encoder_params(quality, state_idx)?;
            let pfn = self
                .lib
                .nvjpeg_encode_yuv()
                .map_err(|e| TranscodeError::Unavailable(format!("nvjpegEncodeYUV: {e}")))?;
            check(pfn(
                self.handle,
                self.encoder_states[state_idx],
                self.encoder_params[state_idx],
                &image,
                css_enum(subsampling),
                width as c_int,
                height as c_int,
                self.streams[stream_idx],
            ))
        }
    }

    /// Enqueue an interleaved-RGB encode into `encoder_states[state_idx]`
    /// on `stream_idx`'s stream. `src` may be host memory (decoded pixels
    /// or a raw PDF stream); it must stay valid until the stream is
    /// synchronized.
    #[allow(clippy::too_many_arguments)] // raw-FFI wrapper; arguments are inherent
    unsafe fn enqueue_encode_rgb(
        &self,
        width: u32,
        height: u32,
        src: *mut u8,
        pitch: usize,
        quality: u8,
        state_idx: usize,
        stream_idx: usize,
    ) -> Result<(), TranscodeError> {
        let image = nvjpeg::nvjpegImage_t {
            channel: [src, ptr::null_mut(), ptr::null_mut(), ptr::null_mut()],
            pitch: [pitch, 0, 0, 0],
        };
        unsafe {
            self.set_encoder_params(quality, state_idx)?;
            let pfn = self
                .lib
                .nvjpeg_encode_image()
                .map_err(|e| TranscodeError::Unavailable(format!("nvjpegEncodeImage: {e}")))?;
            check(pfn(
                self.handle,
                self.encoder_states[state_idx],
                self.encoder_params[state_idx],
                &image,
                NVJPEG_INPUT_RGBI,
                width as c_int,
                height as c_int,
                self.streams[stream_idx],
            ))
        }
    }

    /// Enqueue a bitstream size query for one encoder state on its stream.
    unsafe fn enqueue_size_query(
        &self,
        state_idx: usize,
        stream_idx: usize,
        size: &mut usize,
    ) -> Result<(), TranscodeError> {
        let pfn = self.lib.nvjpeg_encode_retrieve_bitstream().map_err(|e| {
            TranscodeError::Unavailable(format!("nvjpegEncodeRetrieveBitstream: {e}"))
        })?;
        check(unsafe {
            pfn(
                self.handle,
                self.encoder_states[state_idx],
                ptr::null_mut(),
                size,
                self.streams[stream_idx],
            )
        })
    }

    /// Enqueue a bitstream copy from one encoder state into a host buffer
    /// on its stream (async; read only after the stream is synchronized).
    unsafe fn enqueue_bitstream_copy(
        &self,
        state_idx: usize,
        stream_idx: usize,
        dst: *mut u8,
        size: &mut usize,
    ) -> Result<(), TranscodeError> {
        let pfn = self.lib.nvjpeg_encode_retrieve_bitstream().map_err(|e| {
            TranscodeError::Unavailable(format!("nvjpegEncodeRetrieveBitstream: {e}"))
        })?;
        check(unsafe {
            pfn(
                self.handle,
                self.encoder_states[state_idx],
                dst,
                size,
                self.streams[stream_idx],
            )
        })
    }

    /// Reusable pinned output buffer for batch slot `idx`, grown as needed.
    fn pinned_buf(&mut self, idx: usize, needed: usize) -> &mut HostBuf {
        if self.pinned.len() <= idx {
            self.pinned.resize_with(idx + 1, || HostBuf::new(0));
            self.pinned_cap.resize(idx + 1, 0);
        }
        if self.pinned_cap[idx] < needed {
            self.pinned[idx] = HostBuf::new(needed);
            self.pinned_cap[idx] = needed;
        }
        &mut self.pinned[idx]
    }

    /// Route one job through the per-image path: host RGBI decode + RGB
    /// encode (odd subsamplings, raw RGB streams, anything the device path
    /// rejected). Uses slot 0's stream and the dedicated fallback encoder
    /// state, so it never collides with an in-flight batch.
    fn process_single(&mut self, jobs: &mut [Job], idx: usize) {
        const SINGLE: usize = BATCH_MAX; // dedicated encoder slot
        let result = unsafe { self.transcode_single(&jobs[idx].input, jobs[idx].quality, SINGLE) };
        let _ = jobs[idx].reply.send(result);
    }

    /// Per-image transcode: host RGBI decode + RGB encode.
    unsafe fn transcode_single(
        &mut self,
        input: &JobInput,
        quality: u8,
        state: usize,
    ) -> Result<Vec<u8>, TranscodeError> {
        match input {
            JobInput::Jpeg(bytes) => {
                // Grayscale should already be filtered upstream; never let a
                // 1-component JPEG through (a 3-component output would
                // corrupt a /DeviceGray stream).
                if jpeg_components(bytes) == Some(1) {
                    return Err(TranscodeError::Encode(
                        "nvJPEG has no single-channel input format; the CPU backend \
                         handles /DeviceGray streams"
                            .into(),
                    ));
                }
                let (w, h, _, _) = unsafe { self.image_info(bytes) }?;
                let mut buf = unsafe { self.decode_to_host(bytes, w as usize, h as usize) }?;
                unsafe {
                    self.enqueue_encode_rgb(
                        w,
                        h,
                        buf.as_mut_ptr(),
                        (w as usize) * 3,
                        quality,
                        state,
                        0,
                    )?
                };
                unsafe { self.retrieve_state(state) }
            }
            JobInput::Rgb {
                width,
                height,
                bytes,
            } => {
                unsafe {
                    self.enqueue_encode_rgb(
                        *width,
                        *height,
                        bytes.as_ptr() as *mut _,
                        (*width as usize) * 3,
                        quality,
                        state,
                        0,
                    )?
                };
                unsafe { self.retrieve_state(state) }
            }
        }
    }

    /// Enqueue a single-image decode to a pinned host buffer (interleaved
    /// RGB). Used for the per-image fallback path.
    unsafe fn decode_to_host(
        &self,
        data: &[u8],
        width: usize,
        height: usize,
    ) -> Result<HostBuf, TranscodeError> {
        let mut buf = HostBuf::new(width * height * 3);
        let mut image = nvjpeg::nvjpegImage_t {
            channel: [
                buf.as_mut_ptr(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            ],
            pitch: [width * 3, 0, 0, 0],
        };
        let pfn = self
            .lib
            .nvjpeg_decode()
            .map_err(|e| TranscodeError::Unavailable(format!("nvjpegDecode: {e}")))?;
        check(unsafe {
            pfn(
                self.handle,
                self.decode_states[0],
                data.as_ptr(),
                data.len(),
                nvjpeg::nvjpegOutputFormat_t::Rgbi,
                &mut image,
                self.streams[0],
            )
        })?;
        Ok(buf)
    }

    /// Query, sync, copy, sync for one encoder state on slot 0's stream
    /// (per-image path).
    unsafe fn retrieve_state(&mut self, state_idx: usize) -> Result<Vec<u8>, TranscodeError> {
        let mut size = 0usize;
        unsafe { self.enqueue_size_query(state_idx, 0, &mut size) }?;
        unsafe { sync_stream(self.streams[0]) }?;
        if size == 0 {
            return Err(TranscodeError::Encode(
                "nvjpeg produced an empty bitstream".into(),
            ));
        }
        let dst = self.pinned_buf(0, size).as_mut_ptr();
        let mut len = size;
        unsafe { self.enqueue_bitstream_copy(state_idx, 0, dst, &mut len) }?;
        unsafe { sync_stream(self.streams[0]) }?;
        let out = self.pinned_buf(0, len.min(size)).as_slice()[..len.min(size)].to_vec();
        Ok(out)
    }

    /// Process one batch: fan the images out across the per-slot streams,
    /// each decoding to device memory and re-encoding from it, then one
    /// query/copy round per slot. Falls back per-image on any failure so a
    /// single bad stream never drags the whole batch to the CPU path.
    fn process_chunk(&mut self, jobs: &mut [Job], chunk: &[YuvJob]) {
        if self.run_chunk(jobs, chunk).is_err() {
            let _ = unsafe { sync_stream(self.streams[0]) };
            for &job in chunk {
                self.process_single(jobs, job.idx);
            }
        }
    }

    fn run_chunk(&mut self, jobs: &mut [Job], chunk: &[YuvJob]) -> Result<(), TranscodeError> {
        let n = chunk.len();
        let quality = jobs[chunk[0].idx].quality;

        unsafe {
            // Device YUV buffers + the per-slot decode/encode chains. The
            // JPEG bytes stay on the host; the decode stage writes the
            // decoded YUV straight to device memory — NVDEC hardware for
            // baseline 4:2:0 (`job.nvdec`, with an NV12 → planar conversion
            // kernel), nvjpegDecode otherwise.
            let mut devs: Vec<DeviceBuf> = Vec::with_capacity(n);
            let mut sizes = vec![0usize; n];
            for (k, &job) in chunk.iter().enumerate() {
                let JobInput::Jpeg(bytes) = &jobs[job.idx].input else {
                    continue;
                };
                let (py, puv, _, len) = yuv_geometry(job.w as usize, job.h as usize, job.sub);
                // NVDEC decode targets use plain cudaMalloc: the driver-API
                // conversion kernel's stores do not land in async-pool memory
                // (see `alloc_device_plain`).
                let dev = if job.nvdec {
                    self.alloc_device_plain(len)?
                } else {
                    self.alloc_device(len, self.streams[k])?
                };
                if job.nvdec {
                    // NVDEC: hardware decode + NV12 → planar conversion on
                    // this slot's stream. The slot stream is synchronized
                    // inside before the decode surface is unmapped, so the
                    // engine can start the next decode while this slot's
                    // convert/encode work drains.
                    let stream = self.streams[k];
                    let nvdec = self
                        .nvdec
                        .as_mut()
                        .ok_or_else(|| TranscodeError::Gpu("NVDEC stage disappeared".into()))?;
                    nvdec.decode_to_planar(
                        bytes, job.w, job.h, dev.ptr, py as u32, puv as u32, stream,
                    )?;
                } else {
                    self.enqueue_decode_to_device(
                        bytes,
                        job.w as usize,
                        job.h as usize,
                        job.sub,
                        &dev,
                        k,
                    )?;
                }
                self.enqueue_encode_yuv(job.w, job.h, job.sub, &dev, quality, k, k)?;
                self.enqueue_size_query(k, k, &mut sizes[k])?;
                devs.push(dev);
            }
            let n = devs.len();
            if n == 0 {
                return Ok(());
            }

            // First sync: every slot's decode + encode + size query has
            // completed, so the sizes are valid.
            for k in 0..n {
                sync_stream(self.streams[k])?;
            }

            // Copy the bitstreams out (pinned pool), then one more sync.
            for (k, size) in sizes.iter_mut().enumerate() {
                let dst = self.pinned_buf(k, *size).as_mut_ptr();
                self.enqueue_bitstream_copy(k, k, dst, size)?;
            }
            for k in 0..n {
                sync_stream(self.streams[k])?;
            }

            for (k, &job) in chunk.iter().enumerate() {
                let out = self.pinned_buf(k, sizes[k]).as_slice()[..sizes[k]].to_vec();
                let _ = jobs[job.idx].reply.send(Ok(out));
            }
            // `devs` drop here: every stream has drained, so the device
            // buffers are free to return to the pool.
            Ok(())
        }
    }

    /// Classify the batch and dispatch: YUV-capable JPEGs (3 components,
    /// 4:2:0/4:2:2/4:4:4) through the device path — with NVDEC hardware
    /// decode for baseline 4:2:0 — and everything else through the per-image
    /// path.
    fn process(&mut self, jobs: &mut [Job]) {
        let nvdec_ok = self.nvdec.is_some();
        let mut yuv: Vec<YuvJob> = Vec::new();
        let mut single: Vec<usize> = Vec::new();
        for (i, job) in jobs.iter().enumerate() {
            match &job.input {
                JobInput::Jpeg(bytes) => match unsafe { self.image_info(bytes) } {
                    Ok((w, h, sub, ncomp))
                        if ncomp == 3 && matches!(sub, CSS_420 | CSS_422 | CSS_444) =>
                    {
                        yuv.push(YuvJob {
                            idx: i,
                            w,
                            h,
                            sub,
                            // NVDEC decodes baseline (SOF0) 4:2:0 only;
                            // progressive / 4:2:2 / 4:4:4 stay on nvjpeg.
                            nvdec: nvdec_ok && sub == CSS_420 && nvdec::jpeg_sof0(bytes),
                        })
                    }
                    _ => single.push(i),
                },
                JobInput::Rgb { .. } => single.push(i),
            }
        }
        for chunk in yuv_chunks(&yuv) {
            self.process_chunk(jobs, chunk);
        }
        for &i in &single {
            self.process_single(jobs, i);
        }
    }
}

/// NVIDIA nvJPEG transcoder: an mpsc queue into the GPU consumer thread.
/// `transcode_image` enqueues one job and blocks for its result, so the
/// rayon producers stay lock-free and the consumer batches work for the
/// GPU. The thread is detached: like the CUDA teardown, it is left to the
/// OS to reclaim at process exit.
#[derive(Debug)]
pub struct CudaTranscoder {
    tx: Option<Sender<Job>>,
    handle: Option<JoinHandle<()>>,
}

impl CudaTranscoder {
    /// Probe the library cheaply (dlopen only — no CUDA context yet), so
    /// `--acceleration auto` keeps its semantics, then start the consumer
    /// thread. The ~100 ms context init happens there, off the calling
    /// thread, overlapping the PDF parse.
    pub fn new() -> Result<Self, TranscodeError> {
        nvjpeg::nvjpeg()
            .map_err(|e| TranscodeError::Unavailable(format!("nvJPEG library: {e}")))?;
        let (tx, rx) = mpsc::channel();
        let handle = std::thread::Builder::new()
            .name("presse-gpu".into())
            .spawn(move || consumer_main(rx))
            .map_err(|e| TranscodeError::Unavailable(format!("spawn GPU consumer: {e}")))?;
        Ok(Self {
            tx: Some(tx),
            handle: Some(handle),
        })
    }
}

impl Drop for CudaTranscoder {
    fn drop(&mut self) {
        // Shut the consumer down and join it *before* the process exits.
        // The consumer thread is detached from the rayon producers, and a
        // detached thread whose CUDA calls are still in flight while the
        // process tears down segfaults (observed: 3/10 crashes on a
        // many-image document; the nvjpeg teardown no-op above does not
        // cover thread lifetime). Closing the channel makes the consumer
        // exit its recv loop; by the time `drop` runs, every queued job has
        // been answered, so the join waits only for the consumer to unwind
        // its (leaked-by-design) state. CUDA is never touched after this
        // returns.
        drop(self.tx.take()); // closes the job channel
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn consumer_main(rx: Receiver<Job>) {
    // The CUDA context + nvjpeg state are initialized lazily on the first
    // job. Documents that route nothing to the GPU (the common case at the
    // 1 MiB routing threshold) never touch the driver, and the ~100 ms
    // context init is paid once, with jobs already in hand. On failure,
    // every queued job is answered with the error and the caller's
    // FallbackTranscoder routes each stream to the CPU backend.
    let mut gpu: Option<GpuState> = None;
    while let Ok(first) = rx.recv() {
        if gpu.is_none() {
            match GpuState::new() {
                Ok(g) => gpu = Some(g),
                Err(e) => {
                    let _ = first.reply.send(Err(e.clone()));
                    while let Ok(job) = rx.recv() {
                        let _ = job.reply.send(Err(e.clone()));
                    }
                    return;
                }
            }
        }
        let gpu = gpu.as_mut().unwrap();
        let mut batch = Vec::with_capacity(BATCH_MAX);
        batch.push(first);
        while batch.len() < BATCH_MAX {
            match rx.recv_timeout(COALESCE_TIMEOUT) {
                Ok(job) => batch.push(job),
                Err(_) => break,
            }
        }
        gpu.process(&mut batch);
    }
}

impl ImageTranscoder for CudaTranscoder {
    fn transcode_image(&self, input: &Input, quality: u8) -> Result<Vec<u8>, TranscodeError> {
        // Route grayscale away before it reaches the GPU: nvJPEG has no
        // single-channel input format, and a 3-component JPEG written into
        // a /DeviceGray stream renders as washed-out garbage.
        if matches!(input, Input::Jpeg(b) if jpeg_components(b) == Some(1))
            || matches!(input, Input::Pixels(ImageRef::Luma8 { .. }))
        {
            return Err(TranscodeError::Encode(
                "nvJPEG has no single-channel input format; the CPU backend handles \
                 /DeviceGray streams"
                    .into(),
            ));
        }

        let job_input = match input {
            Input::Jpeg(bytes) => JobInput::Jpeg(bytes.to_vec()),
            Input::Pixels(ImageRef::Rgb8 {
                width,
                height,
                bytes,
            }) => JobInput::Rgb {
                width: *width,
                height: *height,
                bytes: bytes.to_vec(),
            },
            Input::Pixels(ImageRef::Luma8 { .. }) => unreachable!("guarded above"),
        };
        let (reply_tx, reply_rx) = mpsc::channel();
        let tx = self
            .tx
            .as_ref()
            .ok_or_else(|| TranscodeError::Unavailable("GPU consumer is gone".into()))?;
        tx.send(Job {
            input: job_input,
            quality,
            reply: reply_tx,
        })
        .map_err(|_| TranscodeError::Unavailable("GPU consumer is gone".into()))?;
        reply_rx
            .recv()
            .map_err(|_| TranscodeError::Unavailable("GPU consumer is gone".into()))?
    }
}
