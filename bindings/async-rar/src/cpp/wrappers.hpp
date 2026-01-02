#pragma once


#if (defined(__unix__) || defined(__linux__) || defined(__APPLE__)) && !defined(_UNIX)
#define _UNIX
#endif

#if defined(_WIN32) || defined(_WIN64)
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#endif

#include "dll.hpp"
#include <cstdint>
#include <cstring>

inline int RARProcessFileW_Wrapper(void* hArcData, int Operation, uint16_t *DestPath, uint16_t *DestName) {
    return RARProcessFileW(hArcData, Operation, reinterpret_cast<wchar_t*>(DestPath), reinterpret_cast<wchar_t*>(DestName));
}

inline void RARSetCallback_Wrapper(void* hArcData, void* Callback, long long UserData) {
    RARSetCallback(hArcData, reinterpret_cast<UNRARCALLBACK>(Callback), static_cast<LPARAM>(UserData));
}

struct RARHeaderDataEx_Wrapper {
  char         ArcName[1024];
  uint16_t     ArcNameW[1024];
  char         FileName[1024];
  uint16_t     FileNameW[1024];
  unsigned int Flags;
  unsigned int PackSize;
  unsigned int PackSizeHigh;
  unsigned int UnpSize;
  unsigned int UnpSizeHigh;
  unsigned int HostOS;
  unsigned int FileCRC;
  unsigned int FileTime;
  unsigned int UnpVer;
  unsigned int Method;
  unsigned int FileAttr;
  unsigned int MtimeLow;
  unsigned int MtimeHigh;
  unsigned int CtimeLow;
  unsigned int CtimeHigh;
  unsigned int AtimeLow;
  unsigned int AtimeHigh;
};

struct RAROpenArchiveDataEx_Wrapper {
  char*        ArcName;
  uint16_t*    ArcNameW;
  unsigned int OpenMode;
  unsigned int OpenResult;
  char*        CmtBuf;
  unsigned int CmtBufSize;
  unsigned int CmtSize;
  unsigned int CmtState;
  unsigned int Flags;
  unsigned int OpFlags;
};

inline void* RAROpenArchiveEx_Wrapper(RAROpenArchiveDataEx_Wrapper* OpenData) {
    RAROpenArchiveDataEx data = {};
    // Zero initialize is handled by {}
    data.ArcName = OpenData->ArcName;
    data.OpenMode = OpenData->OpenMode;
    data.CmtBuf = OpenData->CmtBuf;
    data.CmtBufSize = OpenData->CmtBufSize;
    
    // Convert uint16_t* -> wchar_t* logic if needed, for now just copying pointers if non-null
    // Note: Rust side passes null for ArcNameW typically unless we enable wide paths
    if (OpenData->ArcNameW) data.ArcNameW = reinterpret_cast<wchar_t*>(OpenData->ArcNameW);

    HANDLE h = RAROpenArchiveEx(&data);
    OpenData->OpenResult = data.OpenResult;
    // Copy back relevant OUT fields
    OpenData->CmtSize = data.CmtSize; // Example, if we cared
    OpenData->CmtState = data.CmtState;
    OpenData->Flags = data.Flags; // This might be an out field too? Documentation check required? 
    // Usually OpenArchive usage fills OpenResult.
    
    return h;
}

inline int RARReadHeaderEx_Wrapper(void* hArcData, RARHeaderDataEx_Wrapper* HeaderData) {
    RARHeaderDataEx data = {};
    int res = RARReadHeaderEx(hArcData, &data);
    
    // Copy output data to our wrapper
    if (res == 0) { // Success
        std::memcpy(HeaderData->ArcName, data.ArcName, sizeof(data.ArcName));
        std::memcpy(HeaderData->FileName, data.FileName, sizeof(data.FileName));
        std::memcpy(HeaderData->ArcNameW, data.ArcNameW, sizeof(data.ArcNameW));
        std::memcpy(HeaderData->FileNameW, data.FileNameW, sizeof(data.FileNameW));
        
        HeaderData->Flags = data.Flags;
        HeaderData->PackSize = data.PackSize;
        HeaderData->PackSizeHigh = data.PackSizeHigh;
        HeaderData->UnpSize = data.UnpSize;
        HeaderData->UnpSizeHigh = data.UnpSizeHigh;
        HeaderData->HostOS = data.HostOS;
        HeaderData->FileCRC = data.FileCRC;
        HeaderData->FileTime = data.FileTime;
        HeaderData->UnpVer = data.UnpVer;
        HeaderData->Method = data.Method;
        HeaderData->FileAttr = data.FileAttr;
        HeaderData->MtimeLow = data.MtimeLow;
        HeaderData->MtimeHigh = data.MtimeHigh;
        HeaderData->CtimeLow = data.CtimeLow;
        HeaderData->CtimeHigh = data.CtimeHigh;
        HeaderData->AtimeLow = data.AtimeLow;
        HeaderData->AtimeHigh = data.AtimeHigh;
    }
    
    return res;
}
