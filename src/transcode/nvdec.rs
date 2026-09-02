//! NVIDIA NVDEC (Video Codec SDK) hardware MJPEG decode stage
//! (feature = "cuda", `--acceleration cuda`).
//!
//! nvJPEG's hardware decoder is unreachable on current drivers
//! (`nvjpegCreateEx(HARDWARE)` → status 7, `nvjpegDecodeBatchedInitialize` →
//! status 2 — see [`super::cuda`]), so the one remaining hardware-decode
//! route is the Video Codec SDK: `libnvcuvid.so.1` ships *with the driver*
//! (not the toolkit), which is why this module dlopens it directly through
//! `libloading` exactly like the nvJPEG backend does. The API it calls is
//! the cuvid API declared by the SDK headers (`ffnvcodec-headers` on
//! Debian/Ubuntu, `/usr/include/ffnvcodec/dynlink_nvcuvid.h`) — the
//! `CUVID*` structs below mirror those declarations field-for-field.
//!
//! # Design rationale
//!
//! - **Parser-free decode.** ffmpeg's `nvdec_mjpeg.c` does not use
//!   `cuvidCreateVideoParser` for MJPEG: the whole JPEG stream is handed to
//!   `cuvidDecodePicture` as a single slice with `intra_pic_flag=1`, and the
//!   decoder is created up front from the header dimensions. This module
//!   mirrors that exactly (the parser path was verified to decode, but adds
//!   a state machine for no benefit on intra-only frames).
//! - **The decoder-create parameters are ffmpeg's, verified field by field.**
//!   The original probe failed with status 100 (`CUDA_ERROR_NO_DEVICE`) for
//!   JPEG while H.264 succeeded; bisecting every field against ffmpeg's
//!   `CUVIDDECODECREATEINFO` pinned the failure on `ulNumOutputSurfaces = 0`
//!   (only legal for CUarray/opaque output). The working set: coded dims in
//!   `ulWidth/ulHeight` and `ulTargetWidth/ulTargetHeight`, `NV12` output,
//!   weave deinterlace, 2 decode surfaces, 1 output surface. Everything else
//!   stays zeroed, exactly as ffmpeg leaves it.
//! - **Even-aligned coded dims.** NVDEC's internal surfaces are even-sized
//!   (`(w + 1) & ~1`, as ffmpeg does); odd JPEG dims decode into a
//!   padded surface and the conversion kernel copies only the real `w × h`.
//! - **NV12 → planar YUV420 on device.** NVDEC outputs semi-planar NV12
//!   (256-byte-aligned pitch); the nvJPEG encoder
//!   ([`super::cuda::enqueue_encode_yuv`]) reads planar Y/U/V with 64-byte
//!   pitches. The de-interleave is a tiny CUDA kernel compiled to PTX once
//!   (via nvrtc during development) and **embedded here verbatim** — loaded
//!   with `cuModuleLoadData`, which JIT-compiles against the local driver.
//!   That keeps the runtime dependency surface identical to cuvid's (the
//!   driver alone), unlike NPP (`libnppicc` is toolkit-only).
//! - **One shared decoder per size, 2 surfaces, round-robin.** Every decode
//!   on this GPU serializes through the single hardware MJPEG engine anyway;
//!   a shared per-size decoder cache (LRU, 4 entries) keeps surface memory
//!   tiny (~2 × NV12 per distinct size, vs 16 per-slot decoders). Two
//!   surfaces let the engine decode the next picture while the previous
//!   picture's conversion kernel and nvJPEG encode execute on the SMs.
//! - **`cuvidMapVideoFrame` returns before the engine finishes.** The mapped
//!   surface can still be all-zero when the map returns; the conversion
//!   kernel must not read it yet. `cuCtxSynchronize` after the map is the
//!   reliable barrier (a `cudaStreamSynchronize` on the map's
//!   `output_stream` does *not* wait for the engine on this driver — the
//!   engine is not stream-ordered). Without it, batch pipelines intermittently
//!   produced all-black re-encodes while single-image runs looked fine.
//!   The slot stream is additionally synchronized after the conversion
//!   kernel and before `cuvidUnmapVideoFrame`, so a surface is never handed
//!   back to the engine while a kernel could still be reading it — enforced
//!   explicitly rather than trusted to driver unmap semantics.
//! - **Driver-API context.** cuvid requires a *driver* context; the runtime's
//!   implicit primary context alone made `cuvidCreateDecoder` fail with
//!   status 100 even for H.264. The init sequence
//!   (`cuInit` → `cuDeviceGet` → `cuDevicePrimaryCtxRetain` → `cuCtxSetCurrent`)
//!   is exactly the working probe, and it coexists with the CUDA runtime the
//!   rest of the backend uses (both operate on the same primary context).
//! - **Failure degrades.** Any NVDEC failure (missing `libnvcuvid`, old
//!   driver rejecting the PTX, decoder-create error) is reported to the
//!   caller, which falls back to the existing nvJPEG/CPU paths. NVDEC is a
//!   decode *accelerator*, never a correctness dependency.

