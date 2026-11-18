//! Builds the vendored libopus 1.5.2 (opus/, the unmodified v1.5.2 release tarball minus docs,
//! tests, autotools and DNN model data) with CMake and links it statically. Every target gets the
//! same library, so the desktop and the aarch64 Frame cannot differ in codec behaviour.

fn main() {
    let src = std::path::Path::new("opus");
    println!("cargo:rerun-if-changed=opus");
    println!("cargo:rerun-if-changed=build.rs");

    let mut cfg = cmake::Config::new(src);
    cfg.profile("Release")
        // opus 1.5.2 declares cmake_minimum_required(3.16), but CMake 4 also rejects the
        // older sub-projects it pulls in without this floor.
        .define("CMAKE_POLICY_VERSION_MINIMUM", "3.5")
        .define("BUILD_SHARED_LIBS", "OFF")
        .define("CMAKE_POSITION_INDEPENDENT_CODE", "ON")
        .define("OPUS_BUILD_PROGRAMS", "OFF")
        .define("OPUS_BUILD_TESTING", "OFF")
        .define("OPUS_INSTALL_PKG_CONFIG_MODULE", "OFF")
        .define("OPUS_INSTALL_CMAKE_CONFIG_MODULE", "OFF")
        .define("OPUS_DRED", "OFF")
        .define("OPUS_OSCE", "OFF")
        .define("OPUS_FIXED_POINT", "OFF");
    let dst = cfg.build();
    for sub in ["lib", "lib64"] {
        println!("cargo:rustc-link-search=native={}/{}", dst.display(), sub);
    }
    println!("cargo:rustc-link-lib=static=opus");
}
