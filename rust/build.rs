//! transcribe-cpp-sys 0.4.0's link manifest omits the CUDA runtime
//! libraries its ggml-cuda objects need; link them for the `cuda` feature.
//! `VOICELESS_CUDA_LIB_DIRS` (colon-separated) adds their search paths.

fn main() {
    println!("cargo:rerun-if-env-changed=VOICELESS_CUDA_LIB_DIRS");
    if std::env::var_os("CARGO_FEATURE_CUDA").is_none() {
        return;
    }
    if let Some(dirs) = std::env::var_os("VOICELESS_CUDA_LIB_DIRS") {
        for dir in std::env::split_paths(&dirs) {
            println!("cargo:rustc-link-search=native={}", dir.display());
        }
    }
    for lib in ["cudart", "cublas", "cublasLt", "cuda"] {
        println!("cargo:rustc-link-lib=dylib={lib}");
    }
}
