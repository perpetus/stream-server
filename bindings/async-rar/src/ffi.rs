//! FFI bindings for UnRAR library using autocxx
#![allow(dead_code)]
#![allow(unsafe_op_in_unsafe_fn)]
#![allow(unused_imports)]
//!
//! This module generates Rust bindings from the UnRAR C++ headers.

use autocxx::prelude::*;

include_cpp! {
    #include "wrappers.hpp"
    
    safety!(unsafe_ffi)
    
    // Archive open/close
    generate!("RAROpenArchiveEx_Wrapper")
    generate!("RARCloseArchive")
    
    // Header reading
    generate!("RARReadHeaderEx_Wrapper")
    
    // File processing  
    generate!("RARProcessFile")
    generate!("RARProcessFileW_Wrapper")
    
    // Password setting
    generate!("RARSetPassword")
    
    // Callback setting
    // generate!("RARSetCallback") // Failed to generate directly
    generate!("RARSetCallback_Wrapper")
    generate!("UNRARCALLBACK")
    
    // Data structures
    generate_pod!("RAROpenArchiveDataEx_Wrapper")
    generate_pod!("RARHeaderDataEx_Wrapper")
}

pub use ffi::*;

// Re-export constants that may not be generated
pub const RAR_OM_LIST: u32 = 0;
pub const RAR_OM_EXTRACT: u32 = 1;
pub const RAR_OM_LIST_INCSPLIT: u32 = 2;

pub const RAR_SKIP: i32 = 0;
pub const RAR_TEST: i32 = 1;
pub const RAR_EXTRACT: i32 = 2;

pub const RAR_SUCCESS: i32 = 0;
pub const RAR_END_ARCHIVE: i32 = 10;
pub const RAR_NO_MEMORY: i32 = 11;
pub const RAR_BAD_DATA: i32 = 12;
pub const RAR_BAD_ARCHIVE: i32 = 13;
pub const RAR_UNKNOWN_FORMAT: i32 = 14;
pub const RAR_EOPEN: i32 = 15;
pub const RAR_ECREATE: i32 = 16;
pub const RAR_ECLOSE: i32 = 17;
pub const RAR_EREAD: i32 = 18;
pub const RAR_EWRITE: i32 = 19;
pub const RAR_SMALL_BUF: i32 = 20;
pub const RAR_UNKNOWN: i32 = 21;
pub const RAR_MISSING_PASSWORD: i32 = 22;
pub const RAR_EREFERENCE: i32 = 23;
pub const RAR_BAD_PASSWORD: i32 = 24;

// Callback messages
pub const UCM_CHANGEVOLUME: u32 = 0;
pub const UCM_PROCESSDATA: u32 = 1;
pub const UCM_NEEDPASSWORD: u32 = 2;
pub const UCM_CHANGEVOLUMEW: u32 = 3;
pub const UCM_NEEDPASSWORDW: u32 = 4;
