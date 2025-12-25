#pragma once

struct SevenZArchive;

#ifdef __cplusplus
extern "C" {
#endif

// Function declarations
SevenZArchive* OpenArchive(void* rust_reader_ptr);
void CloseArchive(SevenZArchive* arch);
int ExtractEntry(SevenZArchive* arch, unsigned int index, void* rust_callback_ptr);

unsigned int GetArchiveItemCount(SevenZArchive* arch);
// Returns 0 on success, <0 on error
// buf should be large enough, or we can use a callback allocation strategy?
// Simpler: Return a C-string allocated by C++ that Rust must free?
// Or pass a buffer and size.
// For filenames, let's use a simple allocation.
char* GetArchiveItemName(SevenZArchive* arch, unsigned int index);
void FreeString(char* str);

int IsArchiveItemFolder(SevenZArchive* arch, unsigned int index);
unsigned long long GetArchiveItemSize(SevenZArchive* arch, unsigned int index);
int GetArchiveItemIndex(SevenZArchive* arch, const char* name);

#ifdef __cplusplus
}
#endif
