// Unity build for UnRAR library
// This file includes all necessary cpp files in the correct order to handle
// UnRAR's complex include dependencies.

// Include the main header which sets up all types and includes
#include "rar.hpp"

// Now include all the implementation files in order
// Note: Some files are included by others, so we skip those

// Core utility files
#include "strlist.cpp"
#include "strfn.cpp"
#include "pathfn.cpp"
#include "smallfn.cpp"
#include "global.cpp"

// File operations
#include "file.cpp"
#include "filefn.cpp"
#include "filcreat.cpp"
#include "filestr.cpp"
#include "find.cpp"

// Archive handling
#include "archive.cpp"
#include "arcread.cpp"

// Encoding/compression
#include "unicode.cpp"
#include "rawread.cpp"
#include "encname.cpp"
#include "getbits.cpp"

// Crypto
#include "rijndael.cpp"
#include "sha1.cpp"
#include "sha256.cpp"
#include "blake2s.cpp"
#include "hash.cpp"
#include "crypt.cpp"
#include "secpassword.cpp"

// Decompression - coder must come before model!
#include "coder.cpp"
#include "model.cpp"
#include "suballoc.cpp"
#include "rarvm.cpp"
#include "unpack.cpp"
#include "rdwrfn.cpp"

// Recovery
#include "rs.cpp"
#include "recvol.cpp"

// Headers and extraction
#include "headers.cpp"
#include "extinfo.cpp"
#include "extract.cpp"
#include "volume.cpp"

// Other
#include "timefn.cpp"
#include "errhnd.cpp"
#include "options.cpp"
#include "match.cpp"
#include "scantree.cpp"
#include "qopen.cpp"
#include "cmddata.cpp"
#include "cmdmix.cpp"
#include "cmdfilter.cpp"
#include "crc.cpp"
#include "resource.cpp"
#include "log.cpp"
#include "list.cpp"
#include "system.cpp"
#include "threadpool.cpp"
#include "largepage.cpp"
#include "hardlinks.cpp"
#include "uowners.cpp"
#include "ulinks.cpp"

// Platform-specific (handled by conditionals in the headers)
#ifdef _WIN_ALL
#include "isnt.cpp"
#endif

// UI - using silent mode
#include "ui.cpp"
#include "uisilent.cpp"
#include "uicommon.cpp"

// DLL interface
#include "dll.cpp"
