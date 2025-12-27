


// Include the cxx-generated header first (contains struct definitions)
#include "libtorrent-sys/src/lib.rs.h"
#include "libtorrent-sys/cpp/wrapper.h"

#include <libtorrent/session_params.hpp>
#include <libtorrent/session_stats.hpp>
#include <libtorrent/add_torrent_params.hpp>
#include <libtorrent/torrent_info.hpp>
#include <libtorrent/magnet_uri.hpp>
#include <libtorrent/alert_types.hpp>
#include <libtorrent/peer_info.hpp>
#include <libtorrent/announce_entry.hpp>
#include <libtorrent/write_resume_data.hpp>
#include <libtorrent/read_resume_data.hpp>
#include <libtorrent/hex.hpp>
#include <libtorrent/version.hpp>
#include <libtorrent/create_torrent.hpp>
#include <libtorrent/bencode.hpp>

#include <sstream>
#include <chrono>
#include <iterator>

namespace libtorrent_wrapper {

// ============================================================================
// HELPER FUNCTIONS
// ============================================================================

static rust::String sha1_to_hex(const lt::sha1_hash& hash) {
    std::stringstream ss;
    ss << hash;
    return rust::String(ss.str());
}

static rust::String sha256_to_hex(const lt::sha256_hash& hash) {
    std::stringstream ss;
    ss << hash;
    return rust::String(ss.str());
}

static lt::sha1_hash hex_to_sha1(rust::Str hex) {
    std::string hex_str(hex.data(), hex.size());
    lt::sha1_hash hash;
    lt::aux::from_hex(hex_str, hash.data());
    return hash;
}

static std::string rust_str_to_std(rust::Str s) {
    return std::string(s.data(), s.size());
}

static TorrentStatus make_torrent_status(const lt::torrent_status& ts) {
    TorrentStatus status;
    status.info_hash = sha1_to_hex(ts.info_hashes.get_best());
    status.info_hash_v2 = ts.info_hashes.has_v2() ? sha256_to_hex(ts.info_hashes.v2) : rust::String("");
    status.name = rust::String(ts.name);
    status.save_path = rust::String(ts.save_path);
    status.state = static_cast<int32_t>(ts.state);
    status.total_size = ts.total;
    status.total_done = ts.total_done;
    status.total_downloaded = ts.all_time_download;
    status.total_uploaded = ts.all_time_upload;
    status.total_wanted = ts.total_wanted;
    status.total_wanted_done = ts.total_wanted_done;
    status.download_rate = ts.download_rate;
    status.upload_rate = ts.upload_rate;
    status.download_payload_rate = ts.download_payload_rate;
    status.upload_payload_rate = ts.upload_payload_rate;
    status.num_peers = ts.num_peers;
    status.num_seeds = ts.num_seeds;
    status.num_incomplete = ts.num_incomplete;
    status.num_complete = ts.num_complete;
    status.progress = ts.progress;
    status.progress_ppm = ts.progress_ppm;
    status.is_paused = bool(ts.flags & lt::torrent_flags::paused);
    status.is_auto_managed = bool(ts.flags & lt::torrent_flags::auto_managed);
    status.is_finished = ts.is_finished;
    status.is_seeding = ts.is_seeding;
    status.has_metadata = ts.has_metadata;
    status.sequential_download = bool(ts.flags & lt::torrent_flags::sequential_download);
    status.current_tracker = rust::String(ts.current_tracker);
    status.next_announce_seconds = static_cast<int32_t>(
        std::chrono::duration_cast<std::chrono::seconds>(ts.next_announce).count());
    status.num_pieces = ts.num_pieces;
    // piece_length is in torrent_info, not torrent_status
    status.piece_length = 0;
    status.added_time = ts.added_time;
    status.completed_time = ts.completed_time;
    status.last_seen_complete = ts.last_seen_complete;
    status.queue_position = static_cast<int32_t>(static_cast<int>(ts.queue_position));
    status.error = ts.errc ? rust::String(ts.errc.message()) : rust::String("");
    return status;
}

// ============================================================================
// SESSION LIFECYCLE
// ============================================================================

std::unique_ptr<Session> create_session(SessionSettings const& settings) {
    lt::settings_pack pack;
    
    if (!settings.listen_interfaces.empty()) {
        pack.set_str(lt::settings_pack::listen_interfaces, rust_str_to_std(settings.listen_interfaces));
    } else {
        pack.set_str(lt::settings_pack::listen_interfaces, "0.0.0.0:6881,[::]:6881");
    }
    
    if (!settings.user_agent.empty()) {
        pack.set_str(lt::settings_pack::user_agent, rust_str_to_std(settings.user_agent));
    }
    
    pack.set_bool(lt::settings_pack::enable_dht, settings.enable_dht);
    pack.set_bool(lt::settings_pack::enable_lsd, settings.enable_lsd);
    pack.set_bool(lt::settings_pack::enable_upnp, settings.enable_upnp);
    pack.set_bool(lt::settings_pack::enable_natpmp, settings.enable_natpmp);
    pack.set_bool(lt::settings_pack::anonymous_mode, settings.anonymous_mode);
    pack.set_bool(lt::settings_pack::announce_to_all_trackers, settings.announce_to_all_trackers);
    pack.set_bool(lt::settings_pack::announce_to_all_tiers, settings.announce_to_all_tiers);
    
    // Connection limits
    if (settings.max_connections > 0)
        pack.set_int(lt::settings_pack::connections_limit, settings.max_connections);
    
    // Rate limits
    if (settings.download_rate_limit > 0)
        pack.set_int(lt::settings_pack::download_rate_limit, settings.download_rate_limit);
    if (settings.upload_rate_limit > 0)
        pack.set_int(lt::settings_pack::upload_rate_limit, settings.upload_rate_limit);
    
    // Active torrent limits
    if (settings.active_downloads > 0)
        pack.set_int(lt::settings_pack::active_downloads, settings.active_downloads);
    if (settings.active_seeds > 0)
        pack.set_int(lt::settings_pack::active_seeds, settings.active_seeds);
    if (settings.active_limit > 0)
        pack.set_int(lt::settings_pack::active_limit, settings.active_limit);
    
    // =========================================================================
    // PERFORMANCE OPTIMIZATIONS FOR WINDOWS
    // =========================================================================
    
    // Socket buffer sizes - larger buffers help with high latency connections
    pack.set_int(lt::settings_pack::send_buffer_watermark, 512 * 1024); // 512KB
    pack.set_int(lt::settings_pack::send_buffer_watermark_factor, 150);
    pack.set_int(lt::settings_pack::send_buffer_low_watermark, 10 * 1024); // 10KB
    
    // Choking algorithm optimized for downloading
    // 0 = fixed_slots_choker, 2 = fastest_upload
    pack.set_int(lt::settings_pack::choking_algorithm, 0);
    pack.set_int(lt::settings_pack::seed_choking_algorithm, 2);
    
    // Connection tuning
    pack.set_int(lt::settings_pack::request_timeout, 10);
    pack.set_int(lt::settings_pack::peer_timeout, 60);
    pack.set_int(lt::settings_pack::min_reconnect_time, 1);
    pack.set_int(lt::settings_pack::max_failcount, 3);
    pack.set_int(lt::settings_pack::connection_speed, 200); // Connections per second (was 30)
    
    // DHT Bootstrap nodes - helpful for fast initial startup
    pack.set_str(lt::settings_pack::dht_bootstrap_nodes, 
        "router.bittorrent.com:6881,"
        "router.utorrent.com:6881,"
        "dht.transmissionbt.com:6881,"
        "dht.libtorrent.org:25401,"
        "router.bitcomet.com:6881");

    // Piece selection for streaming
    // CRITICAL: Enable strict endgame mode for deadline pieces
    // This allows requesting the same blocks from MULTIPLE peers simultaneously
    // The fastest peer wins, dramatically speeding up critical piece downloads
    pack.set_bool(lt::settings_pack::strict_end_game_mode, true);
    pack.set_bool(lt::settings_pack::prioritize_partial_pieces, true);
    
    // Faster connections for streaming
    pack.set_bool(lt::settings_pack::smooth_connects, false);  // Don't spread out connection attempts
    pack.set_int(lt::settings_pack::piece_timeout, 5);          // Time before considering piece stalled (was 20)
    pack.set_int(lt::settings_pack::peer_connect_timeout, 3);   // Faster peer connection timeout (was 15)
    pack.set_int(lt::settings_pack::request_timeout, 10);       // Faster request timeout (was 60)
    
    // Increase request queue depth for more aggressive downloading
    pack.set_int(lt::settings_pack::max_out_request_queue, 500);      // Default 200
    pack.set_int(lt::settings_pack::max_allowed_in_request_queue, 250); // Default 100
    pack.set_int(lt::settings_pack::request_queue_time, 3);            // Default 5, reduce for faster requests
    
    // Outgoing connections
    pack.set_int(lt::settings_pack::unchoke_slots_limit, 20); // (was 8)
    
    // Alert mask for debugging (can remove in production)
    pack.set_int(lt::settings_pack::alert_mask, 
        lt::alert_category::error | lt::alert_category::peer | 
        lt::alert_category::status | lt::alert_category::storage);
    
    // Proxy settings
    if (!settings.proxy_host.empty()) {
        pack.set_str(lt::settings_pack::proxy_hostname, rust_str_to_std(settings.proxy_host));
        pack.set_int(lt::settings_pack::proxy_port, settings.proxy_port);
        pack.set_int(lt::settings_pack::proxy_type, settings.proxy_type);
    }
    
    lt::session_params params(pack);
    return std::make_unique<Session>(std::move(params));
}

void session_abort(Session& session) {
    session.session.abort();
}

void session_pause(Session& session) {
    session.session.pause();
}

void session_resume(Session& session) {
    session.session.resume();
}

bool session_is_paused(Session const& session) {
    return session.session.is_paused();
}

// ============================================================================
// SESSION SETTINGS
// ============================================================================

void session_apply_settings(Session& session, SessionSettings const& settings) {
    lt::settings_pack pack;
    if (!settings.listen_interfaces.empty())
        pack.set_str(lt::settings_pack::listen_interfaces, rust_str_to_std(settings.listen_interfaces));
    pack.set_bool(lt::settings_pack::enable_dht, settings.enable_dht);
    pack.set_bool(lt::settings_pack::enable_lsd, settings.enable_lsd);
    pack.set_bool(lt::settings_pack::enable_upnp, settings.enable_upnp);
    pack.set_bool(lt::settings_pack::enable_natpmp, settings.enable_natpmp);
    pack.set_bool(lt::settings_pack::announce_to_all_trackers, settings.announce_to_all_trackers);
    pack.set_bool(lt::settings_pack::announce_to_all_tiers, settings.announce_to_all_tiers);
    if (settings.download_rate_limit >= 0)
        pack.set_int(lt::settings_pack::download_rate_limit, settings.download_rate_limit);
    if (settings.upload_rate_limit >= 0)
        pack.set_int(lt::settings_pack::upload_rate_limit, settings.upload_rate_limit);
    session.session.apply_settings(pack);
}

int32_t session_get_download_rate_limit(Session const& session) {
    return session.session.get_settings().get_int(lt::settings_pack::download_rate_limit);
}

int32_t session_get_upload_rate_limit(Session const& session) {
    return session.session.get_settings().get_int(lt::settings_pack::upload_rate_limit);
}

void session_set_download_rate_limit(Session& session, int32_t limit) {
    lt::settings_pack pack;
    pack.set_int(lt::settings_pack::download_rate_limit, limit);
    session.session.apply_settings(pack);
}

void session_set_upload_rate_limit(Session& session, int32_t limit) {
    lt::settings_pack pack;
    pack.set_int(lt::settings_pack::upload_rate_limit, limit);
    session.session.apply_settings(pack);
}

// ============================================================================
// ADD/REMOVE TORRENTS
// ============================================================================

std::unique_ptr<TorrentHandle> session_add_torrent(Session& session, AddTorrentParams const& params) {
    lt::add_torrent_params p;
    
    if (!params.magnet_uri.empty()) {
        lt::error_code ec;
        lt::parse_magnet_uri(rust_str_to_std(params.magnet_uri), p, ec);
        if (ec) throw std::runtime_error("Failed to parse magnet: " + ec.message());
    }
    
    if (!params.torrent_data.empty()) {
        lt::error_code ec;
        p.ti = std::make_shared<lt::torrent_info>(
            reinterpret_cast<const char*>(params.torrent_data.data()),
            static_cast<int>(params.torrent_data.size()), ec);
        if (ec) throw std::runtime_error("Failed to parse torrent: " + ec.message());
    }
    
    p.save_path = rust_str_to_std(params.save_path);
    
    if (!params.name.empty())
        p.name = rust_str_to_std(params.name);
    
    for (const auto& t : params.trackers)
        p.trackers.push_back(std::string(t.data(), t.size()));
    
    if (params.paused)
        p.flags |= lt::torrent_flags::paused;
    if (params.auto_managed)
        p.flags |= lt::torrent_flags::auto_managed;
    if (params.sequential_download)
        p.flags |= lt::torrent_flags::sequential_download;
    
    p.upload_limit = params.upload_limit;
    p.download_limit = params.download_limit;
    
    lt::torrent_handle h = session.session.add_torrent(p);
    if (!h.is_valid()) throw std::runtime_error("Failed to add torrent");
    
    return std::make_unique<TorrentHandle>(std::move(h));
}

std::unique_ptr<TorrentHandle> session_add_magnet(Session& session, rust::Str magnet_uri, rust::Str save_path) {
    lt::add_torrent_params p;
    lt::error_code ec;
    lt::parse_magnet_uri(rust_str_to_std(magnet_uri), p, ec);
    if (ec) throw std::runtime_error("Failed to parse magnet: " + ec.message());
    
    p.save_path = rust_str_to_std(save_path);
    
    lt::torrent_handle h = session.session.add_torrent(p);
    if (!h.is_valid()) throw std::runtime_error("Failed to add torrent");
    
    return std::make_unique<TorrentHandle>(std::move(h));
}

void session_remove_torrent(Session& session, TorrentHandle const& handle, bool delete_files) {
    lt::remove_flags_t flags = delete_files ? lt::session::delete_files : lt::remove_flags_t{};
    session.session.remove_torrent(handle.handle, flags);
}

// ============================================================================
// TORRENT QUERIES
// ============================================================================

rust::Vec<TorrentStatus> session_get_torrents(Session const& session) {
    rust::Vec<TorrentStatus> result;
    for (const auto& h : session.session.get_torrents()) {
        if (!h.is_valid()) continue;
        result.push_back(make_torrent_status(h.status()));
    }
    return result;
}

std::unique_ptr<TorrentHandle> session_find_torrent(Session const& session, rust::Str info_hash) {
    lt::sha1_hash hash = hex_to_sha1(info_hash);
    lt::torrent_handle h = session.session.find_torrent(hash);
    if (!h.is_valid()) throw std::runtime_error("Torrent not found");
    return std::make_unique<TorrentHandle>(std::move(h));
}

TorrentStatus session_get_torrent_status(Session const& session, rust::Str info_hash) {
    lt::sha1_hash hash = hex_to_sha1(info_hash);
    lt::torrent_handle h = session.session.find_torrent(hash);
    if (!h.is_valid()) throw std::runtime_error("Torrent not found");
    return make_torrent_status(h.status());
}

// ============================================================================
// SESSION STATS
// ============================================================================

SessionStats session_get_stats(Session const& session) {
    SessionStats stats;
    auto handles = session.session.get_torrents();
    
    stats.num_torrents = static_cast<int32_t>(handles.size());
    stats.download_rate = 0;
    stats.upload_rate = 0;
    stats.total_download = 0;
    stats.total_upload = 0;
    stats.num_peers = 0;
    
    for (const auto& h : handles) {
        if (!h.is_valid()) continue;
        auto ts = h.status();
        stats.download_rate += ts.download_rate;
        stats.upload_rate += ts.upload_rate;
        stats.total_download += ts.all_time_download;
        stats.total_upload += ts.all_time_upload;
        stats.num_peers += ts.num_peers;
    }
    
    stats.dht_enabled = session.session.is_dht_running();
    stats.dht_nodes = 0; // Would need session_status for this
    
    return stats;
}

DhtStats session_get_dht_stats(Session const& session) {
    DhtStats stats;
    stats.dht_nodes = 0;
    stats.dht_node_cache = 0;
    stats.dht_torrents = 0;
    stats.total_peers = 0;
    // Full DHT stats require more complex API
    return stats;
}

// ============================================================================
// DHT
// ============================================================================

bool session_is_dht_running(Session const& session) {
    return session.session.is_dht_running();
}

void session_add_dht_node(Session& session, rust::Str host, int32_t port) {
    session.session.add_dht_node({rust_str_to_std(host), port});
}

void session_dht_get_peers(Session& session, rust::Str info_hash) {
    lt::sha1_hash hash = hex_to_sha1(info_hash);
    session.session.dht_get_peers(hash);
}

// ============================================================================
// ALERTS
// ============================================================================

rust::Vec<AlertInfo> session_pop_alerts(Session& session) {
    rust::Vec<AlertInfo> result;
    std::vector<lt::alert*> alerts;
    session.session.pop_alerts(&alerts);
    
    for (const lt::alert* a : alerts) {
        AlertInfo info;
        info.alert_type = a->type();
        info.category = static_cast<int32_t>(static_cast<uint32_t>(a->category()));
        info.what = rust::String(a->what());
        info.message = rust::String(a->message());
        info.timestamp = std::chrono::duration_cast<std::chrono::milliseconds>(
            a->timestamp().time_since_epoch()).count();
        
        // Try to get info_hash if this is a torrent alert
        if (const auto* ta = dynamic_cast<const lt::torrent_alert*>(a)) {
            info.info_hash = sha1_to_hex(ta->handle.info_hashes().get_best());
        }
        
        result.push_back(std::move(info));
    }
    return result;
}

bool session_wait_for_alert(Session const& session, int32_t timeout_ms) {
    // wait_for_alert is actually non-const in libtorrent, but we cast away const for this query
    return const_cast<lt::session&>(session.session).wait_for_alert(std::chrono::milliseconds(timeout_ms)) != nullptr;
}

void session_set_alert_mask(Session& session, uint32_t mask) {
    lt::settings_pack pack;
    pack.set_int(lt::settings_pack::alert_mask, static_cast<int>(mask));
    session.session.apply_settings(pack);
}

// ============================================================================
// STATE PERSISTENCE
// ============================================================================

rust::Vec<uint8_t> session_save_state(Session const& session) {
    // In ABI v3, use write_session_params instead of save_state
    auto params = session.session.session_state();
    std::vector<char> buf = lt::write_session_params_buf(params);
    rust::Vec<uint8_t> result;
    for (char c : buf) result.push_back(static_cast<uint8_t>(c));
    return result;
}

void session_load_state(Session& session, rust::Slice<const uint8_t> state) {
    // In ABI v3, use read_session_params instead of load_state
    lt::span<char const> span(reinterpret_cast<const char*>(state.data()), state.size());
    lt::session_params params = lt::read_session_params(span);
    session.session.apply_settings(params.settings);
}

rust::Vec<uint8_t> session_save_dht_state(Session const& session) {
    // In ABI v3, use write_session_params with save_dht_state flag
    auto params = session.session.session_state(lt::session::save_dht_state);
    std::vector<char> buf = lt::write_session_params_buf(params, lt::session::save_dht_state);
    rust::Vec<uint8_t> result;
    for (char c : buf) result.push_back(static_cast<uint8_t>(c));
    return result;
}

void session_load_dht_state(Session& session, rust::Slice<const uint8_t> state) {
    // In ABI v3, use read_session_params instead of load_state  
    lt::span<char const> span(reinterpret_cast<const char*>(state.data()), state.size());
    lt::session_params params = lt::read_session_params(span, lt::session::save_dht_state);
    // DHT state is part of session_params but can't be applied to running session
    // This would require recreating the session with these params
}

// ============================================================================
// TORRENT HANDLE - BASIC INFO
// ============================================================================

bool handle_is_valid(TorrentHandle const& handle) {
    return handle.handle.is_valid();
}

rust::String handle_get_info_hash(TorrentHandle const& handle) {
    return sha1_to_hex(handle.handle.info_hashes().get_best());
}

std::unique_ptr<TorrentHandle> handle_clone(TorrentHandle const& handle) {
    return std::make_unique<TorrentHandle>(handle.handle);
}

rust::String handle_get_info_hash_v2(TorrentHandle const& handle) {
    auto ih = handle.handle.info_hashes();
    return ih.has_v2() ? sha256_to_hex(ih.v2) : rust::String("");
}

rust::String handle_get_name(TorrentHandle const& handle) {
    return rust::String(handle.handle.status().name);
}

TorrentStatus handle_get_status(TorrentHandle const& handle) {
    return make_torrent_status(handle.handle.status());
}

// ============================================================================
// TORRENT HANDLE - CONTROL
// ============================================================================

void handle_pause(TorrentHandle& handle) {
    handle.handle.pause();
}

void handle_resume(TorrentHandle& handle) {
    handle.handle.resume();
}

void handle_set_upload_limit(TorrentHandle& handle, int32_t limit) {
    handle.handle.set_upload_limit(limit);
}

void handle_set_download_limit(TorrentHandle& handle, int32_t limit) {
    handle.handle.set_download_limit(limit);
}

int32_t handle_get_upload_limit(TorrentHandle const& handle) {
    return handle.handle.upload_limit();
}

int32_t handle_get_download_limit(TorrentHandle const& handle) {
    return handle.handle.download_limit();
}

void handle_force_recheck(TorrentHandle& handle) {
    handle.handle.force_recheck();
}

void handle_force_reannounce(TorrentHandle& handle) {
    handle.handle.force_reannounce();
}

void handle_force_dht_announce(TorrentHandle& handle) {
    handle.handle.force_dht_announce();
}

// ============================================================================
// TORRENT HANDLE - SEQUENTIAL/STREAMING
// ============================================================================

void handle_set_sequential_download(TorrentHandle& handle, bool enable) {
    if (enable)
        handle.handle.set_flags(lt::torrent_flags::sequential_download);
    else
        handle.handle.unset_flags(lt::torrent_flags::sequential_download);
}

bool handle_is_sequential_download(TorrentHandle const& handle) {
    return bool(handle.handle.flags() & lt::torrent_flags::sequential_download);
}

void handle_set_piece_deadline(TorrentHandle& handle, int32_t piece, int32_t deadline_ms) {
    handle.handle.set_piece_deadline(lt::piece_index_t(piece), deadline_ms);
}

void handle_reset_piece_deadline(TorrentHandle& handle, int32_t piece) {
    handle.handle.reset_piece_deadline(lt::piece_index_t(piece));
}

void handle_clear_piece_deadlines(TorrentHandle& handle) {
    handle.handle.clear_piece_deadlines();
}

// ============================================================================
// TORRENT HANDLE - FILES
// ============================================================================

rust::Vec<FileInfo> handle_get_files(TorrentHandle const& handle) {
    rust::Vec<FileInfo> result;
    
    auto ti = handle.handle.torrent_file();
    if (!ti) return result;
    
    auto ts = handle.handle.status();
    const lt::file_storage& files = ti->files();
    auto file_progress = handle.handle.file_progress();
    auto priorities = handle.handle.get_file_priorities();
    
    for (lt::file_index_t i(0); i < files.end_file(); ++i) {
        FileInfo info;
        info.index = static_cast<int32_t>(static_cast<int>(i));
        info.path = rust::String(files.file_path(i));
        info.absolute_path = rust::String(ts.save_path + "/" + files.file_path(i));
        info.size = files.file_size(i);
        info.downloaded = file_progress.size() > static_cast<std::size_t>(static_cast<int>(i)) ? 
                          file_progress[static_cast<int>(i)] : 0;
        info.priority = priorities.size() > static_cast<std::size_t>(static_cast<int>(i)) ?
                        static_cast<int32_t>(static_cast<uint8_t>(priorities[static_cast<int>(i)])) : 4;
        info.progress = info.size > 0 ? static_cast<float>(info.downloaded) / static_cast<float>(info.size) : 0.0f;
        
        // Calculate first and last piece from file offset and size
        auto file_offset = files.file_offset(i);
        auto file_size = files.file_size(i);
        auto piece_len = ti->piece_length();
        info.first_piece = static_cast<int32_t>(file_offset / piece_len);
        info.last_piece = static_cast<int32_t>((file_offset + file_size - 1) / piece_len);
        info.offset = file_offset;
        
        result.push_back(std::move(info));
    }
    return result;
}

rust::Vec<int32_t> handle_get_file_priorities(TorrentHandle const& handle) {
    rust::Vec<int32_t> result;
    for (auto p : handle.handle.get_file_priorities())
        result.push_back(static_cast<int32_t>(static_cast<uint8_t>(p)));
    return result;
}

void handle_set_file_priority(TorrentHandle& handle, int32_t index, int32_t priority) {
    handle.handle.file_priority(lt::file_index_t(index), lt::download_priority_t(static_cast<uint8_t>(priority)));
}

void handle_set_file_priorities(TorrentHandle& handle, rust::Slice<const int32_t> priorities) {
    std::vector<lt::download_priority_t> prios;
    for (int32_t p : priorities)
        prios.push_back(lt::download_priority_t(static_cast<uint8_t>(p)));
    handle.handle.prioritize_files(prios);
}

void handle_rename_file(TorrentHandle& handle, int32_t index, rust::Str new_name) {
    handle.handle.rename_file(lt::file_index_t(index), rust_str_to_std(new_name));
}

void handle_move_storage(TorrentHandle& handle, rust::Str new_path) {
    handle.handle.move_storage(rust_str_to_std(new_path));
}

// ============================================================================
// TORRENT HANDLE - PIECES
// ============================================================================

int32_t handle_num_pieces(TorrentHandle const& handle) {
    auto ti = handle.handle.torrent_file();
    return ti ? ti->num_pieces() : 0;
}

int32_t handle_piece_length(TorrentHandle const& handle) {
    auto ti = handle.handle.torrent_file();
    return ti ? ti->piece_length() : 0;
}

bool handle_have_piece(TorrentHandle const& handle, int32_t piece) {
    return handle.handle.have_piece(lt::piece_index_t(piece));
}

rust::Vec<int32_t> handle_get_piece_availability(TorrentHandle const& handle) {
    rust::Vec<int32_t> result;
    std::vector<int> avail;
    handle.handle.piece_availability(avail);
    for (int a : avail) result.push_back(a);
    return result;
}

void handle_set_piece_priority(TorrentHandle& handle, int32_t piece, int32_t priority) {
    handle.handle.piece_priority(lt::piece_index_t(piece), lt::download_priority_t(static_cast<uint8_t>(priority)));
}

rust::Vec<int32_t> handle_get_piece_priorities(TorrentHandle const& handle) {
    rust::Vec<int32_t> result;
    for (auto p : handle.handle.get_piece_priorities())
        result.push_back(static_cast<int32_t>(static_cast<uint8_t>(p)));
    return result;
}

void handle_read_piece(TorrentHandle& handle, int32_t piece) {
    handle.handle.read_piece(lt::piece_index_t(piece));
}

// ============================================================================
// TORRENT HANDLE - PEERS
// ============================================================================

rust::Vec<PeerInfo> handle_get_peers(TorrentHandle const& handle) {
    rust::Vec<PeerInfo> result;
    std::vector<lt::peer_info> peers;
    handle.handle.get_peer_info(peers);
    
    for (const auto& p : peers) {
        PeerInfo info;
        info.ip = rust::String(p.ip.address().to_string());
        info.port = p.ip.port();
        info.client = rust::String(p.client);
        info.download_rate = p.down_speed;
        info.upload_rate = p.up_speed;
        info.total_download = p.total_download;
        info.total_upload = p.total_upload;
        info.progress = p.progress;
        info.is_seed = bool(p.flags & lt::peer_info::seed);
        info.is_connecting = bool(p.flags & lt::peer_info::connecting);
        info.is_handshake = bool(p.flags & lt::peer_info::handshake);
        // TODO: Fix connection_type cast for ABI v2 - using 0 as placeholder
        info.connection_type = 0;
        info.country = rust::String(""); // country removed in libtorrent 2.0
        result.push_back(std::move(info));
    }
    return result;
}

void handle_connect_peer(TorrentHandle& handle, rust::Str ip, uint16_t port) {
    lt::error_code ec;
    auto addr = lt::make_address(rust_str_to_std(ip), ec);
    if (ec) throw std::runtime_error("Invalid IP address");
    handle.handle.connect_peer({addr, port});
}

// ============================================================================
// TORRENT HANDLE - TRACKERS
// ============================================================================

rust::Vec<TrackerInfo> handle_get_trackers(TorrentHandle const& handle) {
    rust::Vec<TrackerInfo> result;
    for (const auto& ae : handle.handle.trackers()) {
        TrackerInfo info;
        info.url = rust::String(ae.url);
        info.tier = static_cast<int32_t>(ae.tier);
        info.status = 1; // enabled
        info.message = rust::String("");
        info.num_peers = 0;
        info.scrape_incomplete = -1;
        info.scrape_complete = -1;
        info.next_announce_seconds = 0;
        
        // Get info from endpoints if available
        for (const auto& ep : ae.endpoints) {
            // In libtorrent 2.0, info_hashes is indexed by protocol_version
            const auto& ti = ep.info_hashes[lt::protocol_version::V1];
            if (ti.scrape_complete >= 0 || ti.scrape_incomplete >= 0) {
                info.num_peers = ti.scrape_incomplete + ti.scrape_complete;
                info.scrape_incomplete = ti.scrape_incomplete;
                info.scrape_complete = ti.scrape_complete;
                info.message = rust::String(ti.message);
                break;
            }
        }
        
        result.push_back(std::move(info));
    }
    return result;
}

void handle_add_tracker(TorrentHandle& handle, rust::Str url, int32_t tier) {
    lt::announce_entry ae(rust_str_to_std(url));
    ae.tier = static_cast<uint8_t>(tier);
    handle.handle.add_tracker(ae);
}

void handle_remove_tracker(TorrentHandle& handle, rust::Str url) {
    auto trackers = handle.handle.trackers();
    trackers.erase(
        std::remove_if(trackers.begin(), trackers.end(),
            [&](const lt::announce_entry& ae) { return ae.url == rust_str_to_std(url); }),
        trackers.end());
    handle.handle.replace_trackers(trackers);
}

void handle_replace_trackers(TorrentHandle& handle, rust::Slice<const rust::String> urls) {
    std::vector<lt::announce_entry> trackers;
    for (const auto& url : urls)
        trackers.emplace_back(std::string(url.data(), url.size()));
    handle.handle.replace_trackers(trackers);
}

// ============================================================================
// TORRENT HANDLE - SAVE/RESUME
// ============================================================================

void handle_save_resume_data(TorrentHandle& handle) {
    handle.handle.save_resume_data();
}

bool handle_need_save_resume_data(TorrentHandle const& handle) {
    return handle.handle.need_save_resume_data();
}

// ============================================================================
// UTILITIES
// ============================================================================

AddTorrentParams parse_magnet_uri(rust::Str uri) {
    lt::add_torrent_params p;
    lt::error_code ec;
    lt::parse_magnet_uri(rust_str_to_std(uri), p, ec);
    if (ec) throw std::runtime_error("Failed to parse magnet: " + ec.message());
    
    AddTorrentParams result;
    result.magnet_uri = rust::String(rust_str_to_std(uri));
    result.name = rust::String(p.name);
    for (const auto& t : p.trackers)
        result.trackers.push_back(rust::String(t));
    return result;
}

rust::Vec<uint8_t> handle_get_metadata(TorrentHandle const& handle) {
    rust::Vec<uint8_t> result;
    auto ti = handle.handle.torrent_file();
    if (ti) {
        lt::create_torrent ct(*ti);
        std::vector<char> buf;
        lt::bencode(std::back_inserter(buf), ct.generate());
        for (char c : buf) result.push_back(static_cast<uint8_t>(c));
    }
    return result;
}

rust::String make_magnet_uri(TorrentHandle const& handle) {
    return rust::String(lt::make_magnet_uri(handle.handle));
}

rust::String libtorrent_version() {
    return rust::String(lt::version());
}

} // namespace libtorrent_wrapper
