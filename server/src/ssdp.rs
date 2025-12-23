
use futures_util::StreamExt;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tracing::{error, info};
use ssdp_client::SearchTarget;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Device {
    pub id: String,
    pub name: String,
    pub address: String,
    pub port: u16,
    pub device_type: String,
    pub model: Option<String>,
}

pub async fn start_discovery(devices: Arc<tokio::sync::RwLock<Vec<Device>>>) {
    info!("Starting SSDP discovery...");
    let search_targets = vec![
        "urn:schemas-upnp-org:device:MediaRenderer:1",
        "urn:dial-multiscreen-org:service:dial:1", // Chromecast often responds to this
        "ssdp:all",
    ];

    loop {

        for target_str in &search_targets {
             let target = match SearchTarget::from_str(target_str) {
                 Ok(t) => t,
                 Err(e) => {
                     error!("Invalid SSDP search target {}: {}", target_str, e);
                     continue;
                 }
             };

            match ssdp_client::search(&target, Duration::from_secs(3), 0, None).await {
                Ok(mut responses) => {
                    let mut found_devices = Vec::new();
                    while let Some(response_result) = responses.next().await {
                        match response_result {
                            Ok(response) => {
                                let location = response.location();
                                let usn = response.usn(); 
                                
                                found_devices.push(Device {
                                    id: usn.to_string(),
                                    name: format!("Device ({})", response.server()), 
                                    address: location.to_string(),
                                    port: 0,
                                    device_type: response.search_target().to_string(),
                                    model: None,
                                });
                            }
                            Err(e) => {
                                error!("Error receiving SSDP response: {}", e);
                            }
                        }
                    }

                    // Update state 
                    if !found_devices.is_empty() {
                         let mut dev_lock = devices.write().await;
                         for new_dev in found_devices {
                             if !dev_lock.iter().any(|d| d.id == new_dev.id) {
                                 info!("Found new device: {} at {}", new_dev.name, new_dev.address);
                                 dev_lock.push(new_dev);
                             }
                         }
                    }
                }
                Err(e) => {
                    error!("SSDP search error for {}: {}", target_str, e);
                }
            }
        }
        
        // Scan every 30 seconds
        tokio::time::sleep(Duration::from_secs(30)).await;
    }
}
