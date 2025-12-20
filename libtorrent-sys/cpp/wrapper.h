#pragma once

#include <memory>
#include <string>
#include <vector>
#include <cstdint>

#include <libtorrent/session.hpp>
#include <libtorrent/torrent_handle.hpp>
#include <libtorrent/torrent_status.hpp>

#include "rust/cxx.h"

namespace libtorrent_wrapper {

// ============================================================================
// STRUCT FORWARD DECLARATIONS (defined by cxx in lib.rs.h)
// ============================================================================
struct SessionSettings;
struct AddTorrentParams;
struct TorrentStatus;
struct FileInfo;
struct PeerInfo;
struct TrackerInfo;
struct AlertInfo;
struct DhtStats;
struct SessionStats;

// ============================================================================
// OPAQUE WRAPPER CLASSES
// ============================================================================

class Session {
public:
    lt::session session;
    Session(lt::session_params params) : session(std::move(params)) {}
};

class TorrentHandle {
public:
    lt::torrent_handle handle;
    TorrentHandle(lt::torrent_handle h) : handle(std::move(h)) {}
};

// ============================================================================
// SESSION FUNCTIONS
// ============================================================================

// Lifecycle
std::unique_ptr<Session> create_session(SessionSettings const& settings);
void session_abort(Session& session);
void session_pause(Session& session);
void session_resume(Session& session);
bool session_is_paused(Session const& session);

// Settings
void session_apply_settings(Session& session, SessionSettings const& settings);
int32_t session_get_download_rate_limit(Session const& session);
int32_t session_get_upload_rate_limit(Session const& session);
void session_set_download_rate_limit(Session& session, int32_t limit);
void session_set_upload_rate_limit(Session& session, int32_t limit);

// Torrents
std::unique_ptr<TorrentHandle> session_add_torrent(Session& session, AddTorrentParams const& params);
std::unique_ptr<TorrentHandle> session_add_magnet(Session& session, rust::Str magnet_uri, rust::Str save_path);
void session_remove_torrent(Session& session, TorrentHandle const& handle, bool delete_files);
rust::Vec<TorrentStatus> session_get_torrents(Session const& session);
std::unique_ptr<TorrentHandle> session_find_torrent(Session const& session, rust::Str info_hash);
TorrentStatus session_get_torrent_status(Session const& session, rust::Str info_hash);

// Stats
SessionStats session_get_stats(Session const& session);
DhtStats session_get_dht_stats(Session const& session);

// DHT
bool session_is_dht_running(Session const& session);
void session_add_dht_node(Session& session, rust::Str host, int32_t port);
void session_dht_get_peers(Session& session, rust::Str info_hash);

// Alerts
rust::Vec<AlertInfo> session_pop_alerts(Session& session);
bool session_wait_for_alert(Session const& session, int32_t timeout_ms);
void session_set_alert_mask(Session& session, uint32_t mask);

// State
rust::Vec<uint8_t> session_save_state(Session const& session);
void session_load_state(Session& session, rust::Slice<const uint8_t> state);
rust::Vec<uint8_t> session_save_dht_state(Session const& session);
void session_load_dht_state(Session& session, rust::Slice<const uint8_t> state);

// ============================================================================
// TORRENT HANDLE FUNCTIONS
// ============================================================================

// Basic info
bool handle_is_valid(TorrentHandle const& handle);
rust::String handle_get_info_hash(TorrentHandle const& handle);
std::unique_ptr<TorrentHandle> handle_clone(TorrentHandle const& handle);
rust::String handle_get_info_hash_v2(TorrentHandle const& handle);
rust::String handle_get_name(TorrentHandle const& handle);
TorrentStatus handle_get_status(TorrentHandle const& handle);

// Control
void handle_pause(TorrentHandle& handle);
void handle_resume(TorrentHandle& handle);
void handle_set_upload_limit(TorrentHandle& handle, int32_t limit);
void handle_set_download_limit(TorrentHandle& handle, int32_t limit);
int32_t handle_get_upload_limit(TorrentHandle const& handle);
int32_t handle_get_download_limit(TorrentHandle const& handle);
void handle_force_recheck(TorrentHandle& handle);
void handle_force_reannounce(TorrentHandle& handle);
void handle_force_dht_announce(TorrentHandle& handle);

// Sequential/Streaming
void handle_set_sequential_download(TorrentHandle& handle, bool enable);
bool handle_is_sequential_download(TorrentHandle const& handle);
void handle_set_piece_deadline(TorrentHandle& handle, int32_t piece, int32_t deadline_ms);
void handle_reset_piece_deadline(TorrentHandle& handle, int32_t piece);
void handle_clear_piece_deadlines(TorrentHandle& handle);

// Files
rust::Vec<FileInfo> handle_get_files(TorrentHandle const& handle);
rust::Vec<int32_t> handle_get_file_priorities(TorrentHandle const& handle);
void handle_set_file_priority(TorrentHandle& handle, int32_t index, int32_t priority);
void handle_set_file_priorities(TorrentHandle& handle, rust::Slice<const int32_t> priorities);
void handle_rename_file(TorrentHandle& handle, int32_t index, rust::Str new_name);
void handle_move_storage(TorrentHandle& handle, rust::Str new_path);

// Pieces
int32_t handle_num_pieces(TorrentHandle const& handle);
int32_t handle_piece_length(TorrentHandle const& handle);
bool handle_have_piece(TorrentHandle const& handle, int32_t piece);
rust::Vec<int32_t> handle_get_piece_availability(TorrentHandle const& handle);
void handle_set_piece_priority(TorrentHandle& handle, int32_t piece, int32_t priority);
rust::Vec<int32_t> handle_get_piece_priorities(TorrentHandle const& handle);
void handle_read_piece(TorrentHandle& handle, int32_t piece);

// Peers
rust::Vec<PeerInfo> handle_get_peers(TorrentHandle const& handle);
void handle_connect_peer(TorrentHandle& handle, rust::Str ip, uint16_t port);

// Trackers
rust::Vec<TrackerInfo> handle_get_trackers(TorrentHandle const& handle);
void handle_add_tracker(TorrentHandle& handle, rust::Str url, int32_t tier);
void handle_remove_tracker(TorrentHandle& handle, rust::Str url);
void handle_replace_trackers(TorrentHandle& handle, rust::Slice<const rust::String> urls);

// Save/Resume
void handle_save_resume_data(TorrentHandle& handle);
bool handle_need_save_resume_data(TorrentHandle const& handle);

// ============================================================================
// UTILITIES
// ============================================================================

AddTorrentParams parse_magnet_uri(rust::Str uri);
rust::String make_magnet_uri(TorrentHandle const& handle);
rust::String libtorrent_version();

} // namespace libtorrent_wrapper
