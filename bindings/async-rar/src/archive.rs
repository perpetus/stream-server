//! Synchronous RAR archive operations
//!
//! This module provides safe wrappers around the raw FFI bindings.

use std::ffi::{CStr, CString};
use std::path::Path;
use std::ptr;

use crate::error::{RarError, Result};

use crate::ffi;
use autocxx;

// Callback function for UnRAR
extern "C" fn rar_callback(msg: u32, user_data: isize, p1: isize, p2: isize) -> i32 {
    if msg == ffi::UCM_PROCESSDATA {
        let callback = unsafe { &mut *(user_data as *mut &mut dyn FnMut(&[u8]) -> bool) };
        let size = p2 as usize;
        if size > 0 {
            let data = unsafe { std::slice::from_raw_parts(p1 as *const u8, size) };
            if !callback(data) {
                return -1; // Abort operation
            }
        }
    }
    1 // Continue
}

/// Mode for opening a RAR archive
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenMode {
    /// Open for listing entries only
    List,
    /// Open for extraction
    Extract,
    /// Open for listing including split volumes
    ListIncludingSplit,
}

impl OpenMode {
    fn to_raw(self) -> u32 {
        match self {
            OpenMode::List => ffi::RAR_OM_LIST,
            OpenMode::Extract => ffi::RAR_OM_EXTRACT,
            OpenMode::ListIncludingSplit => ffi::RAR_OM_LIST_INCSPLIT,
        }
    }
}

/// Information about an entry in a RAR archive
#[derive(Debug, Clone)]
pub struct RarEntry {
    /// File name (path inside archive)
    pub name: String,
    /// Uncompressed size in bytes
    pub size: u64,
    /// Compressed size in bytes
    pub compressed_size: u64,
    /// Whether this entry is a directory
    pub is_directory: bool,
    /// Whether this entry is encrypted
    pub is_encrypted: bool,
    /// CRC32 checksum
    pub crc: u32,
    /// File attributes
    pub attributes: u32,
    /// Modification time (Unix timestamp)
    pub mtime: u64,
}

/// A handle to an opened RAR archive (synchronous)
pub struct RarArchive {
    handle: *mut std::ffi::c_void,
    path: String,
}

// RAR operations are NOT thread-safe, but we can move between threads
unsafe impl Send for RarArchive {}

impl RarArchive {
    /// Open a RAR archive
    pub fn open(path: impl AsRef<Path>, mode: OpenMode) -> Result<Self> {
        let path_str = path.as_ref().to_str().ok_or(RarError::InvalidPath)?;
        let c_path = CString::new(path_str).map_err(|_| RarError::InvalidPath)?;

        // Create open archive data structure
        let mut open_data = unsafe { std::mem::zeroed::<ffi::RAROpenArchiveDataEx_Wrapper>() };
        
        // Set archive name
        open_data.ArcName = c_path.as_ptr() as *mut _;
        open_data.OpenMode = mode.to_raw();
        
        // Allocate comment buffer
        let mut comment_buf = vec![0u8; 65536];
        open_data.CmtBuf = comment_buf.as_mut_ptr() as *mut _;
        open_data.CmtBufSize = comment_buf.len() as u32;

        let handle = unsafe { ffi::RAROpenArchiveEx_Wrapper(&mut open_data) };

        if handle.is_null() || open_data.OpenResult != 0 {
            return Err(RarError::from_code(open_data.OpenResult as i32));
        }

        Ok(Self {
            handle: handle as *mut _,
            path: path_str.to_string(),
        })
    }

    /// Set password for encrypted archives
    pub fn set_password(&mut self, password: &str) -> Result<()> {
        let c_password = CString::new(password).map_err(|_| RarError::InvalidPath)?;
        unsafe {
            ffi::RARSetPassword(self.handle as *mut _, c_password.as_ptr() as *mut _);
        }
        Ok(())
    }

