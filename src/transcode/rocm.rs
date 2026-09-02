//! AMD rocJPEG backend (`--acceleration rocm`, feature = "rocm").
//!
//! rocJPEG has no published Rust bindings, so this module hand-rolls the
//! minimal ABI against `librocjpeg.so` and loads it at runtime through
//! `libloading` — no ROCm SDK needed to build, no link-time dependency.
//! The decode path (create → get_image_info → decode → destroy) runs on the
//! GPU; encoding stays on the CPU backend until the rocJPEG encoder ABI is
//! validated. Every failure degrades to the CPU backend.
//!
//! **Validation status:** the ABI below follows the rocJPEG public header
//! but has **not been exercised on AMD hardware** in this project's CI. A
//! warning is printed when the backend initializes; verify against the ROCm
//! version you ship with (or use `-a cpu`) until it is hardware-validated.

use libloading::{Library, Symbol};
use std::ffi::{c_int, c_void};
use std::sync::Mutex;

use crate::transcode::{
    ImageRef, ImageTranscoder, Input, TranscodeError, encode_jpeg, jpeg_components,
};

type Status = c_int;

/// Opaque rocJPEG handle. Not thread-safe by itself; access is serialized
/// behind a mutex, so the pointer can be shared across rayon workers.
#[repr(transparent)]
#[derive(Debug, Clone, Copy)]
struct Handle(*mut c_void);

// SAFETY: the handle is only ever dereferenced inside the mutex-guarded
// transcode calls, mirroring how the C library expects to be used.
unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}

/// `rocjpegImage_t` — planar host output buffers.
#[repr(C)]
struct Image {
    channel: [*mut u8; 3],
    pitch: [usize; 3],
}

/// Resolved rocJPEG entry points.
#[derive(Debug)]
struct Symbols {
    create: unsafe extern "C" fn(*mut Handle) -> Status,
    destroy: unsafe extern "C" fn(Handle) -> Status,
    get_image_info: unsafe extern "C" fn(
        Handle,
        *const u8,
        usize,
        *mut c_int,
        *mut c_int,
        *mut c_int,
    ) -> Status,
    decode: unsafe extern "C" fn(Handle, *const u8, usize, *mut Image, *mut c_void) -> Status,
}

impl Symbols {
    /// Load every symbol; a single miss makes the backend unavailable.
    unsafe fn load(lib: &Library) -> Result<Self, TranscodeError> {
        unsafe fn sym<'a, T>(
            lib: &'a Library,
            name: &[u8],
        ) -> Result<Symbol<'a, T>, TranscodeError> {
            unsafe {
                lib.get(name).map_err(|e| {
                    TranscodeError::Unavailable(format!("rocjpeg symbol {name:?}: {e}"))
                })
            }
        }
        Ok(Symbols {
            create: *unsafe { sym(lib, b"rocjpegCreate") }?,
            destroy: *unsafe { sym(lib, b"rocjpegDestroy") }?,
            get_image_info: *unsafe { sym(lib, b"rocjpegGetImageInfo") }?,
            decode: *unsafe { sym(lib, b"rocjpegDecode") }?,
        })
    }
}

/// AMD rocJPEG transcoder (GPU decode, CPU encode).
#[derive(Debug)]
pub struct RocmTranscoder {
    _lib: Library, // keeps the library loaded for the lifetime of the backend
    symbols: Symbols,
    handle: Handle,
    _guard: Mutex<()>, // rocJPEG handles are not thread-safe
}

impl RocmTranscoder {
    /// Load `librocjpeg.so` and create a handle. Fails as
    /// [`TranscodeError::Unavailable`] when the library, driver, or any
    /// required symbol is missing; callers fall back to the CPU backend.
    pub fn new() -> Result<Self, TranscodeError> {
        let candidates: &[&str] = &["librocjpeg.so.1", "librocjpeg.so", "rocjpeg.dll"];
        // SAFETY: the library is loaded read-only for the backend's lifetime.
        let lib = candidates
            .iter()
            .find_map(|c| unsafe { Library::new(c) }.ok())
            .ok_or_else(|| {
                TranscodeError::Unavailable(format!(
                    "rocJPEG library not found (tried {candidates:?}); is ROCm installed?"
                ))
            })?;

        unsafe {
            let symbols = Symbols::load(&lib)?;
            let mut handle = Handle(std::ptr::null_mut());
            if (symbols.create)(&mut handle) != 0 || handle.0.is_null() {
                return Err(TranscodeError::Gpu("rocjpegCreate failed".into()));
            }
            eprintln!(
                "warning: experimental ROCm backend loaded — not validated on \
                 AMD hardware in CI; use `-a cpu` if you see any instability"
            );
            Ok(Self {
                _lib: lib,
                symbols,
                handle,
                _guard: Mutex::new(()),
            })
        }
    }

