//! Called by rust/build.rs when CARGO_CFG_TARGET_OS is macos.
pub fn build() {
    let source = "src/desktop/macos/bridge.m";
    println!("cargo:rerun-if-changed={source}");
    cc::Build::new()
        .file(source)
        .flag("-fobjc-arc")
        .flag("-fblocks")
        .compile("voiceless_macos");
    for framework in [
        "AppKit",
        "AVFoundation",
        "ApplicationServices",
        "CoreGraphics",
        "CoreFoundation",
        "Foundation",
    ] {
        println!("cargo:rustc-link-lib=framework={framework}");
    }
    println!("cargo:rustc-link-lib=objc");
    let plist =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/desktop/macos/Info.plist");
    println!("cargo:rerun-if-changed={}", plist.display());
    println!(
        "cargo:rustc-link-arg-bin=voiceless=-Wl,-sectcreate,__TEXT,__info_plist,{}",
        plist.display()
    );
}
