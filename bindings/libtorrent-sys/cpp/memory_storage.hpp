#pragma once

// Memory-only disk I/O implementation for libtorrent
// Used when cache_size = 0 (no disk writes)

#include <libtorrent/disk_interface.hpp>
#include <libtorrent/disk_buffer_holder.hpp>
#include <libtorrent/hasher.hpp>
#include <libtorrent/aux_/vector.hpp>
#include <libtorrent/storage_defs.hpp>
#include <libtorrent/file_storage.hpp>
#include <libtorrent/torrent_info.hpp>
#include <libtorrent/add_torrent_params.hpp>
#include <libtorrent/session.hpp>
#include <libtorrent/io_context.hpp>

#include "rust/cxx.h"

#include <map>
#include <mutex>
#include <memory>
#include <vector>

namespace lt = libtorrent;

namespace libtorrent_wrapper {

// Per-torrent in-memory storage
struct memory_torrent_storage {
    lt::file_storage files;
    int piece_length = 0;
    int num_pieces = 0;
    
    // Map: piece_index -> piece_data (full piece buffer)
    std::map<lt::piece_index_t, std::vector<char>> pieces;
    std::mutex mutex;
};

// Memory-only disk I/O - no files written to disk
// Implements lt::disk_interface with pieces stored in RAM
class memory_disk_io final : public lt::disk_interface, public lt::buffer_allocator_interface {
public:
    explicit memory_disk_io(lt::io_context& ioc, lt::counters& cnt);
    ~memory_disk_io() override = default;
    
    // ========== buffer_allocator_interface ==========
    void free_disk_buffer(char* buf) override;
    
    // ========== disk_interface: Torrent lifecycle ==========
    lt::storage_holder new_torrent(lt::storage_params const& p,
        std::shared_ptr<void> const& torrent) override;
    void remove_torrent(lt::storage_index_t idx) override;
    
    // ========== disk_interface: Read/Write ==========
    bool async_write(lt::storage_index_t storage, lt::peer_request const& r,
        char const* buf, std::shared_ptr<lt::disk_observer> o,
        std::function<void(lt::storage_error const&)> handler,
        lt::disk_job_flags_t flags = {}) override;
    
    void async_read(lt::storage_index_t storage, lt::peer_request const& r,
        std::function<void(lt::disk_buffer_holder, lt::storage_error const&)> handler,
        lt::disk_job_flags_t flags = {}) override;
    
    // ========== disk_interface: Hashing ==========
    void async_hash(lt::storage_index_t storage, lt::piece_index_t piece,
        lt::span<lt::sha256_hash> v2, lt::disk_job_flags_t flags,
        std::function<void(lt::piece_index_t, lt::sha1_hash const&, lt::storage_error const&)> handler) override;
    
    void async_hash2(lt::storage_index_t storage, lt::piece_index_t piece,
        int offset, lt::disk_job_flags_t flags,
        std::function<void(lt::piece_index_t, lt::sha256_hash const&, lt::storage_error const&)> handler) override;
    
    // ========== disk_interface: File operations (mostly no-ops for memory) ==========
    void async_move_storage(lt::storage_index_t storage, std::string p, lt::move_flags_t flags,
        std::function<void(lt::status_t, std::string const&, lt::storage_error const&)> handler) override;
    
    void async_release_files(lt::storage_index_t storage,
        std::function<void()> handler = std::function<void()>()) override;
    
    void async_check_files(lt::storage_index_t storage, lt::add_torrent_params const* resume_data,
        lt::aux::vector<std::string, lt::file_index_t> links,
        std::function<void(lt::status_t, lt::storage_error const&)> handler) override;
    
    void async_stop_torrent(lt::storage_index_t storage,
        std::function<void()> handler = std::function<void()>()) override;
    
    void async_rename_file(lt::storage_index_t storage, lt::file_index_t index, std::string name,
        std::function<void(std::string const&, lt::file_index_t, lt::storage_error const&)> handler) override;
    
    void async_delete_files(lt::storage_index_t storage, lt::remove_flags_t options,
        std::function<void(lt::storage_error const&)> handler) override;
    
    void async_set_file_priority(lt::storage_index_t storage,
        lt::aux::vector<lt::download_priority_t, lt::file_index_t> prio,
        std::function<void(lt::storage_error const&, lt::aux::vector<lt::download_priority_t, lt::file_index_t>)> handler) override;
    
    void async_clear_piece(lt::storage_index_t storage, lt::piece_index_t index,
        std::function<void(lt::piece_index_t)> handler) override;
    
    // ========== disk_interface: Stats and control ==========
    void update_stats_counters(lt::counters& c) const override;
    std::vector<lt::open_file_state> get_status(lt::storage_index_t) const override;
    void abort(bool wait) override;
    void submit_jobs() override;
    void settings_updated() override;

private:
    std::shared_ptr<memory_torrent_storage> get_storage(lt::storage_index_t idx);
    char* allocate_buffer();

public:
    std::vector<std::shared_ptr<memory_torrent_storage>> get_all_storages();
    
    lt::io_context& m_ioc;
    lt::counters& m_counters;
    
    std::map<lt::storage_index_t, std::shared_ptr<memory_torrent_storage>> m_torrents;
    std::mutex m_mutex;
    lt::storage_index_t m_next_storage_index{0};
    
    // Stats
    std::atomic<int64_t> m_total_read{0};
    std::atomic<int64_t> m_total_write{0};
};

// Factory function for session_params.disk_io_constructor
std::unique_ptr<lt::disk_interface> memory_disk_io_constructor(
    lt::io_context& ioc,
    lt::settings_interface const& settings,
    lt::counters& counters);

// Direct memory piece read — bypasses libtorrent's read_piece() which
// fails with custom disk interfaces due to slot list validation.
rust::Vec<uint8_t> memory_read_piece_direct(int32_t piece);

} // namespace libtorrent_wrapper
