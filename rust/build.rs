//! Resolve the built console assets and add CUDA link paths when needed.
//!
//! transcribe-cpp-sys 0.4.0's link manifest omits the CUDA runtime
//! libraries its ggml-cuda objects need; link them for the `cuda` feature.
//! `VOICELESS_CUDA_LIB_DIRS` (colon-separated) adds their search paths.
//!
//! Without the `cuda` feature (`CARGO_FEATURE_CUDA` unset), CPU builds and
//! CI never look for CUDA. With it, the
//! final link gets `-lcudart -lcublas -lcublasLt -lcuda` plus the search
//! paths from `VOICELESS_CUDA_LIB_DIRS`. `flake.nix` (`cudaEngineEnv`)
//! sets that variable to the nixpkgs `cuda_cudart` `lib` and `lib/stubs`
//! directories (the stub provides `libcuda.so` for linking; the real
//! driver library only exists on the running machine) and the `libcublas`
//! `lib` directory. At runtime the driver is found through
//! `/run/opengl-driver/lib` (wrapper `LD_LIBRARY_PATH` suffix and
//! `autoAddDriverRunpath`). Outside Nix, point the variable at your CUDA
//! toolkit's library directories.

use std::env;
use std::path::PathBuf;

#[path = "src/desktop/macos/build.rs"]
mod macos;

fn embed_console() {
    println!("cargo:rerun-if-env-changed=VOICELESS_CONSOLE_DIST");
    let assets = env::var_os("VOICELESS_CONSOLE_DIST")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../frontend/dist"));
    let assets = std::path::absolute(assets).expect("resolve console asset directory");
    for name in ["index.html", "app.js", "style.css"] {
        let asset = assets.join(name);
        assert!(
            asset.is_file(),
            "missing console asset {}. Run `npm --prefix frontend ci` and `npm --prefix frontend run build` from the repository root.",
            asset.display()
        );
        println!("cargo:rerun-if-changed={}", asset.display());
    }
    println!(
        "cargo:rustc-env=VOICELESS_CONSOLE_DIST={}",
        assets.display()
    );
}

fn main() {
    embed_console();
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        macos::build();
    }

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
