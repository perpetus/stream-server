#[test]
fn starts_and_stops_embedded_server() -> anyhow::Result<()> {
    let config_dir = tempfile::tempdir()?;
    let cache_dir = tempfile::tempdir()?;

    let handle = stream_server::start(stream_server::ServerConfig {
        config_dir: Some(config_dir.path().join("config")),
        cache_dir: Some(cache_dir.path().join("cache")),
        ..stream_server::ServerConfig::default()
    })?;

    let response = reqwest::blocking::get(format!("http://{}/heartbeat", handle.http_addr()))?
        .error_for_status()?;
    let body: serde_json::Value = response.json()?;
    assert_eq!(body["success"], true);

    handle.shutdown()?;
    assert_eq!(handle.join()?, Some(stream_server::ShutdownSource::External));

    Ok(())
}
