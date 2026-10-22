//! transcribe-cpp-sys 0.4.0's link manifest omits the CUDA runtime
//! libraries its ggml-cuda objects need; link them for the `cuda` feature.
//! `VOICELESS_CUDA_LIB_DIRS` (colon-separated) adds their search paths.
//!
//! Without the `cuda` feature (`CARGO_FEATURE_CUDA` unset) this script
//! emits nothing, so CPU builds and CI never look for CUDA. With it, the
//! final link gets `-lcudart -lcublas -lcublasLt -lcuda` plus the search
//! paths from `VOICELESS_CUDA_LIB_DIRS`. `flake.nix` (`cudaEngineEnv`)
//! sets that variable to the nixpkgs `cuda_cudart` `lib` and `lib/stubs`
//! directories (the stub provides `libcuda.so` for linking; the real
//! driver library only exists on the running machine) and the `libcublas`
//! `lib` directory. At runtime the driver is found through
//! `/run/opengl-driver/lib` (wrapper `LD_LIBRARY_PATH` suffix and
//! `autoAddDriverRunpath`). Outside Nix, point the variable at your CUDA
//! toolkit's library directories.

fn main() {
    // Declared before the feature check so toggling the variable re-runs
    // this script even when the feature set did not change.
    println!("cargo:rerun-if-env-changed=VOICELESS_CUDA_LIB_DIRS");
    if std::env::var_os("CARGO_FEATURE_CUDA").is_none() {
        return;
    }
    if let Some(dirs) = std::env::var_os("VOICELESS_CUDA_LIB_DIRS") {
        for dir in std::env::split_paths(&dirs) {
            println!("cargo:rustc-link-search=native={}", dir.display());
        }
    }
    // `cuda` is the driver API (libcuda.so); the other three are the
    // runtime and BLAS libraries ggml-cuda calls.
    for lib in ["cudart", "cublas", "cublasLt", "cuda"] {
        println!("cargo:rustc-link-lib=dylib={lib}");
    }
}
