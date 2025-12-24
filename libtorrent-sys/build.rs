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
    // System packages (e.g., Arch Linux) usually don't need this specific ABI version 3
    if using_vcpkg {
        build.define("TORRENT_ABI_VERSION", "3");
        
        // Ensure static linking definitions are present for vcpkg builds
        // These are critical for both Windows and Linux to avoid linker errors
        build.define("TORRENT_LINKING_STATIC", None);
        build.define("BOOST_ASIO_STATIC_LINK", None);
        build.define("BOOST_ASIO_SEPARATE_COMPILATION", None);
    }
    
    build.define("TORRENT_USE_OPENSSL", None);

    if cfg!(target_os = "windows") {
        build.flag_if_supported("/std:c++17");
        build.flag("/Zc:__cplusplus"); // Force correct C++ version
        build.flag_if_supported("/EHsc"); // Enable C++ exceptions
    }

    // Compile
    build.compile("libtorrent_wrapper");

    println!("cargo:rerun-if-changed=src/lib.rs");
    println!("cargo:rerun-if-changed=cpp/wrapper.cpp");
    println!("cargo:rerun-if-changed=cpp/wrapper.h");
}

/// Returns (include_paths, using_vcpkg)
fn find_libtorrent() -> (Vec<std::path::PathBuf>, bool) {
    let mut errors = String::new();

    // Try pkg-config first
    match pkg_config::Config::new()
        .atleast_version("2.0")
        .probe("libtorrent-rasterbar")
    {
        Ok(lib) => {
            // Check if this is a vcpkg-installed library by looking at the path
            let is_vcpkg = lib.include_paths.iter().any(|p| {
                p.to_string_lossy().contains("vcpkg_installed")
            });
            return (lib.include_paths, is_vcpkg);
        }
        Err(e) => {
            errors.push_str(&format!("pkg-config: {}\n", e));
        }
    }

    // Configure vcpkg crate
    let triplet = if cfg!(target_os = "windows") {
        "x64-windows-static-release"
    } else {
        "x64-linux-release"
    };

    unsafe {
        std::env::set_var("VCPKGRS_DYNAMIC", "0");
        std::env::set_var("VCPKGRS_TRIPLET", triplet);
    }

    // Fallback to vcpkg crate
    let packages = ["libtorrent", "libtorrent-rasterbar"];
    for package in packages {
        match vcpkg::Config::new()
            .emit_includes(true)
            .find_package(package)
        {
            Ok(lib) => return (lib.include_paths, true),
            Err(e) => {
                errors.push_str(&format!("vcpkg ({}): {}\n", package, e));
            }
        }
    }

    // Last resort: Manual discovery if VCPKG_INSTALLED_DIR is set (common in CI)
    if let Ok(installed_dir) = std::env::var("VCPKG_INSTALLED_DIR") {
        let include_path = std::path::PathBuf::from(installed_dir)
            .join(triplet)
            .join("include");
        
        if include_path.exists() {
            // If we found it manually, we also need to tell cargo where the libs are
            let lib_path = include_path.parent().unwrap().join("lib");
            if lib_path.exists() {
                println!("cargo:rustc-link-search=native={}", lib_path.display());
                println!("cargo:rustc-link-lib=static=torrent-rasterbar");
                // On Windows, we also need to link with some system libs for boost/vcpkg
                if cfg!(target_os = "windows") {
                    println!("cargo:rustc-link-lib=iphlpapi");
                    println!("cargo:rustc-link-lib=dbghelp");
                }
                return (vec![include_path], true);
            }
        }
    }

    panic!("Could not find libtorrent.\nErrors:\n{}", errors);
}