    /// List all entries in the archive
    pub fn list_entries(&mut self) -> Result<Vec<RarEntry>> {
        let mut entries = Vec::new();
        
        loop {
            let mut header = unsafe { std::mem::zeroed::<ffi::RARHeaderDataEx_Wrapper>() };
            
            let result = unsafe { ffi::RARReadHeaderEx_Wrapper(self.handle as *mut _, &mut header) };
            
            if result == autocxx::c_int(ffi::RAR_END_ARCHIVE) {
                break;
            }
            
            if result != autocxx::c_int(ffi::RAR_SUCCESS) {
                return Err(RarError::from_code(result.into()));
            }

            // Extract filename
            let name = unsafe {
                let ptr = header.FileName.as_ptr();
                CStr::from_ptr(ptr).to_string_lossy().into_owned()
            };

            let entry = RarEntry {
                name,
                size: ((header.UnpSizeHigh as u64) << 32) | (header.UnpSize as u64),
                compressed_size: ((header.PackSizeHigh as u64) << 32) | (header.PackSize as u64),
                is_directory: (header.Flags & 0x10) != 0, // RHDF_DIRECTORY
                is_encrypted: (header.Flags & 0x04) != 0, // RHDF_ENCRYPTED
                crc: header.FileCRC,
                attributes: header.FileAttr,
                mtime: header.MtimeLow as u64 | ((header.MtimeHigh as u64) << 32),
            };

            entries.push(entry);

            // Skip to next entry
            let skip_result = unsafe { ffi::RARProcessFile(self.handle as *mut _, autocxx::c_int(ffi::RAR_SKIP), ptr::null_mut(), ptr::null_mut()) };
            if skip_result != autocxx::c_int(ffi::RAR_SUCCESS) && skip_result != autocxx::c_int(ffi::RAR_END_ARCHIVE) {
                return Err(RarError::from_code(skip_result.into()));
            }
        }

        Ok(entries)
    }

    /// Extract all files to a destination directory
    pub fn extract_all(&mut self, dest: impl AsRef<Path>) -> Result<()> {
        let dest_str = dest.as_ref().to_str().ok_or(RarError::InvalidPath)?;
        let c_dest = CString::new(dest_str).map_err(|_| RarError::InvalidPath)?;

        loop {
            let mut header = unsafe { std::mem::zeroed::<ffi::RARHeaderDataEx_Wrapper>() };
            
            let result = unsafe { ffi::RARReadHeaderEx_Wrapper(self.handle as *mut _, &mut header) };
            
            if result == autocxx::c_int(ffi::RAR_END_ARCHIVE) {
                break;
            }
            
            if result != autocxx::c_int(ffi::RAR_SUCCESS) {
                return Err(RarError::from_code(result.into()));
            }

            let extract_result = unsafe {
                ffi::RARProcessFile(
                    self.handle as *mut _,
                    autocxx::c_int(ffi::RAR_EXTRACT),
                    c_dest.as_ptr() as *mut _,
                    ptr::null_mut(),
                )
            };

            if extract_result != autocxx::c_int(ffi::RAR_SUCCESS) {
                return Err(RarError::from_code(extract_result.into()));
            }
        }

        Ok(())
    }

    /// Get the path to the archive
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Stream an entry's data via callback
    pub fn stream_entry<F>(&mut self, entry_name: &str, mut callback: F) -> Result<()> 
    where F: FnMut(&[u8]) -> bool 
    {
        // Trait object to pass safe pointer
        let mut trait_obj: &mut dyn FnMut(&[u8]) -> bool = &mut callback;
        let user_data = &mut trait_obj as *mut _ as isize;

        unsafe {
            ffi::RARSetCallback_Wrapper(
                self.handle as *mut _,
                rar_callback as *const () as *mut autocxx::c_void, 
                autocxx::c_longlong(user_data as i64)
            );
        }

        loop {
            let mut header = unsafe { std::mem::zeroed::<ffi::RARHeaderDataEx_Wrapper>() };
            let result = unsafe { ffi::RARReadHeaderEx_Wrapper(self.handle as *mut _, &mut header) };
            
            if result == autocxx::c_int(ffi::RAR_END_ARCHIVE) {
                break;
            }
            
            if result != autocxx::c_int(ffi::RAR_SUCCESS) {
                return Err(RarError::from_code(result.into()));
            }

            let name = unsafe {
                let ptr = header.FileName.as_ptr();
                CStr::from_ptr(ptr).to_string_lossy()
            };

            if name == entry_name {
                // Found the entry, process it with RAR_TEST to trigger callbacks without extracting to file
                // Note: RAR_TEST + Callback receiving UCM_PROCESSDATA acts as extraction to memory
                let process_result = unsafe {
                    ffi::RARProcessFile(
                        self.handle as *mut _,
                        autocxx::c_int(ffi::RAR_TEST),
                        ptr::null_mut(),
                        ptr::null_mut()
                    )
                };

                if process_result != autocxx::c_int(ffi::RAR_SUCCESS) {
                     return Err(RarError::from_code(process_result.into()));
                }
                
                // We are done with this specific entry
                return Ok(());
            } else {
                // Skip other entries
                let skip_result = unsafe { 
                    ffi::RARProcessFile(
                        self.handle as *mut _,
                        autocxx::c_int(ffi::RAR_SKIP),
                        ptr::null_mut(),
                        ptr::null_mut()
                    )
                };
                if skip_result != autocxx::c_int(ffi::RAR_SUCCESS) {
                     return Err(RarError::from_code(skip_result.into()));
                }
            }
        }
        
        Err(RarError::Unknown(0)) // Entry not found
    }
}

impl Drop for RarArchive {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe {
                ffi::RARCloseArchive(self.handle as *mut _);
            }
        }
    }
}
