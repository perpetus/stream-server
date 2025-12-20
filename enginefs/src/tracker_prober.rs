use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use url::Url;

const UDP_CONNECT_TIMEOUT: Duration = Duration::from_secs(1);
const HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);

pub struct TrackerProber;

impl TrackerProber {
    pub async fn rank_trackers(trackers: Vec<String>) -> Vec<String> {
        let mut results = Vec::new();

        let mut handles = Vec::new();
        for tracker in trackers {
            handles.push(tokio::spawn(async move {
                let rtt = Self::probe(&tracker).await;
                (tracker, rtt)
            }));
        }

        for handle in handles {
            if let Ok((tracker, rtt)) = handle.await {
                if let Some(duration) = rtt {
                    results.push((tracker, duration));
                } else {
                    // If probe failed, push to end with max duration
                    results.push((tracker, Duration::from_secs(3600)));
                }
            }
        }

        // Sort by RTT
        results.sort_by_key(|k| k.1);

        // Return just the URLs
        results.into_iter().map(|(url, _)| url).collect()
    }

    async fn probe(url_str: &str) -> Option<Duration> {
        let url = match Url::parse(url_str) {
            Ok(u) => u,
            Err(_) => return None,
        };

        match url.scheme() {
            "http" | "https" => Self::probe_http(&url).await,
            "udp" => Self::probe_udp(&url).await,
            _ => None,
        }
    }

    async fn probe_http(url: &Url) -> Option<Duration> {
        let client = reqwest::Client::builder()
            .timeout(HTTP_CONNECT_TIMEOUT)
            .build()
            .ok()?;

        let start = Instant::now();
        // Just try to fetch the root or scrape - HEAD might be enough to verify connectivity
        // Many trackers return 400 or similar on root, but if we get a response, it's alive.
        let result = client.head(url.clone()).send().await;

        match result {
            Ok(_) => Some(start.elapsed()),
            Err(_) => {
                // Try GET if HEAD fails
                match client.get(url.clone()).send().await {
                    Ok(_) => Some(start.elapsed()),
                    Err(_) => None,
                }
            }
        }
    }

    async fn probe_udp(url: &Url) -> Option<Duration> {
        let host = url.host_str()?;
        let port = url.port().unwrap_or(80);
        let addr = format!("{}:{}", host, port);

        // Resolve address first
        let socket = UdpSocket::bind("0.0.0.0:0").await.ok()?;
        if socket.connect(&addr).await.is_err() {
            return None;
        }

        // UDP Tracker Protocol - Connect Request
        // Offset  Size    Name            Value
        // 0       64      protocol_id     0x41727101980 // magic constant
        // 8       32      action          0 // connect
        // 12      32      transaction_id

        let protocol_id: u64 = 0x41727101980;
        let action: u32 = 0;
        let transaction_id: u32 = 12345; // simplified

        let mut buf = [0u8; 16];
        buf[0..8].copy_from_slice(&protocol_id.to_be_bytes());
        buf[8..12].copy_from_slice(&action.to_be_bytes());
        buf[12..16].copy_from_slice(&transaction_id.to_be_bytes());

        let start = Instant::now();
        if socket.send(&buf).await.is_err() {
            return None;
        }

        let mut recv_buf = [0u8; 16];
        let timeout = tokio::time::timeout(UDP_CONNECT_TIMEOUT, socket.recv(&mut recv_buf));

        match timeout.await {
            Ok(Ok(n)) if n >= 8 => {
                // Check action (0) and transaction_id (12345)
                let recv_action = u32::from_be_bytes(recv_buf[0..4].try_into().unwrap());
                let recv_trans_id = u32::from_be_bytes(recv_buf[4..8].try_into().unwrap());

                if recv_action == 0 && recv_trans_id == transaction_id {
                    Some(start.elapsed())
                } else {
                    None
                }
            }
            _ => None,
        }
    }
}