use libloading::Library;
use std::ffi::{c_int, c_void};
use std::ptr;

use baracuda_cuda_sys::driver::driver;
use baracuda_cuda_sys::runtime::{self as cudart, cudaStream_t};

use crate::transcode::TranscodeError;

/// `cudaVideoCodec_JPEG`.
const CODEC_JPEG: i32 = 5;
/// `cudaVideoChromaFormat_420`.
const CHROMA_420: i32 = 1;
/// `cudaVideoSurfaceFormat_NV12`.
const SURFACE_NV12: i32 = 0;
/// `cudaVideoDeinterlaceMode_Weave`.
const DEINTERLACE_WEAVE: i32 = 0;

/// NV12 (semi-planar) → planar YUV 4:2:0, compiled by nvrtc to PTX (see the
/// module doc). Parameters, in order: `src`, `src_pitch`, `src_height`
/// (even-aligned NV12 coded height — the UV plane starts at
/// `src + src_height * src_pitch`), `dst_y`, `dst_u`, `dst_v`,
/// `dst_pitch_y`, `dst_pitch_uv`, `width`, `height`. Only the real `w × h`
/// pixels are written; the NV12 source may be padded (odd dims / pitch).
const NV12_TO_PLANAR_PTX: &str = r#"
.version 9.3
.target sm_75
.address_size 64

