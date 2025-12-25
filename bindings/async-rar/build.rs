//! Build script for async-rar
//!
//! Compiles the UnRAR C++ library and generates autocxx bindings.
//! Supports Windows, Linux, and macOS.
//!
//! UnRAR has complex include dependencies between cpp files, so we use
//! a "unity build" approach with a single source file that includes
//! everything in the correct order.

use std::path::PathBuf;
use std::env;

fn find_llvm_path() -> Option<PathBuf> {
    if env::var("LIBCLANG_PATH").is_ok() {
        return None;
    }

    let program_files = env::var("ProgramFiles").unwrap_or_else(|_| r"C:\Program Files".to_string());
    let program_files_x86 = env::var("ProgramFiles(x86)").unwrap_or_else(|_| r"C:\Program Files (x86)".to_string());

    let search_paths = vec![
        format!(r"{}\LLVM\bin", program_files),
        format!(r"{}\LLVM\bin", program_files_x86),
        format!(r"{}\Microsoft Visual Studio\2022\Community\VC\Tools\Llvm\x64\bin", program_files),
        format!(r"{}\Microsoft Visual Studio\2022\Enterprise\VC\Tools\Llvm\x64\bin", program_files),
        format!(r"{}\Microsoft Visual Studio\2022\Professional\VC\Tools\Llvm\x64\bin", program_files),
        r"C:\ProgramData\chocolatey\bin".to_string(),
        r"C:\ProgramData\chocolatey\lib\llvm\tools\llvm\bin".to_string(),
    ];

    for path in search_paths {
        let p = PathBuf::from(path);
        if p.join("libclang.dll").exists() {
            return Some(p);
        }
    }
    
    // Check scoop
    if let Ok(profile) = env::var("USERPROFILE") {
        let p = PathBuf::from(profile).join(r"scoop\apps\llvm\current\bin");
        if p.join("libclang.dll").exists() {
            return Some(p);
        }
    }

    None
}

fn find_include_paths() -> Vec<PathBuf> {
    let mut includes = Vec::new();
    let program_files = env::var("ProgramFiles").unwrap_or_else(|_| r"C:\Program Files".to_string());
    let program_files_x86 = env::var("ProgramFiles(x86)").unwrap_or_else(|_| r"C:\Program Files (x86)".to_string());

    // 1. Find MSVC Includes
    let vs_editions = ["Enterprise", "Professional", "Community", "BuildTools"];
    let vs_years = ["2022", "2019", "2017"];
    let mut msvc_path = None;

    for year in vs_years {
        for edition in vs_editions {
            // Check both Program Files directories
            for root in &[&program_files, &program_files_x86] {
                let p = PathBuf::from(format!(r"{}\Microsoft Visual Studio\{}\{}\VC\Tools\MSVC", root, year, edition));
                if p.exists() {
                    // Find latest version
                    if let Ok(entries) = std::fs::read_dir(&p) {
                        if let Some(latest) = entries
                            .filter_map(Result::ok)
                            .filter(|e| e.path().is_dir())
                            .map(|e| e.path())
                            .max() 
                        {
                            let include = latest.join("include");
                            if include.exists() {
                                msvc_path = Some(include);
                                break;
                            }
                        }
                    }
                }
            }
            if msvc_path.is_some() { break; }
        }
        if msvc_path.is_some() { break; }
    }

    if let Some(p) = msvc_path {
        // println!("cargo:warning=Found MSVC includes at: {:?}", p);
        includes.push(p);
    } else {
        // println!("cargo:warning=Could not find MSVC includes (checked standard paths)");
    }
    
    // 2. Find Windows SDK Includes
    let sdk_roots = vec![
        PathBuf::from(format!(r"{}\Windows Kits\10\Include", program_files_x86)),
        PathBuf::from(format!(r"{}\Windows Kits\10\Include", program_files)),
    ];

    let mut found_sdk = false;
    for sdk_root in sdk_roots {
        if let Ok(entries) = std::fs::read_dir(&sdk_root) {
            // Find latest version
            if let Some(latest) = entries
                .filter_map(Result::ok)
                .filter(|e| e.path().is_dir())
                .map(|e| e.path())
                .filter(|p| p.file_name().unwrap().to_str().unwrap().starts_with("10."))
                .max()
            {
                // println!("cargo:warning=Found Windows SDK at: {:?}", latest);
                includes.push(latest.join("ucrt"));
                includes.push(latest.join("shared"));
                includes.push(latest.join("um"));
                found_sdk = true;
                break;
            }
        }
    }
    
    if !found_sdk {
        // println!("cargo:warning=Could not find Windows SDK includes");
    }
    
    includes
}

