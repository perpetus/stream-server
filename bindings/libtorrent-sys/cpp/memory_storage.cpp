// Memory-only disk I/O implementation for libtorrent
// Used when cache_size = 0 (no disk writes)

#include "memory_storage.hpp"

#include <boost/asio/post.hpp>
#include <libtorrent/error_code.hpp>
#include <libtorrent/session_stats.hpp>
#include <openssl/sha.h>

#include <cstring>

#include "rust/cxx.h"
#include "libtorrent-sys/src/lib.rs.h"

namespace libtorrent_wrapper {

// Global reference to the memory disk I/O instance for direct piece reads.
// Protected by g_dio_mutex to prevent use-after-free on torrent removal.
static memory_disk_io* g_memory_disk_io = nullptr;
static std::mutex g_dio_mutex;

// ============================================================================
// Constructor
// ============================================================================

memory_disk_io::memory_disk_io(lt::io_context &ioc, lt::counters &cnt)
    : m_ioc(ioc), m_counters(cnt) {
  std::lock_guard<std::mutex> lock(g_dio_mutex);
  g_memory_disk_io = this;
}

// ============================================================================
// buffer_allocator_interface
// ============================================================================

void memory_disk_io::free_disk_buffer(char *buf) {
  // We use simple new/delete for buffers
  delete[] buf;
}

char *memory_disk_io::allocate_buffer() {
  // Allocate a 16KB block (standard libtorrent block size)
  return new char[16 * 1024];
}

// ============================================================================
// Torrent lifecycle
// ============================================================================

lt::storage_holder
memory_disk_io::new_torrent(lt::storage_params const &p,
                            std::shared_ptr<void> const & /* torrent */) {
  auto storage = std::make_shared<memory_torrent_storage>();
  storage->files = p.files;
  storage->piece_length = p.files.piece_length();
  storage->num_pieces = p.files.num_pieces();

  std::lock_guard<std::mutex> lock(m_mutex);
  lt::storage_index_t const idx = m_next_storage_index++;
  m_torrents[idx] = storage;

  return lt::storage_holder(idx, *this);
}

void memory_disk_io::remove_torrent(lt::storage_index_t idx) {
  std::lock_guard<std::mutex> lock(m_mutex);
  m_torrents.erase(idx);
}

std::shared_ptr<memory_torrent_storage>
memory_disk_io::get_storage(lt::storage_index_t idx) {
  std::lock_guard<std::mutex> lock(m_mutex);
  auto it = m_torrents.find(idx);
  return it != m_torrents.end() ? it->second : nullptr;
}

std::vector<std::shared_ptr<memory_torrent_storage>>
memory_disk_io::get_all_storages() {
  std::lock_guard<std::mutex> lock(m_mutex);
  std::vector<std::shared_ptr<memory_torrent_storage>> result;
  result.reserve(m_torrents.size());
  for (auto& [idx, st] : m_torrents) {
    result.push_back(st);
  }
  return result;
}

// ============================================================================
// async_write - Store piece data in memory
// ============================================================================

bool memory_disk_io::async_write(
    lt::storage_index_t storage, lt::peer_request const &r, char const *buf,
    std::shared_ptr<lt::disk_observer> /* o */,
    std::function<void(lt::storage_error const &)> handler,
    lt::disk_job_flags_t /* flags */) {

  auto st = get_storage(storage);
  if (!st) {

    handler(lt::storage_error(
        lt::error_code(boost::system::errc::no_such_file_or_directory,
                       boost::system::generic_category())));
    return false;
  }

  {
    std::lock_guard<std::mutex> lock(st->mutex);

    auto &piece_data = st->pieces[r.piece];

    // Ensure piece buffer is large enough
    size_t const required_size =
        static_cast<size_t>(r.start) + static_cast<size_t>(r.length);
    if (piece_data.size() < required_size) {
      piece_data.resize(required_size);
    }

    // Copy data to memory
    std::memcpy(piece_data.data() + r.start, buf,
                static_cast<size_t>(r.length));
    m_total_write += r.length;
  }

  // Notify completion (no error) asynchronously
  boost::asio::post(m_ioc, [handler]() { handler(lt::storage_error()); });

  return false; // Not blocking - write queue not full
}

// ============================================================================
// async_read - Read piece data from memory
// ============================================================================

void memory_disk_io::async_read(
    lt::storage_index_t storage, lt::peer_request const &r,
    std::function<void(lt::disk_buffer_holder, lt::storage_error const &)>
        handler,
    lt::disk_job_flags_t /* flags */) {

  auto st = get_storage(storage);
  if (!st) {

    handler(lt::disk_buffer_holder(),
            lt::storage_error(
                lt::error_code(boost::system::errc::no_such_file_or_directory,
                               boost::system::generic_category())));
    return;
  }

  std::vector<char> result;
  {
    std::lock_guard<std::mutex> lock(st->mutex);

    auto it = st->pieces.find(r.piece);
    if (it == st->pieces.end()) {

      handler(lt::disk_buffer_holder(),
              lt::storage_error(
                  lt::error_code(boost::system::errc::no_such_file_or_directory,
                                 boost::system::generic_category())));
      return;
    }

    auto const &piece_data = it->second;
    size_t const start = static_cast<size_t>(r.start);
    size_t const length = static_cast<size_t>(r.length);

    if (piece_data.size() < start + length) {

      handler(lt::disk_buffer_holder(),
              lt::storage_error(
                  lt::error_code(boost::system::errc::no_such_file_or_directory,
                                 boost::system::generic_category())));
      return;
    }

    // Copy data out while holding lock
    result.assign(piece_data.begin() + start,
                  piece_data.begin() + start + length);
    m_total_read += r.length;
  }

  // Create buffer holder and copy data
  // Note: we allocate exact size needed, not fixed 16KB
  char *buf = new char[result.size()];
  std::memcpy(buf, result.data(), result.size());

  lt::disk_buffer_holder holder(*this, buf, static_cast<int>(result.size()));

  boost::asio::post(m_ioc, [handler, h = std::move(holder)]() mutable {
    handler(std::move(h), lt::storage_error());
  });
}

// ============================================================================
// async_hash - Compute SHA-1 hash of piece from memory
// ============================================================================

void memory_disk_io::async_hash(
    lt::storage_index_t storage, lt::piece_index_t piece,
    lt::span<lt::sha256_hash> /* v2 */, lt::disk_job_flags_t flags,
    std::function<void(lt::piece_index_t, lt::sha1_hash const &,
                       lt::storage_error const &)>
        handler) {
  auto st = get_storage(storage);
  if (!st) {
    boost::asio::post(m_ioc, [handler, piece]() {
      handler(piece, lt::sha1_hash(),
              lt::storage_error(
                  lt::error_code(boost::system::errc::no_such_file_or_directory,
                                 boost::system::generic_category())));
    });
    return;
  }

  lt::sha1_hash hash;
  {
    std::lock_guard<std::mutex> lock(st->mutex);

    auto it = st->pieces.find(piece);
    if (it == st->pieces.end()) {
      // Piece not in memory - that's an error
      boost::asio::post(m_ioc, [handler, piece]() {
        handler(piece, lt::sha1_hash(),
                lt::storage_error(lt::error_code(
                    boost::system::errc::no_such_file_or_directory,
                    boost::system::generic_category())));
      });
      return;
    }

    auto const &piece_data = it->second;

    // Always compute SHA-1 hash (libtorrent expects a valid sha1_hash for v1 torrents)
    unsigned char sha1_result[20];
    SHA1(reinterpret_cast<const unsigned char *>(piece_data.data()),
         piece_data.size(), sha1_result);
    // Copy to libtorrent's sha1_hash format
    std::memcpy(hash.data(), sha1_result, 20);
  }

  boost::asio::post(m_ioc, [handler, piece, hash]() {
    handler(piece, hash, lt::storage_error());
  });
}

void memory_disk_io::async_hash2(
    lt::storage_index_t /* storage */, lt::piece_index_t piece,
    int /* offset */, lt::disk_job_flags_t /* flags */,
    std::function<void(lt::piece_index_t, lt::sha256_hash const &,
                       lt::storage_error const &)>
        handler) {
  // V2 hash - not implemented for memory-only mode (we focus on v1 torrents)
  // Return empty hash - libtorrent will handle this gracefully
  boost::asio::post(m_ioc, [handler, piece]() {
    handler(piece, lt::sha256_hash(), lt::storage_error());
  });
}

// ============================================================================
// File operations - mostly no-ops for memory storage
// ============================================================================

void memory_disk_io::async_move_storage(
    lt::storage_index_t /* storage */, std::string p,
    lt::move_flags_t /* flags */,
    std::function<void(lt::status_t, std::string const &,
                       lt::storage_error const &)>
        handler) {
  // No-op for memory storage
  boost::asio::post(m_ioc, [handler, p = std::move(p)]() {
    handler(lt::status_t::no_error, p, lt::storage_error());
  });
}

void memory_disk_io::async_release_files(lt::storage_index_t /* storage */,
                                         std::function<void()> handler) {
  if (handler) {
    boost::asio::post(m_ioc, handler);
  }
}

void memory_disk_io::async_check_files(
    lt::storage_index_t /* storage */,
    lt::add_torrent_params const * /* resume_data */,
    lt::aux::vector<std::string, lt::file_index_t> /* links */,
    std::function<void(lt::status_t, lt::storage_error const &)> handler) {
  // For memory storage, always report no pieces (fresh start)
  boost::asio::post(m_ioc, [handler]() {
    handler(lt::status_t::no_error, lt::storage_error());
  });
}

void memory_disk_io::async_stop_torrent(lt::storage_index_t /* storage */,
                                        std::function<void()> handler) {
  if (handler) {
    boost::asio::post(m_ioc, handler);
  }
}

void memory_disk_io::async_rename_file(
    lt::storage_index_t /* storage */, lt::file_index_t index, std::string name,
    std::function<void(std::string const &, lt::file_index_t,
                       lt::storage_error const &)>
        handler) {
  // No-op for memory storage
  boost::asio::post(m_ioc, [handler, name = std::move(name), index]() {
    handler(name, index, lt::storage_error());
  });
}

void memory_disk_io::async_delete_files(
    lt::storage_index_t storage, lt::remove_flags_t /* options */,
    std::function<void(lt::storage_error const &)> handler) {
  // Clear all pieces for this torrent
  auto st = get_storage(storage);
  if (st) {
    std::lock_guard<std::mutex> lock(st->mutex);
    st->pieces.clear();
  }

  boost::asio::post(m_ioc, [handler]() { handler(lt::storage_error()); });
}

void memory_disk_io::async_set_file_priority(
    lt::storage_index_t /* storage */,
    lt::aux::vector<lt::download_priority_t, lt::file_index_t> prio,
    std::function<
        void(lt::storage_error const &,
             lt::aux::vector<lt::download_priority_t, lt::file_index_t>)>
        handler) {
  // No-op for memory storage - priorities don't affect storage
  boost::asio::post(m_ioc, [handler, prio = std::move(prio)]() {
    handler(lt::storage_error(), prio);
  });
}

void memory_disk_io::async_clear_piece(
    lt::storage_index_t storage, lt::piece_index_t index,
    std::function<void(lt::piece_index_t)> handler) {
  auto st = get_storage(storage);
  if (st) {
    std::lock_guard<std::mutex> lock(st->mutex);
    st->pieces.erase(index);
  }

  boost::asio::post(m_ioc, [handler, index]() { handler(index); });
}

// ============================================================================
// Stats and control
// ============================================================================

void memory_disk_io::update_stats_counters(lt::counters & /* c */) const {
  // Could add memory usage stats here if needed
}

std::vector<lt::open_file_state>
memory_disk_io::get_status(lt::storage_index_t) const {
  // No open files for memory storage
  return {};
}

void memory_disk_io::abort(bool /* wait */) {
  // Nothing to abort
}

void memory_disk_io::submit_jobs() {
  // Jobs are processed immediately
}

void memory_disk_io::settings_updated() {
  // No settings to update
}

// ============================================================================
// Factory function
// ============================================================================

std::unique_ptr<lt::disk_interface>
memory_disk_io_constructor(lt::io_context &ioc,
                           lt::settings_interface const & /* settings */,
                           lt::counters &counters) {

  return std::make_unique<memory_disk_io>(ioc, counters);
}

rust::Vec<uint8_t> memory_read_piece_direct(int32_t piece) {
  rust::Vec<uint8_t> result;
  std::lock_guard<std::mutex> lock(g_dio_mutex);
  if (!g_memory_disk_io) return result;

  auto storages = g_memory_disk_io->get_all_storages();
  for (auto& st : storages) {
    std::lock_guard<std::mutex> slock(st->mutex);
    auto it = st->pieces.find(lt::piece_index_t(piece));
    if (it != st->pieces.end()) {
      auto const& data = it->second;
      result.reserve(data.size());
      // Bulk copy using heap buffer to avoid stack issues with 8MB+ pieces
      auto* buf = new uint8_t[data.size()];
      std::memcpy(buf, data.data(), data.size());
      for (size_t i = 0; i < data.size(); i++) {
        result.push_back(buf[i]);
      }
      delete[] buf;
      return result;
    }
  }
  return result;
}

} // namespace libtorrent_wrapper
