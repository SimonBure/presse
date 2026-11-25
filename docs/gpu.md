# GPU acceleration (experimental)


`presse press` can offload JPEG re-encoding to a GPU with
`--acceleration cuda` (NVIDIA nvJPEG) or `--acceleration rocm` (AMD
rocJPEG). This is experimental, opt-in work — neither backend is linked
into the release binaries, neither runs in CI, and the CPU backend is the
default and always available. On a default build, `--acceleration cuda`
or `rocm` fails with an explicit "requires a build with the … feature"
error; build with the feature enabled to use them:

```bash
# NVIDIA
cargo install presse --features cuda --locked

# AMD
cargo install presse --features rocm --locked
```

`--locked` matters: the `baracuda` requirement is a caret range over
pre-releases, so without it Cargo may resolve a version the code has not
been tested against.

Runtime needs:

- **cuda** — an NVIDIA driver and the `nvjpeg` shared library. No CUDA
  toolkit is needed to *build*: the vendor library is loaded at runtime.
  Baseline 4:2:0 JPEGs are additionally decoded on the NVDEC hardware
  engine through the Video Codec SDK. The SDK's library (`libnvcuvid.so.1`)
  ships with the driver; its API declarations come from the SDK headers
  (`ffnvcodec-headers` on Debian/Ubuntu — the NVDEC stage is written
  against the cuvid API they declare), so install both. The NV12→planar
  conversion runs a tiny kernel that ships as embedded PTX, JIT-compiled
  by the driver. Progressive / 4:2:2 / 4:4:4 JPEGs keep the nvJPEG decode
  path. Validated on an RTX 4080 SUPER with CUDA 13.3 (verify with
  `cargo run --release --features cuda --example nvdec_verify`).
- **rocm** — compile-tested only; requires a ROCm installation with
  `rocjpeg` at runtime.

If the driver or library is missing at runtime, presse warns and falls
back to the CPU encoder per stream — a broken GPU can never drop or
corrupt a stream. The NVDEC stage is optional the same way: when
`libnvcuvid` or the PTX JIT is unavailable, decode stays on nvJPEG
(`PRESSE_NO_NVDEC=1` forces that path). Measured behavior (threshold
routing, speed vs size tradeoffs) is documented in
[`benches/docker/RESULTS.md`](../benches/docker/RESULTS.md).
