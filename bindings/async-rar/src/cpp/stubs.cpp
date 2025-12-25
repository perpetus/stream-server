#include "rar.hpp"

// Global Error Handler instance
ErrorHandler ErrHandler;

// Console I/O stubs for RARDLL/SILENT mode
void OutComment(const std::wstring &Comment) {}
void SetConsoleMsgStream(MESSAGE_TYPE MsgStream) {}
void SetConsoleRedirectCharset(RAR_CHARSET RedirectCharset) {}
void ProhibitConsoleInput() {}
bool IsConsoleOutputPresent() { return false; }