fn main() {
    let mut extra_includes = Vec::new();

    // Attempt to locate libclang on Windows if not set
    if cfg!(target_os = "windows") {
        if let Some(path) = find_llvm_path() {
            println!("cargo:rustc-env=LIBCLANG_PATH={}", path.display());
            unsafe {
                env::set_var("LIBCLANG_PATH", path);
            }
        }
        
        // Find and set include paths for autocxx/bindgen
        if env::var("INCLUDE").is_err() {
            let includes = find_include_paths();
            let mut args = String::new();
            for inc in includes {
                let p = inc.to_string_lossy();
                extra_includes.push(format!("-I{}", p));
                args.push_str(&format!("-I\"{}\" ", p));
            }
            if !args.is_empty() {
                // println!("cargo:warning=Setting BINDGEN_EXTRA_CLANG_ARGS: {}", args);
                println!("cargo:rustc-env=BINDGEN_EXTRA_CLANG_ARGS={}", args);
                // Also set env var for current process (autocxx build)
                unsafe {
                    env::set_var("BINDGEN_EXTRA_CLANG_ARGS", args);
                }
            } else {
                // println!("cargo:warning=No include paths found!");
            }
        } else {
            // println!("cargo:warning=INCLUDE env var is set: {:?}", env::var("INCLUDE"));
        }
    }

    let unrar_src = PathBuf::from("vendor/unrar");
    
    if !unrar_src.exists() {
        panic!(
            "UnRAR source not found at {:?}. Please download from https://www.rarlab.com/rar_add.htm",
            unrar_src
        );
    }

    // Collect all C++ source files
    // Note: UnRAR uses #include for some .cpp files, so we exclude those that are 
    // included by other files to avoid duplicate symbol errors.
    let mut cpp_files: Vec<PathBuf> = glob::glob("vendor/unrar/*.cpp")
        .expect("Failed to read glob pattern")
        .filter_map(Result::ok)
        .filter(|p| {
            let name = p.file_name().unwrap().to_str().unwrap();
            
            // Common exclusions
            if matches!(name,
                // GUI/Console related - not needed for library
                "arccmt.cpp" |
                "consio.cpp" |
                "uiconsole.cpp" |
                "rar.cpp" |
                // Files that are #included by other .cpp files
                "blake2s_sse.cpp" |     // included by blake2s.cpp
                "blake2sp.cpp" |        // included by blake2s.cpp
                "unpack15.cpp" |        // included by unpack.cpp
                "unpack20.cpp" |        // included by unpack.cpp
                "unpack30.cpp" |        // included by unpack.cpp
                "unpack50.cpp" |        // included by unpack.cpp
                "unpack50mt.cpp" |      // included by unpack.cpp
                "unpack50frag.cpp" |    // included by unpack.cpp
                "unpackinline.cpp" |    // included by unpack.cpp
                "coder.cpp" |           // included by unpack.cpp
                "model.cpp" |           // included by unpack.cpp
                "suballoc.cpp" |        // included by unpack.cpp
                "uicommon.cpp" |        // included by ui.cpp/uisilent.cpp
                "uisilent.cpp" |        // included by other ui
                // Dependencies of crypt.cpp/recvol.cpp
                "crypt1.cpp" |          // included by crypt.cpp
                "crypt2.cpp" |          // included by crypt.cpp
                "crypt3.cpp" |          // included by crypt.cpp
                "crypt5.cpp" |          // included by crypt.cpp
                "recvol3.cpp" |         // included by recvol.cpp
                "recvol5.cpp" |         // included by recvol.cpp
                "rs16.cpp" |            // included by recvol5.cpp
                "win32acl.cpp" |        // Windows-specific, included conditionally
                "win32stm.cpp" |        // Windows-specific, included conditionally
                "win32lnk.cpp"          // Windows-specific, included conditionally
            ) {
                return false;
            }

            // Platform-specific exclusions
            if cfg!(windows) {
                if matches!(name, "ulinks.cpp" | "uowners.cpp") {
                    return false;
                }
            }

            true
        })
        .collect();

    // Add stubs for missing symbols
    cpp_files.push(PathBuf::from("src/cpp/stubs.cpp"));

    // Build UnRAR as a C++ static library
    let mut cc_build = cc::Build::new();
    cc_build
        .cpp(true)
        .include(&unrar_src)
        .files(&cpp_files)
        .define("RARDLL", None)      // Build as DLL/library mode
        .define("SILENT", None);      // Disable console output

    // Platform-specific settings
    #[cfg(target_os = "windows")]
    {
        cc_build
            .define("_WIN_ALL", None)
            .flag("/std:c++17")
            .flag("/EHsc")  // Enable C++ exceptions
            // Force include rar.hpp - UnRAR uses precompiled headers
            .flag("/FIrar.hpp");
    }
    
    #[cfg(not(target_os = "windows"))]
    {
        cc_build
            .flag("-std=c++17")
            .flag("-fPIC")
            .flag("-pthread")
            // Force include rar.hpp - UnRAR uses precompiled headers
            .flag("-include")
            .flag("rar.hpp");
        
        #[cfg(target_os = "macos")]
        {
            cc_build.define("_APPLE", None);
        }
        
        #[cfg(target_os = "linux")]
        {
            cc_build.define("_UNIX", None);
        }
    }

    cc_build.compile("unrar");

    // Generate autocxx bindings
    let mut clang_args = vec!["-DRARDLL", "-DSILENT"];
    
    #[cfg(target_os = "windows")]
    clang_args.extend(&["-D_WIN_ALL", "-std=c++17"]);
    
    #[cfg(target_os = "macos")]
    clang_args.extend(&["-D_APPLE", "-std=c++17"]);
    
    #[cfg(target_os = "linux")]
    clang_args.extend(&["-D_UNIX", "-std=c++17"]);
    
    // Add explicitly found includes
    let extra_includes_refs: Vec<&str> = extra_includes.iter().map(|s| s.as_str()).collect();
    clang_args.extend(extra_includes_refs);

    let mut b = autocxx_build::Builder::new("src/ffi.rs", &[&unrar_src, &PathBuf::from("src/cpp")])
        .extra_clang_args(&clang_args)
        .build()
        .expect("Failed to generate autocxx bindings");

    #[cfg(target_os = "windows")]
    b.flag("/std:c++17");
    
    #[cfg(not(target_os = "windows"))]
    b.flag("-std=c++17");

    b.compile("async-rar-autocxx");

    // Rerun triggers
    println!("cargo:rerun-if-changed=src/ffi.rs");
    println!("cargo:rerun-if-changed=vendor/unrar");
}

