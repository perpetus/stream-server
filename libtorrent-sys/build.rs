fn main() {
    let include_paths = find_libtorrent();

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

    if cfg!(target_os = "windows") {
        build.flag_if_supported("/std:c++17");
        build.flag("/Zc:__cplusplus"); // Force correct C++ version
        build.flag_if_supported("/EHsc"); // Enable C++ exceptions

        // Static linking configuration - library uses boost::string_view, NOT std::string_view
        build.define("TORRENT_LINKING_STATIC", None);
        build.define("TORRENT_ABI_VERSION", "3"); // Match v2 namespace in vcpkg library
        build.define("TORRENT_USE_OPENSSL", None);

        build.define("BOOST_ASIO_STATIC_LINK", None);
        build.define("BOOST_ASIO_SEPARATE_COMPILATION", None);
    }

    // Compile
    build.compile("libtorrent_wrapper");

    println!("cargo:rerun-if-changed=src/lib.rs");
    println!("cargo:rerun-if-changed=cpp/wrapper.cpp");
    println!("cargo:rerun-if-changed=cpp/wrapper.h");
}

fn find_libtorrent() -> Vec<std::path::PathBuf> {
    let mut errors = String::new();

    // Try pkg-config first
    match pkg_config::Config::new()
        .atleast_version("2.0")
        .probe("libtorrent-rasterbar")
    {
        Ok(lib) => return lib.include_paths,
        Err(e) => {
            errors.push_str(&format!("pkg-config: {}\n", e));
        }
    }

    // Configure vcpkg for static linking on Windows
    if cfg!(target_os = "windows") {
        unsafe {
            std::env::set_var("VCPKGRS_DYNAMIC", "0");
            std::env::set_var("VCPKGRS_TRIPLET", "x64-windows-static");
        }
    }

    // Fallback to vcpkg
    match vcpkg::Config::new()
        .emit_includes(true)
        .find_package("libtorrent")
    {
        Ok(lib) => return lib.include_paths,
        Err(e) => {
            errors.push_str(&format!("vcpkg (libtorrent): {}\n", e));
        }
    }

    // Try finding "libtorrent-rasterbar" package just in case
    match vcpkg::Config::new()
        .emit_includes(true)
        .find_package("libtorrent-rasterbar")
    {
        Ok(lib) => return lib.include_paths,
        Err(e) => {
            errors.push_str(&format!("vcpkg (libtorrent-rasterbar): {}\n", e));
        }
    }

    panic!("Could not find libtorrent-rasterbar.\nErrors:\n{}", errors);
}
