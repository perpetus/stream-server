fn main() {
    let (include_paths, using_vcpkg) = find_libtorrent();

    // Build the cxx bridge
    let mut build = cxx_build::bridge("src/lib.rs");

    // Add libtorrent include paths
    for path in include_paths {
        build.include(path);
    }

    // Add our C++ wrapper
    build
        .file("cpp/wrapper.cpp")
        .std("c++17")
        .flag_if_supported("-Wno-unused-parameter")
        .flag_if_supported("-Wno-missing-field-initializers");

    // Only set TORRENT_ABI_VERSION when using vcpkg-built libtorrent
    // System packages (e.g., Arch Linux) use their own ABI version and don't need this
    if using_vcpkg {
        build.define("TORRENT_ABI_VERSION", "3");
    }
    build.define("TORRENT_USE_OPENSSL", None);

    if cfg!(target_os = "windows") {
        build.flag_if_supported("/std:c++17");
        build.flag("/Zc:__cplusplus"); // Force correct C++ version
        build.flag_if_supported("/EHsc"); // Enable C++ exceptions

        // Static linking configuration
        build.define("TORRENT_LINKING_STATIC", None);
        build.define("BOOST_ASIO_STATIC_LINK", None);
        build.define("BOOST_ASIO_SEPARATE_COMPILATION", None);
    }

    // Compile
    build.compile("libtorrent_wrapper");

    println!("cargo:rerun-if-changed=src/lib.rs");
    println!("cargo:rerun-if-changed=cpp/wrapper.cpp");
    println!("cargo:rerun-if-changed=cpp/wrapper.h");
}

/// Returns (include_paths, using_vcpkg)
/// using_vcpkg is true when vcpkg was used to find the library
fn find_libtorrent() -> (Vec<std::path::PathBuf>, bool) {
    let mut errors = String::new();

    // Try pkg-config first (system packages like on Arch Linux)
    match pkg_config::Config::new()
        .atleast_version("2.0")
        .probe("libtorrent-rasterbar")
    {
        Ok(lib) => return (lib.include_paths, false), // NOT vcpkg
        Err(e) => {
            errors.push_str(&format!("pkg-config: {}\n", e));
        }
    }

    // Configure vcpkg for static linking on Windows
    if cfg!(target_os = "windows") {
        unsafe {
            std::env::set_var("VCPKGRS_DYNAMIC", "0");
            std::env::set_var("VCPKGRS_TRIPLET", "x64-windows-static-release");
        }
    }

    // Fallback to vcpkg
    match vcpkg::Config::new()
        .emit_includes(true)
        .find_package("libtorrent")
    {
        Ok(lib) => return (lib.include_paths, true), // IS vcpkg
        Err(e) => {
            errors.push_str(&format!("vcpkg (libtorrent): {}\n", e));
        }
    }

    // Try finding "libtorrent-rasterbar" package just in case
    match vcpkg::Config::new()
        .emit_includes(true)
        .find_package("libtorrent-rasterbar")
    {
        Ok(lib) => return (lib.include_paths, true), // IS vcpkg
        Err(e) => {
            errors.push_str(&format!("vcpkg (libtorrent-rasterbar): {}\n", e));
        }
    }

    panic!("Could not find libtorrent-rasterbar.\nErrors:\n{}", errors);
}
