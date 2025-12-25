use std::env;
use std::path::PathBuf;

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
        includes.push(p);
    }
    
    // 2. Find Windows SDK Includes
    let sdk_roots = vec![
        PathBuf::from(format!(r"{}\Windows Kits\10\Include", program_files_x86)),
        PathBuf::from(format!(r"{}\Windows Kits\10\Include", program_files)),
    ];

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
                includes.push(latest.join("ucrt"));
                includes.push(latest.join("shared"));
                includes.push(latest.join("um"));
                includes.push(latest.join("um"));
                break;
            }
        }
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
            let includes_sys = find_include_paths();
            let mut args = String::new();
            for inc in includes_sys {
                let p = inc.to_string_lossy();
                extra_includes.push(format!("-I{}", p));
                args.push_str(&format!("-I\"{}\" ", p));
            }
            if !args.is_empty() {
                println!("cargo:rustc-env=BINDGEN_EXTRA_CLANG_ARGS={}", args);
                // Also set env var for current process (autocxx build)
                unsafe {
                    env::set_var("BINDGEN_EXTRA_CLANG_ARGS", args);
                }
            }
        }
    }

    let root = PathBuf::from("vendor/7zip/CPP");
    
    if !root.exists() {
        panic!("7-Zip source not found at {:?}. Please provide it.", root);
    }

    // Include paths
    let includes = vec![
        root.clone(),
        root.join("7zip"),
        root.join("Common"),
        root.parent().unwrap().join("C"), // Add ../C
        PathBuf::from("src"), // Add src for cpp/wrappers.hpp
    ];

    // Core C++ files needed for 7z Format (Decoders + 7z handler)
    // This is a minimal set; we might need to add more as linker errors appear.
    let mut files = vec![
        // Common
        root.join("Common/IntToString.cpp"),

        root.join("Common/MyString.cpp"),
        root.join("Common/StringConvert.cpp"),
        root.join("Common/MyVector.cpp"),
        root.join("Common/Wildcard.cpp"),
        root.join("Common/C_FileIO.cpp"), 
        root.join("Common/NewHandler.cpp"),
        root.join("Common/StringToInt.cpp"),
        // root.join("Common/CRC.cpp"), // Likely redundant if we include C/7zCrc.c or 7zip/Common/CrcReg.cpp
        root.join("Windows/PropVariant.cpp"),


        root.join("Windows/PropVariantConv.cpp"),
        root.join("Windows/System.cpp"),
        root.join("Windows/TimeUtils.cpp"),
        root.join("Windows/FileIO.cpp"),
        root.join("Windows/FileDir.cpp"),
        root.join("Windows/FileFind.cpp"),
        root.join("Windows/FileName.cpp"),
        root.join("Windows/Synchronization.cpp"),
        root.join("Windows/ErrorMsg.cpp"),
        root.join("Windows/DLL.cpp"),
        
        // 7-Zip C files (Parent directory)
        root.parent().unwrap().join("C/7zCrc.c"),
        root.parent().unwrap().join("C/7zCrcOpt.c"),
        root.parent().unwrap().join("C/Alloc.c"),
        root.parent().unwrap().join("C/Threads.c"),
        root.parent().unwrap().join("C/7zAlloc.c"),
        root.parent().unwrap().join("C/7zStream.c"),
        root.parent().unwrap().join("C/CpuArch.c"),


        // 7zip Common
        root.join("7zip/Common/StreamUtils.cpp"),
        root.join("7zip/Common/FileStreams.cpp"),
        root.join("7zip/Common/CreateCoder.cpp"), // For codec instantiation
        root.join("7zip/Common/FilterCoder.cpp"),
        root.join("7zip/Common/LimitedStreams.cpp"),
        root.join("7zip/Common/LockedStream.cpp"),
        root.join("7zip/Common/MethodId.cpp"),
        root.join("7zip/Common/MethodProps.cpp"),
        root.join("7zip/Common/OffsetStream.cpp"),
        root.join("7zip/Common/ProgressUtils.cpp"),
        root.join("7zip/Common/PropId.cpp"),
        root.join("7zip/Common/StreamBinder.cpp"),
        root.join("7zip/Common/StreamObjects.cpp"),
        root.join("7zip/Common/UniqBlocks.cpp"),
        root.join("7zip/Common/VirtThread.cpp"), 
        
        // Archive Common
        // root.join("7zip/Archive/Common/ParseProperties.cpp"), // Not needed for extraction
        // root.join("7zip/Archive/Common/OutStreamWithCRC.cpp"), // Not needed for extraction
        root.join("7zip/Archive/Common/HandlerOut.cpp"), // Needed for SetCommonProperty
        root.join("7zip/Archive/Common/CoderMixer2.cpp"),



        // 7z Archive Support (Extraction only)
        root.join("7zip/Archive/7z/7zDecode.cpp"),
        root.join("7zip/Archive/7z/7zExtract.cpp"),
        root.join("7zip/Archive/7z/7zFolderInStream.cpp"),
        root.join("7zip/Archive/7z/7zHandler.cpp"),
        // root.join("7zip/Archive/7z/7zHandlerOut.cpp"), // Not needed for extraction
        root.join("7zip/Archive/7z/7zHeader.cpp"),
        root.join("7zip/Archive/7z/7zIn.cpp"),
        root.join("7zip/Archive/7z/7zRegister.cpp"),
        root.join("7zip/Archive/7z/7zSpecStream.cpp"),
        root.join("7zip/Archive/7z/7zProperties.cpp"),
        // root.join("7zip/Archive/7z/7zUpdate.cpp"), // Not needed for extraction

        // root.join("7zip/Archive/7z/7zOut.cpp"), // Not needed for extraction
        // root.join("7zip/Archive/7z/7zEncode.cpp"), // Not needed for extraction

        
        // Compress/Codecs (Needed for extraction)
        // This is tricky. 7z uses a dynamic codec registration. 
        // We might need to include specific codec files or link against a bigger library.
        // For now, let's include basic ones.
        // COPY, LZMA, LZMA2, BCJ, BCJ2, ARM, AES, etc.
         // NOTE: These are often in C directory outside CPP?
         // 7-Zip mixes C and C++.
    ];
    
    // We also need C files from C directory? 
    // Usually 7-Zip wraps C code.
    // The "unity build" approach of `7z.dll` project file is usually the reference.
    
    // For now, let's start with our wrappers and see what links.
    // files.push(PathBuf::from("src/cpp/stream.cpp")); // Moved to autocxx build
    files.push(PathBuf::from("src/cpp/guids.cpp"));

    let mut builder = cc::Build::new();

    builder
        .cpp(true)
        .std("c++17")
        .includes(&includes)
        .files(files)
        .define("_7ZIP_ST", None) // Single Threaded strictly (simplifies things)
        .define("Z7_EXTRACT_ONLY", None) // Exclude encoding functionality
        .define("_REENTRANT", None);


    if cfg!(target_os = "windows") {
        builder.define("ENV_MSVC", None);
        builder.flag_if_supported("/EHsc");
    } else {
        builder.define("ENV_UNIX", None);
        builder.define("_POSIX", None);
    }
    
    builder.compile("sevenz_cpp");

    if cfg!(target_os = "windows") {
        println!("cargo:rustc-link-lib=dylib=ole32");
        println!("cargo:rustc-link-lib=dylib=oleaut32");
        println!("cargo:rustc-link-lib=dylib=user32");
        println!("cargo:rustc-link-lib=dylib=uuid");
    }


    // Autocxx
    // Add explicitly found includes
    let extra_includes_refs: Vec<&str> = extra_includes.iter().map(|s| s.as_str()).collect();

    let mut b = autocxx_build::Builder::new("src/ffi.rs", &includes)
        .extra_clang_args(&extra_includes_refs)
        .build()
        .expect("autocxx failed");
        
    b.flag_if_supported("-std=c++17");
    
    if cfg!(target_os = "windows") {
        b.flag_if_supported("/EHsc");
    }

    b.file("src/cpp/stream.cpp") // Compile stream.cpp here to access rust/cxx.h
     .compile("sevenz_autocxx");

    println!("cargo:rerun-if-changed=src/ffi.rs");
    println!("cargo:rerun-if-changed=src/cpp/stream.cpp");
    println!("cargo:rerun-if-changed=src/cpp/wrappers.hpp");
}
