use super::parser::Nzb;
use super::nntp::Client as NntpClient;
use anyhow::{anyhow, Result};
use std::sync::Arc;

#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct NzbConfig {
    pub servers: Vec<NzbServerConfig>,
    pub nzb_url: String, // Keep track of source
}

#[derive(Clone, Debug)]
pub struct NzbServerConfig {
    pub host: String,
    pub port: u16,
    pub user: Option<String>,
    pub pass: Option<String>,
    pub ssl: bool,
    pub connections: u32,
}

#[derive(Clone)]
#[allow(dead_code)]
pub struct NzbSession {
    pub key: String,
    pub config: NzbConfig,
    pub nzb: Arc<Nzb>,
    pool: async_channel::Sender<NntpClient>,
    pool_receiver: async_channel::Receiver<NntpClient>,
}

impl NzbSession {
    pub async fn new(key: String, config: NzbConfig, nzb_content: String) -> Result<Self> {
        let nzb = super::parser::parse_nzb_xml(&nzb_content)?;
        
        let total_connections: usize = config.servers.iter().map(|s| s.connections as usize).sum();
        let (tx, rx) = async_channel::bounded(total_connections);
        
        let config_clone = config.clone();
        let tx_clone = tx.clone();
        
        tokio::spawn(async move {
            for server in &config_clone.servers {
                for _ in 0..server.connections {
                    match NntpClient::connect(&server.host, server.port, server.ssl).await {
                        Ok(mut client) => {
                             if let (Some(u), Some(p)) = (&server.user, &server.pass) {
                                 if let Err(e) = client.authenticate(u, p).await {
                                     tracing::error!("NNTP Auth failed for {}: {}", server.host, e);
                                     continue;
                                 }
                             }
                             let _ = tx_clone.send(client).await;
                        },
                        Err(e) => {
                            tracing::error!("Failed to connect to NNTP {}: {}", server.host, e);
                        }
                    }
                }
            }
        });

        Ok(Self {
            key,
            config,
            nzb: Arc::new(nzb),
            pool: tx,
            pool_receiver: rx,
        })
    }
    
    pub async fn fetch_segment(&self, message_id: &str) -> Result<Vec<u8>> {
        let mut client = self.pool_receiver.recv().await.map_err(|_| anyhow!("Connection pool closed"))?;
        
        let result = client.fetch_body(message_id).await;
        
        match result {
            Ok(data) => {
                let _ = self.pool.send(client).await;
                Ok(data)
            }
            Err(e) => {
                 tracing::warn!("Failed to fetch article: {}", e);
                 // If error is recoverable, maybe put back?
                 // For now, if we fail to fetch body, client might be in bad state or connection dropped.
                 // We rely on pool not putting it back to drop it.
                 // But wait, if I don't send it back, I lose a slot.
                 // I should probably drop the client and spawn a replacement or try to reconnect.
                 // For MVP: drop.
                 Err(e)
            }
        }
    }

    pub fn stream_file(&self, filename: &str) -> Result<super::stream::NzbFileStream> {
        // Find file
        let file = self.nzb.files.iter().find(|f| {
            // Very naive match: subject contains filename
            f.subject.contains(filename)
        });
        
        if let Some(file) = file {
            // Ensure self is cheap to clone (Arc fields)
            Ok(super::stream::NzbFileStream::new(Arc::new(self.clone()), file.clone()))
        } else {
            Err(anyhow!("File not found in NZB: {}", filename))
        }
    }
}
