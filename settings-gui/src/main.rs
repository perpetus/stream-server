use anyhow::{Context, Result};
use reqwest::Client;
use std::sync::Arc;

fn main() -> Result<()> {
    let server_url = parse_server_url();
    let http_client = Client::builder()
        .build()
        .context("failed to create HTTP client")?;
    let connector = Arc::new(settings_gui::HttpConnector::new(http_client, server_url));
    settings_gui::run(connector)
}

fn parse_server_url() -> String {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--server-url" {
            if let Some(value) = args.next() {
                return trim_server_url(value);
            }
        } else if let Some(value) = arg.strip_prefix("--server-url=") {
            return trim_server_url(value.to_string());
        }
    }
    "http://127.0.0.1:11470".to_string()
}

fn trim_server_url(value: String) -> String {
    value.trim_end_matches('/').to_string()
}