.visible .entry nv12_to_planar(
	.param .u64 nv12_to_planar_param_0,
	.param .u32 nv12_to_planar_param_1,
	.param .u32 nv12_to_planar_param_2,
	.param .u64 nv12_to_planar_param_3,
	.param .u64 nv12_to_planar_param_4,
	.param .u64 nv12_to_planar_param_5,
	.param .u32 nv12_to_planar_param_6,
	.param .u32 nv12_to_planar_param_7,
	.param .u32 nv12_to_planar_param_8,
	.param .u32 nv12_to_planar_param_9
)
{
	.reg .pred 	%p<4>;
	.reg .b16 	%rs<7>;
	.reg .b32 	%r<18>;
	.reg .b64 	%rd<29>;


	ld.param.u64 	%rd1, [nv12_to_planar_param_0];
	ld.param.u32 	%r3, [nv12_to_planar_param_1];
	ld.param.u32 	%r4, [nv12_to_planar_param_2];
	ld.param.u64 	%rd2, [nv12_to_planar_param_3];
	ld.param.u64 	%rd3, [nv12_to_planar_param_4];
	ld.param.u64 	%rd4, [nv12_to_planar_param_5];
	ld.param.u32 	%r5, [nv12_to_planar_param_6];
	ld.param.u32 	%r6, [nv12_to_planar_param_7];
	ld.param.u32 	%r7, [nv12_to_planar_param_8];
	ld.param.u32 	%r8, [nv12_to_planar_param_9];
	mov.u32 	%r9, %ntid.x;
	mov.u32 	%r10, %ctaid.x;
	mov.u32 	%r11, %tid.x;
	mad.lo.s32 	%r1, %r10, %r9, %r11;
	mov.u32 	%r12, %ntid.y;
	mov.u32 	%r13, %ctaid.y;
	mov.u32 	%r14, %tid.y;
	mad.lo.s32 	%r2, %r13, %r12, %r14;
	setp.ge.u32 	%p1, %r1, %r7;
	setp.ge.u32 	%p2, %r2, %r8;
	or.pred  	%p3, %p1, %p2;
	@%p3 bra 	$L__BB0_2;

	cvta.to.global.u64 	%rd5, %rd1;
	cvta.to.global.u64 	%rd6, %rd2;
	cvt.u64.u32 	%rd7, %r3;
	mul.wide.u32 	%rd8, %r3, %r2;
	cvt.u64.u32 	%rd9, %r1;
	add.s64 	%rd10, %rd8, %rd9;
	add.s64 	%rd11, %rd5, %rd10;
	ld.global.nc.u8 	%rs1, [%rd11];
	cvt.u16.u8 	%rs2, %rs1;
	mul.wide.u32 	%rd12, %r5, %r2;
	add.s64 	%rd13, %rd12, %rd9;
	add.s64 	%rd14, %rd6, %rd13;
	st.global.u8 	[%rd14], %rs2;
	shr.u32 	%r15, %r2, 1;
	cvt.u64.u32 	%rd15, %r15;
	cvt.u64.u32 	%rd16, %r4;
	add.s64 	%rd17, %rd16, %rd15;
	mul.lo.s64 	%rd18, %rd17, %rd7;
	and.b32  	%r16, %r1, -2;
	cvt.u64.u32 	%rd19, %r16;
	add.s64 	%rd20, %rd18, %rd19;
	add.s64 	%rd21, %rd5, %rd20;
	ld.global.nc.u8 	%rs3, [%rd21];
	cvt.u16.u8 	%rs4, %rs3;
	mul.wide.u32 	%rd22, %r6, %r15;
	shr.u32 	%r17, %r1, 1;
	cvt.u64.u32 	%rd23, %r17;
	add.s64 	%rd24, %rd22, %rd23;
	cvta.to.global.u64 	%rd25, %rd3;
	add.s64 	%rd26, %rd25, %rd24;
	ld.global.nc.u8 	%rs5, [%rd21+1];
	cvt.u16.u8 	%rs6, %rs5;
	cvta.to.global.u64 	%rd27, %rd4;
	add.s64 	%rd28, %rd27, %rd24;
	st.global.u8 	[%rd26], %rs4;
	st.global.u8 	[%rd28], %rs6;

$L__BB0_2:
	ret;

}
"#;

/// `CUVIDPICPARAMS` — 4280 bytes. Only the header fields through the slice
/// table and the intra/ref flags are used for JPEG; the trailing 1024-u32
/// `CodecSpecific` union must still occupy the full size (the driver reads
/// it as a fixed-size struct).
// Field names mirror the C header 1:1; keep the C spelling.
#[allow(clippy::upper_case_acronyms)]
#[repr(C)]
struct CUVIDPICPARAMS {
    pic_width_in_mbs: i32,
    frame_height_in_mbs: i32,
    curr_pic_idx: i32,
    field_pic_flag: i32,
    bottom_field_flag: i32,
    second_field: i32,
    n_bitstream_data_len: u32,
    _pad0: u32,
    p_bitstream_data: *const u8,
    n_num_slices: u32,
    _pad1: u32,
    p_slice_data_offsets: *const u32,
    ref_pic_flag: i32,
    intra_pic_flag: i32,
    timestamp: u64,
    reserved: [u32; 28],
    codec_specific: [u32; 1024],
}

/// `CUVIDDECODECREATEINFO` — 176 bytes, layout verified against
/// `dynlink_nvcuvid.h` with C `offsetof` (do not reorder fields).
// Field names mirror the C header 1:1; keep the C spelling.
#[allow(clippy::upper_case_acronyms)]
#[repr(C)]
struct CUVIDDECODECREATEINFO {
    ul_width: u64,
    ul_height: u64,
    ul_num_decode_surfaces: u64,
    codec_type: i32,
    chroma_format: i32,
    ul_creation_flags: u64,
    bit_depth_minus8: u64,
    ul_intra_decode_only: u64,
    ul_max_width: u64,
    ul_max_height: u64,
    reserved1: u64,
    display_area: [i16; 4],
    output_format: i32,
    deinterlace_mode: i32,
    ul_target_width: u64,
    ul_target_height: u64,
    ul_num_output_surfaces: u64,
    vid_lock: *mut c_void,
    target_rect: [i16; 4],
    enable_histogram: u64,
    enable_decode_features: u64,
    reserved2: [u64; 3],
}

