
fn main() {
    // Find libtorrent-rasterbar via pkg-config
    let libtorrent = pkg_config::Config::new()
        .atleast_version("2.0")
        .probe("libtorrent-rasterbar")
        .expect("libtorrent-rasterbar >= 2.0 not found. Install with: sudo pacman -S libtorrent-rasterbar");

    // Build the cxx bridge
    let mut build = cxx_build::bridge("src/lib.rs");

    // Add libtorrent include paths
    for path in &libtorrent.include_paths {
        build.include(path);
    }

    // Add our C++ wrapper
    build
        .file("cpp/wrapper.cpp")
        .std("c++17")
        .flag_if_supported("-Wno-unused-parameter")
        .flag_if_supported("-Wno-missing-field-initializers");

    // Compile
    build.compile("libtorrent_wrapper");

    // Link instructions
    println!("cargo:rerun-if-changed=src/lib.rs");
    println!("cargo:rerun-if-changed=cpp/wrapper.cpp");
    println!("cargo:rerun-if-changed=cpp/wrapper.h");

    // Link libtorrent
    for lib in &libtorrent.libs {
        println!("cargo:rustc-link-lib={}", lib);
    }
    for path in &libtorrent.link_paths {
        println!("cargo:rustc-link-search=native={}", path.display());
    }
}