    /// Decode a JPEG stream to interleaved RGB host memory on the GPU.
    unsafe fn decode(&self, data: &[u8]) -> Result<(u32, u32, Vec<u8>), TranscodeError> {
        let mut components: c_int = 0;
        let mut width: c_int = 0;
        let mut height: c_int = 0;
        if unsafe {
            (self.symbols.get_image_info)(
                self.handle,
                data.as_ptr(),
                data.len(),
                &mut components,
                &mut width,
                &mut height,
            )
        } != 0
        {
            return Err(TranscodeError::Decode("rocjpegGetImageInfo failed".into()));
        }
        let (w, h) = (width as usize, height as usize);
        let mut planar = vec![0u8; w * h * 3];
        let mut image = Image {
            channel: [
                planar.as_mut_ptr(),
                unsafe { planar.as_mut_ptr().add(w * h) },
                unsafe { planar.as_mut_ptr().add(w * h * 2) },
            ],
            pitch: [w, w, w],
        };
        if unsafe {
            (self.symbols.decode)(
                self.handle,
                data.as_ptr(),
                data.len(),
                &mut image,
                std::ptr::null_mut(),
            )
        } != 0
        {
            return Err(TranscodeError::Decode("rocjpegDecode failed".into()));
        }
        // Planar RGB -> interleaved RGB for the encoder.
        let mut rgb = vec![0u8; w * h * 3];
        for i in 0..w * h {
            rgb[i * 3] = planar[i];
            rgb[i * 3 + 1] = planar[w * h + i];
            rgb[i * 3 + 2] = planar[w * h * 2 + i];
        }
        Ok((w as u32, h as u32, rgb))
    }
}

impl Drop for RocmTranscoder {
    fn drop(&mut self) {
        unsafe {
            let _ = (self.symbols.destroy)(self.handle);
        }
    }
}

impl ImageTranscoder for RocmTranscoder {
    fn transcode_image(&self, input: &Input, quality: u8) -> Result<Vec<u8>, TranscodeError> {
        let _guard = self._guard.lock().unwrap();
        unsafe {
            match input {
                // GPU decode, CPU encode (rocJPEG encoder ABI not yet validated).
                Input::Jpeg(bytes) => {
                    // Grayscale JPEGs have no GPU input format; keep them on
                    // the CPU path so /DeviceGray streams stay single-channel.
                    if jpeg_components(bytes) == Some(1) {
                        return Err(TranscodeError::Encode(
                            "rocJPEG has no single-channel input format; the CPU backend \
                             preserves /DeviceGray JPEGs"
                                .into(),
                        ));
                    }
                    let (w, h, rgb) = self.decode(bytes)?;
                    let img = image::RgbImage::from_raw(w, h, rgb)
                        .map(image::DynamicImage::ImageRgb8)
                        .ok_or_else(|| TranscodeError::Decode("bad RGB dimensions".into()))?;
                    let mut out = Vec::new();
                    encode_jpeg(&mut out, &img, quality);
                    Ok(out)
                }
                Input::Pixels(ImageRef::Luma8 {
                    width,
                    height,
                    bytes,
                }) => {
                    let img = image::GrayImage::from_raw(*width, *height, bytes.to_vec())
                        .map(image::DynamicImage::ImageLuma8)
                        .ok_or_else(|| TranscodeError::Decode("bad grayscale dimensions".into()))?;
                    let mut out = Vec::new();
                    encode_jpeg(&mut out, &img, quality);
                    Ok(out)
                }
                Input::Pixels(ImageRef::Rgb8 {
                    width,
                    height,
                    bytes,
                }) => {
                    let img = image::RgbImage::from_raw(*width, *height, bytes.to_vec())
                        .map(image::DynamicImage::ImageRgb8)
                        .ok_or_else(|| TranscodeError::Decode("bad RGB dimensions".into()))?;
                    let mut out = Vec::new();
                    encode_jpeg(&mut out, &img, quality);
                    Ok(out)
                }
            }
        }
    }
}