/// `CUVIDPROCPARAMS` — 264 bytes.
// Field names mirror the C header 1:1; keep the C spelling.
#[allow(clippy::upper_case_acronyms)]
#[repr(C)]
struct CUVIDPROCPARAMS {
    progressive_frame: c_int,
    second_field: c_int,
    top_field_first: c_int,
    unpaired_field: c_int,
    reserved_flags: u32,
    reserved_zero: u32,
    raw_input_dptr: u64,
    raw_input_pitch: u32,
    raw_input_format: u32,
    raw_output_dptr: u64,
    raw_output_pitch: u32,
    reserved1: u32,
    output_stream: *mut c_void,
    reserved: [u32; 46],
    histogram_dptr: *mut u64,
    p_cuvid_proc_ext: *mut c_void,
}

/// Resolved cuvid entry points.
struct Cuvid {
    create_decoder: unsafe extern "C" fn(*mut *mut c_void, *mut CUVIDDECODECREATEINFO) -> c_int,
    destroy_decoder: unsafe extern "C" fn(*mut c_void) -> c_int,
    decode_picture: unsafe extern "C" fn(*mut c_void, *mut CUVIDPICPARAMS) -> c_int,
    map_frame:
        unsafe extern "C" fn(*mut c_void, c_int, *mut u64, *mut u32, *mut CUVIDPROCPARAMS) -> c_int,
    unmap_frame: unsafe extern "C" fn(*mut c_void, u64) -> c_int,
}

/// One cached NVDEC decoder, keyed by the even-aligned coded dimensions.
struct DecoderEntry {
    w: u32,
    h: u32,
    handle: *mut c_void,
    /// Round-robin surface index (`% 2`); 2 decode surfaces per decoder.
    next_surface: u32,
    last_used: u64,
}

/// Maximum distinct sizes kept in the decoder cache. Document pages are
/// usually one or two sizes; eviction just recreates a decoder (~µs).
const CACHE_MAX: usize = 4;

/// NVDEC decode + NV12→planar state, owned by the GPU consumer thread.
pub struct Nvdec {
    _lib: Library, // keeps libnvcuvid loaded for the backend's lifetime
    cuvid: Cuvid,
    module: *mut c_void,
    kernel: *mut c_void,
    decoders: Vec<DecoderEntry>,
    clock: u64,
}

impl Drop for Nvdec {
    fn drop(&mut self) {
        // Like the nvjpeg handles, cuvid decoders are left to the OS at
        // process exit (this module is only ever used by the short-lived
        // GPU consumer thread, which is joined before the process ends —
        // see CudaTranscoder::drop).
        let _ = self.module;
    }
}

/// Check a CUDA driver-API status, returning early with a [`TranscodeError`]
/// on failure. A macro because `CUresult` is not nameable here (the binding
/// keeps the type private; only its public `0` field is accessible).
macro_rules! check_drv {
    ($st:expr, $what:expr $(,)?) => {{
        let st = $st;
        if st.0 != 0 {
            return Err(gpu(format!("{}: driver status {}", $what, st.0)));
        }
    }};
}

fn gpu(msg: impl Into<String>) -> TranscodeError {
    TranscodeError::Gpu(msg.into())
}

/// Wait for the stream to drain (no-op for the null stream).
fn sync_stream(stream: cudaStream_t) -> Result<(), TranscodeError> {
    if stream.is_null() {
        return Ok(());
    }
    let pfn = cudart::runtime()
        .map_err(|e| gpu(format!("cuda runtime: {e}")))?
        .cuda_stream_synchronize()
        .map_err(|e| gpu(format!("cudaStreamSynchronize: {e}")))?;
    let st = unsafe { pfn(stream) };
    if st.0 == 0 {
        Ok(())
    } else {
        Err(gpu(format!("cudaStreamSynchronize: {}", st.0)))
    }
}

