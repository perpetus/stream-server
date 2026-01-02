#include "wrappers.hpp"
#include "7zip/Archive/7z/7zHandler.h"
#include "7zip/Archive/IArchive.h"
#include <string>
#include <vector>
#include <cstring>
#include "Common/StringConvert.h"
#include "Windows/PropVariant.h"
#include "7zip/Archive/7z/7zItem.h"
#include "7zip/PropID.h"

extern "C" {
    int rust_read_cb(void* ptr, void* buf, unsigned int size, unsigned int* processed);
    int rust_seek_cb(void* ptr, long long offset, unsigned int origin, unsigned long long* new_pos);
    int rust_write_cb(void* ctx, const void* buf, unsigned int size, unsigned int* processed);
}

STDMETHODIMP RustInStream::Read(void *data, UInt32 size, UInt32 *processedSize) Z7_COM7F_E
{
    if (processedSize) *processedSize = 0;
    return rust_read_cb(rust_reader_ptr, data, size, processedSize) == 0 ? S_OK : E_FAIL;
}

STDMETHODIMP RustInStream::Seek(Int64 offset, UInt32 seekOrigin, UInt64 *newPosition) Z7_COM7F_E
{
    unsigned long long pos = 0;
    int res = rust_seek_cb(rust_reader_ptr, offset, seekOrigin, &pos);
    if (res != 0) return E_FAIL;
    if (newPosition) *newPosition = pos;
    return S_OK;
}

class RustOutStream : public ISequentialOutStream, public CMyUnknownImp {
    void* ctx;
public:
    RustOutStream(void* c) : ctx(c) {}
    Z7_COM_UNKNOWN_IMP_1(ISequentialOutStream)
    Z7_COM7F_IMF(Write(const void *data, UInt32 size, UInt32 *processedSize));
    virtual ~RustOutStream() {}
};

STDMETHODIMP RustOutStream::Write(const void *data, UInt32 size, UInt32 *processedSize) Z7_COM7F_E {
    return rust_write_cb(ctx, data, size, processedSize) == 0 ? S_OK : E_FAIL;
}

// RustExtractCallback implementation
STDMETHODIMP RustExtractCallback::SetTotal(UInt64 /*total*/) Z7_COM7F_E { return S_OK; }
STDMETHODIMP RustExtractCallback::SetCompleted(const UInt64 * /*completeValue*/) Z7_COM7F_E { return S_OK; }
STDMETHODIMP RustExtractCallback::GetStream(UInt32 index, ISequentialOutStream **outStream, Int32 askExtractMode) Z7_COM7F_E {
    *outStream = NULL;
    if (index != targetIndex || askExtractMode != NArchive::NExtract::NAskMode::kExtract) {
        return S_OK;
    }
    // Create output stream wrapping our Rust writer
    RustOutStream *stream = new RustOutStream(rust_callback_ptr);
    if (!stream) return E_OUTOFMEMORY;
    
    CMyComPtr<ISequentialOutStream> streamPtr = stream;
    *outStream = streamPtr.Detach();
    
    return S_OK;
}
STDMETHODIMP RustExtractCallback::PrepareOperation(Int32 /*askExtractMode*/) Z7_COM7F_E { return S_OK; }
STDMETHODIMP RustExtractCallback::SetOperationResult(Int32 /*resultEOperationResult*/) Z7_COM7F_E { return S_OK; }
STDMETHODIMP RustExtractCallback::CryptoGetTextPassword(BSTR * /*password*/) Z7_COM7F_E { return E_NOTIMPL; }


SevenZArchive* OpenArchive(void* rust_reader_ptr) {
    RustInStream* stream = new RustInStream(rust_reader_ptr);
    if (!stream) return nullptr;
    
    // NArchive::N7z::CHandler is the 7z implementation
    NArchive::N7z::CHandler *specHandler = new NArchive::N7z::CHandler;
    CMyComPtr<IInArchive> archive = specHandler;
    
    const UInt64 scanSize = 1 << 23; // default
    
    // Open
    // We casts stream to IInStream*
    if (archive->Open(stream, &scanSize, NULL) != S_OK) {
        // Failed
        return nullptr; 
    }
    
    SevenZArchive* wrapper = new SevenZArchive();
    wrapper->archive = archive;
    wrapper->fileStream = stream; // Keep reference
    return wrapper;
}

void CloseArchive(SevenZArchive* arch) {
    if (arch) {
        if (arch->archive) arch->archive->Close();
        delete arch;
    }
}

int ExtractEntry(SevenZArchive* arch, unsigned int index, void* rust_callback_ptr) {
    if (!arch || !arch->archive) return -1;
    
    const UInt32 indices[] = { index };
    
    RustExtractCallback* cb = new RustExtractCallback(rust_callback_ptr, index);
    CMyComPtr<IArchiveExtractCallback> cbPtr = cb;
    
    return arch->archive->Extract(indices, 1, 0, cb);
}

unsigned int GetArchiveItemCount(SevenZArchive* arch) {
    if (!arch || !arch->archive) return 0;
    UInt32 numItems = 0;
    arch->archive->GetNumberOfItems(&numItems);
    return numItems;
}

char* GetArchiveItemName(SevenZArchive* arch, unsigned int index) {
     if (!arch || !arch->archive) return nullptr;
    NWindows::NCOM::CPropVariant prop;
    if (arch->archive->GetProperty(index, kpidPath, &prop) != S_OK) return nullptr;
    if (prop.vt != VT_BSTR) return nullptr;
    
    // Convert BSTR (UTF-16) to UTF-8 without std::wstring
    if (!prop.bstrVal) return nullptr;
    
    AString utf8Name = UnicodeStringToMultiByte(prop.bstrVal, CP_UTF8);
    int size_needed = utf8Name.Len() + 1;
    
    char* str = (char*)malloc(size_needed);
    std::memcpy(str, (const char*)utf8Name, size_needed);
    return str;
}

void FreeString(char* str) {
    if (str) free(str);
}

int IsArchiveItemFolder(SevenZArchive* arch, unsigned int index) {
     if (!arch || !arch->archive) return 0;
    NWindows::NCOM::CPropVariant prop;
    if (arch->archive->GetProperty(index, kpidIsDir, &prop) != S_OK) return 0;
    if (prop.vt == VT_BOOL) return prop.boolVal ? 1 : 0;
    return 0;
}

unsigned long long GetArchiveItemSize(SevenZArchive* arch, unsigned int index) {
      if (!arch || !arch->archive) return 0;
    NWindows::NCOM::CPropVariant prop;
    if (arch->archive->GetProperty(index, kpidSize, &prop) != S_OK) return 0;
    // Size can be VT_UI8, VT_UI4, or VT_EMPTY
    if (prop.vt == VT_UI8) return prop.uhVal.QuadPart;
    if (prop.vt == VT_UI4) return prop.ulVal;
    return 0;
}

int GetArchiveItemIndex(SevenZArchive* arch, const char* name) {
    if (!arch || !arch->archive || !name) return -1;
    
    // Convert UTF-8 name to UTF-16
    UString target = MultiByteToUnicodeString(name, CP_UTF8);

    UInt32 count = 0;
    arch->archive->GetNumberOfItems(&count);

    for (UInt32 i = 0; i < count; i++) {
        NWindows::NCOM::CPropVariant prop;
        if (arch->archive->GetProperty(i, kpidPath, &prop) == S_OK) {
            if (prop.vt == VT_BSTR && prop.bstrVal) {
                // Exact match
                if (target == prop.bstrVal) {
                    return (int)i;
                }
            }
        }
    }
    return -1;
}

