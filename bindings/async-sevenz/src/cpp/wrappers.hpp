#pragma once

#include "Common/MyString.h"
#include "Common/MyVector.h"
#include "Common/MyCom.h" // Added for CMyComPtr, CMyUnknownImp
#include "7zip/Archive/IArchive.h"
#include "7zip/IPassword.h"
#include "7zip/ICoder.h"
#include "7zip/IStream.h"

// Forward declarations of wrapped Rust types?
// autocxx usually handles simple types.

// We need a class that implements IInStream and calls Rust
class RustInStream : public IInStream, public CMyUnknownImp
{
public:
    // Ptr to Rust callback/trait object?
    void* rust_reader_ptr; 
    
    Z7_COM_UNKNOWN_IMP_1(IInStream)
public:
    Z7_COM7F_IMF(Read(void *data, UInt32 size, UInt32 *processedSize));
    Z7_COM7F_IMF(Seek(Int64 offset, UInt32 seekOrigin, UInt64 *newPosition));
    
    RustInStream(void* ptr) : rust_reader_ptr(ptr) {}
    virtual ~RustInStream() {}
};

// We need a class that implements IArchiveExtractCallback
class RustExtractCallback : public IArchiveExtractCallback, public ICryptoGetTextPassword, public CMyUnknownImp
{
public:
    void* rust_callback_ptr;
    UInt32 targetIndex;
    
    Z7_COM_UNKNOWN_IMP_2(IArchiveExtractCallback, ICryptoGetTextPassword)

    // IProgress
public:
    Z7_COM7F_IMF(SetTotal(UInt64 total));
    Z7_COM7F_IMF(SetCompleted(const UInt64 *completeValue));

    // IArchiveExtractCallback
    Z7_COM7F_IMF(GetStream(UInt32 index, ISequentialOutStream **outStream, Int32 askExtractMode));
    Z7_COM7F_IMF(PrepareOperation(Int32 askExtractMode));
    Z7_COM7F_IMF(SetOperationResult(Int32 resultEOperationResult));

    // ICryptoGetTextPassword
    Z7_COM7F_IMF(CryptoGetTextPassword(BSTR *password));

    RustExtractCallback(void* ptr, UInt32 idx) : rust_callback_ptr(ptr), targetIndex(idx) {}
    virtual ~RustExtractCallback() {}
};

// Helper function to open archive
// Returns raw pointer that Rust can look at?
// Actually we probably want a wrapper struct that holds the C++ objects.

struct SevenZArchive {
    CMyComPtr<IInArchive> archive;
    CMyComPtr<IInStream> fileStream;
    
    SevenZArchive() {}
};

#include "wrappers_api.h"
