use std::{
    net::IpAddr,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{anyhow, Result};
use once_cell::sync::Lazy;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
    sync::{Mutex, RwLock},
};

use crate::{config::get_config, peer::PeerInfo, zone::get_zones};

use super::RouteConfig;

pub const KEY_BGP_ENDPOINT: &str = "bgp_endpoint";
pub const KEY_BGP_ENDPOINT_PORT: &str = "bgp_endpoint_port";
pub const KEY_BGP_NEIGHBOR_AS: &str = "bgp_neighbor_as";

pub static mut BIRD_OPERATION_MUTEX: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));
pub static mut BIRD_LAST_UPDATE_TIME: RwLock<SystemTime> = RwLock::const_new(UNIX_EPOCH);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BIRDConfig {
    endpoint: IpAddr,
    endpoint_port: Option<u16>,
    neighbor_as: u32,
}

impl BIRDConfig {
    pub async fn new(peer: &PeerInfo) -> Result<BIRDConfig> {
        let conf = BIRDConfig {
            endpoint: peer
                .props
                .get(KEY_BGP_ENDPOINT)
                .ok_or(anyhow!("BGP endpoint is not available"))?
                .parse()?,
            endpoint_port: if let Some(port) = peer.props.get(KEY_BGP_ENDPOINT_PORT) {
                Some(port.parse()?)
            } else {
                None
            },
            neighbor_as: peer
                .props
                .get(KEY_BGP_NEIGHBOR_AS)
                .ok_or(anyhow!("BGP neighbor as is not available"))?
                .parse()?,
        };
        Ok(conf)
    }

    pub async fn update() -> Result<()> {
        Self::_update(0).await
    }

    pub async fn _update(delay_times: u32) -> Result<()> {
        let op_lock = unsafe { BIRD_OPERATION_MUTEX.lock().await };
        let config = {
            let bird_opts = &get_config().await?.bird;
            bird_opts
                .as_ref()
                .ok_or(anyhow!("BIRD is not enabled"))?
                .clone()
        };
        {
            let mut lines = vec!["# generated by peerd".to_string()];
            for zone in get_zones() {
                if let Some(zone_conf) = &zone.conf.bird {
                    if let Ok(peers) = zone.peers.try_lock() {
                        for peer in peers.iter() {
                            if let RouteConfig::BIRD(conf) = &peer.route {
                                lines.push(format!(
                                    "protocol bgp {} from {} {{ neighbor {} {} as {}; {} }};",
                                    zone_conf.protocol_prefix.clone() + peer.info.name.as_str(),
                                    zone_conf.bgp_template,
                                    conf.endpoint.to_string(),
                                    if let Some(port) = conf.endpoint_port {
                                        format!(" port {} ", port)
                                    } else {
                                        "".to_string()
                                    },
                                    conf.neighbor_as,
                                    if let Ok(Some(ifname)) = peer.tun.get_ifname(&peer.info).await
                                    {
                                        format!("interface '{}';", ifname)
                                    } else {
                                        "".to_string()
                                    }
                                ));
                            }
                        }
                    } else {
                        Self::request_delayed_update(delay_times);
                        return Ok(());
                    }
                }
            }
            tokio::fs::write(config.generated_conf, lines.join("\n")).await?;
        }
        if config.do_reconfigure {
            let mut stream = UnixStream::connect(config.control_sock).await?;
            stream.write_all(b"configure\n").await?;
            let mut response = String::new();
            stream.read_to_string(&mut response).await?;
            info!("BIRD re-configure response: {}", response);
        }
        drop(op_lock);
        Ok(())
    }

    pub fn request_delayed_update(delay_times: u32) {
        let time = SystemTime::now();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(500)).await;
            let last_time = unsafe { BIRD_LAST_UPDATE_TIME.read() }.await;
            if last_time.cmp(&time).is_le() {
                if delay_times > 50 {
                    warn!(
                        "A BIRD update request has been delayed for {} times",
                        delay_times
                    );
                }
                if let Err(err) = Self::_update(delay_times + 1).await {
                    error!("failed to perform delayed BIRD update: {}", err);
                }
            }
        });
    }
}