/// True when the JPEG stream starts with a baseline (SOF0) frame header —
/// the only MJPEG flavor NVDEC decodes. Progressive (SOF2) and extended
/// sequential (SOF1) JPEGs keep the nvJPEG path.
pub fn jpeg_sof0(data: &[u8]) -> bool {
    let mut i = 2; // skip SOI
    while i + 4 <= data.len() {
        if data[i] != 0xFF {
            i += 1;
            continue;
        }
        let marker = data[i + 1];
        match marker {
            0xD0..=0xD8 => i += 2,       // RSTn / SOI
            0xD9 | 0xDA => return false, // EOI / SOS: no frame seen
            0xC0 => return true,         // SOF0: baseline sequential
            0xC1..=0xCF if marker != 0xC4 && marker != 0xC8 && marker != 0xCC => return false,
            _ => i += 2 + u16::from_be_bytes([data[i + 2], data[i + 3]]) as usize,
        }
    }
    false
}

/// Build a minimal JPEG stream with the given SOF marker after SOI (plus an
/// optional segment before it, to exercise marker skipping).
#[cfg(test)]
fn fake_jpeg(sof: u8, prefix: &[u8]) -> Vec<u8> {
    let mut b = vec![0xFF, 0xD8]; // SOI
    b.extend_from_slice(prefix);
    b.extend_from_slice(&[0xFF, sof, 0x00, 0x11]); // SOFn, length 17
    b.extend_from_slice(&[8, 0x00, 0x40, 0x00, 0x40, 3]); // precision, 64x64, 3 comps
    b
}

