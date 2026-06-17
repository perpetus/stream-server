fn main() -> anyhow::Result<()> {
    let config_dir = tempfile::tempdir()?;
    let cache_dir = tempfile::tempdir()?;

    let handle = stream_server::start(stream_server::ServerConfig {
        config_dir: Some(config_dir.path().join("config")),
        cache_dir: Some(cache_dir.path().join("cache")),
        ..stream_server::ServerConfig::default()
    })?;

    println!("embedded server listening at http://{}", handle.http_addr());

    let response = reqwest::blocking::get(format!("http://{}/heartbeat", handle.http_addr()))?
        .error_for_status()?;
    println!("heartbeat: {}", response.text()?);

    handle.shutdown()?;
    handle.join()?;

    Ok(())
}
