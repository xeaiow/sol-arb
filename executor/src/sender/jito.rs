use anyhow::Result;
use solana_sdk::transaction::VersionedTransaction;
use tonic::transport::Channel;

pub mod proto {
    pub mod packet {
        tonic::include_proto!("packet");
    }
    pub mod shared {
        tonic::include_proto!("shared");
    }
    pub mod bundle {
        tonic::include_proto!("bundle");
    }
    pub mod searcher {
        tonic::include_proto!("searcher");
    }
    pub mod auth {
        tonic::include_proto!("auth");
    }
}

use proto::searcher::searcher_service_client::SearcherServiceClient;

#[derive(Clone)]
pub struct JitoSender {
    pub endpoint: String,
    client: Option<SearcherServiceClient<Channel>>,
}

impl JitoSender {
    pub fn new(endpoint: String) -> Self {
        Self {
            endpoint,
            client: None,
        }
    }

    /// Pre-connect gRPC channel at startup
    pub async fn connect(&mut self) -> Result<()> {
        let channel = Channel::from_shared(self.endpoint.clone())?
            .connect()
            .await?;
        self.client = Some(SearcherServiceClient::new(channel));
        log::info!("Jito gRPC connected: {}", self.endpoint);
        Ok(())
    }

    /// Send a bundle containing a single transaction
    pub async fn send_bundle(&self, tx: &VersionedTransaction) -> Result<()> {
        let client = self
            .client
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Jito not connected"))?;

        let tx_bytes = bincode::serialize(tx)?;
        let packet = proto::packet::Packet {
            data: tx_bytes.clone(),
            meta: Some(proto::packet::Meta {
                size: tx_bytes.len() as u64,
                addr: String::new(),
                port: 0,
                flags: None,
                sender_stake: 0,
            }),
        };
        let bundle = proto::bundle::Bundle {
            header: None,
            packets: vec![packet],
        };
        let request = proto::searcher::SendBundleRequest {
            bundle: Some(bundle),
        };

        let mut client = client.clone();
        let _response = client.send_bundle(request).await?;
        log::debug!("Jito bundle sent to {}", self.endpoint);
        Ok(())
    }
}