impl Nvdec {
    /// Initialize the driver-API context, dlopen `libnvcuvid.so.1`, and load
    /// the embedded conversion kernel. Any failure makes the whole NVDEC
    /// stage unavailable; the caller falls back to nvJPEG/CPU decode.
    pub fn new() -> Result<Self, TranscodeError> {
        // SAFETY: read-only dlopen of the driver component; the library is
        // kept alive for the backend's lifetime via `_lib`.
        let lib = unsafe {
            Library::new("libnvcuvid.so.1")
                .map_err(|e| TranscodeError::Unavailable(format!("libnvcuvid.so.1: {e}")))?
        };
        unsafe fn sym<'a, T>(
            lib: &'a Library,
            name: &[u8],
        ) -> Result<libloading::Symbol<'a, T>, TranscodeError> {
            unsafe {
                lib.get(name)
                    .map_err(|e| TranscodeError::Unavailable(format!("cuvid symbol {name:?}: {e}")))
            }
        }
        let cuvid = Cuvid {
            create_decoder: *unsafe { sym(&lib, b"cuvidCreateDecoder") }?,
            destroy_decoder: *unsafe { sym(&lib, b"cuvidDestroyDecoder") }?,
            decode_picture: *unsafe { sym(&lib, b"cuvidDecodePicture") }?,
            map_frame: *unsafe { sym(&lib, b"cuvidMapVideoFrame") }?,
            unmap_frame: *unsafe { sym(&lib, b"cuvidUnmapVideoFrame") }?,
        };

        let drv = driver().map_err(|e| TranscodeError::Unavailable(format!("libcuda: {e}")))?;
        unsafe {
            // cuvid requires a driver-API context; the runtime's implicit
            // primary context alone makes decoder creation fail with status
            // 100 even for H.264. Both APIs share the same primary context,
            // so the runtime calls elsewhere in the backend keep working.
            check_drv!(
                drv.cu_init().map_err(|e| gpu(format!("cuInit: {e}")))?(0),
                "cuInit",
            );
            let mut dev = baracuda_cuda_sys::CUdevice(0);
            check_drv!(
                drv.cu_device_get()
                    .map_err(|e| gpu(format!("cuDeviceGet: {e}")))?(&mut dev, 0),
                "cuDeviceGet",
            );
            let mut ctx: *mut c_void = ptr::null_mut();
            check_drv!(
                drv.cu_device_primary_ctx_retain()
                    .map_err(|e| gpu(format!("cuDevicePrimaryCtxRetain: {e}")))?(
                    &mut ctx, dev
                ),
                "cuDevicePrimaryCtxRetain",
            );
            check_drv!(
                drv.cu_ctx_set_current()
                    .map_err(|e| gpu(format!("cuCtxSetCurrent: {e}")))?(ctx),
                "cuCtxSetCurrent",
            );

            // Embed the conversion kernel as PTX; cuModuleLoadData
            // JIT-compiles it for the local GPU using the driver's own
            // ptxjitcompiler (no toolkit component needed).
            let mut module: *mut c_void = ptr::null_mut();
            // cuModuleLoadData needs a NUL-terminated image; `&str` is not.
            let mut ptx = NV12_TO_PLANAR_PTX.as_bytes().to_vec();
            ptx.push(0);
            check_drv!(
                drv.cu_module_load_data()
                    .map_err(|e| gpu(format!("cuModuleLoadData: {e}")))?(
                    &mut module,
                    ptx.as_ptr() as *const c_void,
                ),
                "cuModuleLoadData",
            );
            let mut kernel: *mut c_void = ptr::null_mut();
            let name = c"nv12_to_planar".as_ptr();
            check_drv!(
                drv.cu_module_get_function()
                    .map_err(|e| gpu(format!("cuModuleGetFunction: {e}")))?(
                    &mut kernel,
                    module,
                    name,
                ),
                "cuModuleGetFunction",
            );

            Ok(Self {
                _lib: lib,
                cuvid,
                module,
                kernel,
                decoders: Vec::with_capacity(CACHE_MAX),
                clock: 0,
            })
        }
    }

    /// Decode a baseline 4:2:0 JPEG with NVDEC and convert the mapped NV12
    /// into the planar YUV 4:2:0 layout the nvJPEG encoder reads (`dst` is
    /// the base of the Y plane; the U/V planes follow at
    /// `dst_pitch_y * height` / `+ dst_pitch_uv * ceil(height/2)`).
    ///
    /// `stream` is the batch slot's CUDA stream: the conversion kernel runs
    /// on it, and it is synchronized before the decode surface is unmapped,
    /// so the surface is never reused while a kernel could still read it.
    ///
    /// # Safety
    ///
    /// `dst` must be a live device allocation large enough for the planar
    /// layout at the given pitches; `data` must stay valid for the call.
    #[allow(clippy::too_many_arguments)] // raw-FFI wrapper; arguments are inherent
    pub unsafe fn decode_to_planar(
        &mut self,
        data: &[u8],
        width: u32,
        height: u32,
        dst: *mut u8,
        dst_pitch_y: u32,
        dst_pitch_uv: u32,
        stream: cudaStream_t,
    ) -> Result<(), TranscodeError> {
        unsafe {
            // Even-aligned coded dims for the decoder and the NV12 surface
            // (ffmpeg: `(coded + 1) & ~1`).
            let ew = (width + 1) & !1;
            let eh = (height + 1) & !1;

            let entry = self.get_decoder(ew, eh)?;
            let handle = entry.handle;
            let idx = (entry.next_surface % 2) as i32;
            entry.next_surface += 1;

            let slice_offset: u32 = 0;
            let mut pic = CUVIDPICPARAMS {
                pic_width_in_mbs: (ew.div_ceil(16)) as i32,
                frame_height_in_mbs: (eh.div_ceil(16)) as i32,
                curr_pic_idx: idx,
                field_pic_flag: 0,
                bottom_field_flag: 0,
                second_field: 0,
                n_bitstream_data_len: data.len() as u32,
                _pad0: 0,
                p_bitstream_data: data.as_ptr(),
                n_num_slices: 1,
                _pad1: 0,
                p_slice_data_offsets: &slice_offset,
                ref_pic_flag: 0,
                intra_pic_flag: 1,
                timestamp: 0,
                reserved: [0; 28],
                codec_specific: [0; 1024],
            };
            let st = (self.cuvid.decode_picture)(handle, &mut pic);
            if st != 0 {
                return Err(gpu(format!("cuvidDecodePicture: {st}")));
            }

            // The map returns immediately (see the sync note below); the
            // decode was already submitted to the engine.
            let mut vpp = CUVIDPROCPARAMS {
                progressive_frame: 1,
                second_field: 0,
                top_field_first: 0,
                unpaired_field: 0,
                reserved_flags: 0,
                reserved_zero: 0,
                raw_input_dptr: 0,
                raw_input_pitch: 0,
                raw_input_format: 0,
                raw_output_dptr: 0,
                raw_output_pitch: 0,
                reserved1: 0,
                output_stream: stream,
                reserved: [0; 46],
                histogram_dptr: ptr::null_mut(),
                p_cuvid_proc_ext: ptr::null_mut(),
            };
            let mut dptr: u64 = 0;
            let mut pitch: u32 = 0;
            let st = (self.cuvid.map_frame)(handle, idx, &mut dptr, &mut pitch, &mut vpp);
            if st != 0 {
                return Err(gpu(format!("cuvidMapVideoFrame: {st}")));
            }
            // The map returns before the hardware engine has necessarily
            // finished this picture: without a device-wide sync the
            // conversion kernel can read the still-zeroed surface (observed
            // as all-black output in the batch pipeline, while single-image
            // runs that happened to let the engine drain stayed correct).
            // cuCtxSynchronize waits for the decode engine as well as CUDA
            // stream work, so the kernel below always reads decoded pixels.
            // (A plain cudaStreamSynchronize on the map's output_stream does
            // NOT wait for the engine on this driver — verified.)
            let drv = driver().map_err(|e| TranscodeError::Unavailable(format!("libcuda: {e}")))?;
            check_drv!(
                drv.cu_ctx_synchronize()
                    .map_err(|e| gpu(format!("cuCtxSynchronize: {e}")))?(),
                "cuCtxSynchronize",
            );

            let st = self.launch_convert(
                dptr,
                pitch,
                eh,
                dst,
                dst_pitch_y,
                dst_pitch_uv,
                width,
                height,
                stream,
            );
            if st.is_ok() {
                // The surface must not be handed back to the engine while
                // the conversion kernel could still read it: sync the slot
                // stream first (explicit ordering, not driver unmap
                // semantics).
                let _ = sync_stream(stream);
            }
            let st_unmap = (self.cuvid.unmap_frame)(handle, dptr);
            st?; // propagate a conversion-kernel failure after the unmap
            if st_unmap != 0 {
                return Err(gpu(format!("cuvidUnmapVideoFrame: {st_unmap}")));
            }
            Ok(())
        }
    }

    /// Get (or create) the decoder for the even-aligned size, evicting the
    /// least-recently-used entry when the cache is full.
    unsafe fn get_decoder(&mut self, w: u32, h: u32) -> Result<&mut DecoderEntry, TranscodeError> {
        unsafe {
            self.clock += 1;
            if let Some(pos) = self.decoders.iter().position(|d| d.w == w && d.h == h) {
                self.decoders[pos].last_used = self.clock;
                return Ok(&mut self.decoders[pos]);
            }
            if self.decoders.len() >= CACHE_MAX {
                let victim = self
                    .decoders
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, d)| d.last_used)
                    .map(|(i, _)| i)
                    .expect("cache non-empty");
                let dead = self.decoders.swap_remove(victim);
                let _ = (self.cuvid.destroy_decoder)(dead.handle);
            }
            let mut handle: *mut c_void = ptr::null_mut();
            let mut dci = CUVIDDECODECREATEINFO {
                ul_width: w as u64,
                ul_height: h as u64,
                ul_num_decode_surfaces: 2,
                codec_type: CODEC_JPEG,
                chroma_format: CHROMA_420,
                ul_creation_flags: 0,
                bit_depth_minus8: 0,
                ul_intra_decode_only: 0,
                ul_max_width: 0,
                ul_max_height: 0,
                reserved1: 0,
                display_area: [0; 4],
                output_format: SURFACE_NV12,
                deinterlace_mode: DEINTERLACE_WEAVE,
                ul_target_width: w as u64,
                ul_target_height: h as u64,
                // 0 here is only legal for opaque (CUarray) output and made
                // cuvidCreateDecoder fail with status 100 on every probe; 1
                // is what ffmpeg uses for mapped output.
                ul_num_output_surfaces: 1,
                vid_lock: ptr::null_mut(),
                target_rect: [0; 4],
                enable_histogram: 0,
                enable_decode_features: 0,
                reserved2: [0; 3],
            };
            let st = (self.cuvid.create_decoder)(&mut handle, &mut dci);
            if st != 0 || handle.is_null() {
                return Err(gpu(format!("cuvidCreateDecoder {w}x{h}: {st}")));
            }
            self.decoders.push(DecoderEntry {
                w,
                h,
                handle,
                next_surface: 0,
                last_used: self.clock,
            });
            Ok(self.decoders.last_mut().expect("just pushed"))
        }
    }

    /// Launch the NV12 → planar conversion kernel on `stream`.
    #[allow(clippy::too_many_arguments)] // raw-FFI wrapper; arguments are inherent
    unsafe fn launch_convert(
        &self,
        src: u64,
        src_pitch: u32,
        src_height: u32,
        dst: *mut u8,
        dst_pitch_y: u32,
        dst_pitch_uv: u32,
        width: u32,
        height: u32,
        stream: cudaStream_t,
    ) -> Result<(), TranscodeError> {
        unsafe {
            let drv = driver().map_err(|e| TranscodeError::Unavailable(format!("libcuda: {e}")))?;
            let dst_u = dst.add((dst_pitch_y * height) as usize);
            let dst_v =
                dst.add((dst_pitch_y * height + dst_pitch_uv * height.div_ceil(2)) as usize);
            let mut params: [*mut c_void; 10] = [
                (&src as *const u64) as *mut c_void,
                (&src_pitch as *const u32) as *mut c_void,
                (&src_height as *const u32) as *mut c_void,
                (&(dst as *const u8) as *const *const u8) as *mut c_void,
                (&(dst_u as *const u8) as *const *const u8) as *mut c_void,
                (&(dst_v as *const u8) as *const *const u8) as *mut c_void,
                (&dst_pitch_y as *const u32) as *mut c_void,
                (&dst_pitch_uv as *const u32) as *mut c_void,
                (&width as *const u32) as *mut c_void,
                (&height as *const u32) as *mut c_void,
            ];
            let (gx, gy) = (width.div_ceil(16), height.div_ceil(16));
            let st = drv
                .cu_launch_kernel()
                .map_err(|e| gpu(format!("cuLaunchKernel: {e}")))?(
                self.kernel,
                gx,
                gy,
                1,
                16,
                16,
                1,
                0,
                stream,
                params.as_mut_ptr(),
                ptr::null_mut(),
            );
            if st.0 != 0 {
                return Err(gpu(format!("cuLaunchKernel: {}", st.0)));
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sof0_baseline_detected() {
        assert!(jpeg_sof0(&fake_jpeg(0xC0, &[])));
        // APP0 before the frame header must be skipped.
        let app0 = [
            0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46, 0x00, 0x01, 0x01, 0x00, 0x00, 0x01,
            0x00, 0x01, 0x00, 0x00,
        ];
        assert!(jpeg_sof0(&fake_jpeg(0xC0, &app0)));
    }

    #[test]
    fn progressive_and_extended_not_baseline() {
        assert!(!jpeg_sof0(&fake_jpeg(0xC2, &[]))); // SOF2 progressive
        assert!(!jpeg_sof0(&fake_jpeg(0xC1, &[]))); // SOF1 extended sequential
    }

    #[test]
    fn non_jpeg_and_sos_yield_false() {
        assert!(!jpeg_sof0(b"not a jpeg"));
        assert!(!jpeg_sof0(&[0xFF, 0xD8, 0xFF, 0xDA, 0x00, 0x08])); // SOI then SOS
        assert!(!jpeg_sof0(&[]));
    }
}
