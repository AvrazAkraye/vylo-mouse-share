//! LAN peer discovery via mDNS (`_vylo-share._tcp`). Multicast on the
//! local network only — nothing leaves the LAN. Used purely to populate
//! the pairing screen; discovery grants no trust (that's what pairing
//! is for).

use lan_mouse_ipc::DiscoveredPeer;
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use std::collections::HashMap;
use tokio::sync::mpsc::UnboundedSender;

const SERVICE_TYPE: &str = "_vylo-share._tcp.local.";
/// TXT key carrying a fingerprint prefix, used to filter out ourselves
const FP_PROP: &str = "fp";

pub(crate) struct Discovery {
    daemon: ServiceDaemon,
    fullname: String,
}

impl Discovery {
    /// Register this device and start browsing. Discovered peers are
    /// pushed to `peers_tx` as a full replacement list on every change.
    pub(crate) fn new(
        device_name: &str,
        sync_port: u16,
        fingerprint: &str,
        peers_tx: UnboundedSender<Vec<DiscoveredPeer>>,
    ) -> Result<Self, mdns_sd::Error> {
        let daemon = ServiceDaemon::new()?;
        let fp_prefix = fp_prefix(fingerprint);

        let hostname = format!(
            "{}.local.",
            gethostname::gethostname()
                .to_string_lossy()
                .trim_end_matches(".local")
        );
        let props = [(FP_PROP, fp_prefix.as_str())];
        let info = ServiceInfo::new(
            SERVICE_TYPE,
            device_name,
            &hostname,
            "",
            sync_port,
            &props[..],
        )?
        .enable_addr_auto();
        let fullname = info.get_fullname().to_string();
        daemon.register(info)?;

        let receiver = daemon.browse(SERVICE_TYPE)?;
        {
            let fp_prefix = fp_prefix.clone();
            std::thread::Builder::new()
                .name("vylo-mdns".into())
                .spawn(move || {
                    let mut peers: HashMap<String, DiscoveredPeer> = HashMap::new();
                    while let Ok(event) = receiver.recv() {
                        match event {
                            ServiceEvent::ServiceResolved(info) => {
                                // skip our own advertisement
                                if info
                                    .get_property_val_str(FP_PROP)
                                    .is_some_and(|fp| fp == fp_prefix)
                                {
                                    continue;
                                }
                                let name = info
                                    .get_fullname()
                                    .split(&format!(".{SERVICE_TYPE}"))
                                    .next()
                                    .unwrap_or(info.get_fullname())
                                    .to_string();
                                let peer = DiscoveredPeer {
                                    name,
                                    addrs: info.get_addresses().iter().copied().collect(),
                                    port: info.get_port(),
                                };
                                peers.insert(info.get_fullname().to_string(), peer);
                                if peers_tx.send(peers.values().cloned().collect()).is_err() {
                                    return;
                                }
                            }
                            ServiceEvent::ServiceRemoved(_, fullname) => {
                                if peers.remove(&fullname).is_some()
                                    && peers_tx.send(peers.values().cloned().collect()).is_err()
                                {
                                    return;
                                }
                            }
                            _ => (),
                        }
                    }
                })
                .expect("failed to spawn mdns thread");
        }

        Ok(Self { daemon, fullname })
    }

    pub(crate) fn shutdown(&self) {
        let _ = self.daemon.unregister(&self.fullname);
        let _ = self.daemon.shutdown();
    }
}

fn fp_prefix(fingerprint: &str) -> String {
    fingerprint
        .chars()
        .filter(|c| c.is_ascii_hexdigit())
        .take(12)
        .collect()
}
