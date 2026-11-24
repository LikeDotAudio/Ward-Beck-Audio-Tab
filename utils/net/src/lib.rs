// Copyright (c) 2026 Ward-Beck Systems (Ward-Beck.ca). All rights reserved.
//! Network helpers shared by the MXL engine, the AES67 transmitters and the AoIP listener:
//! this host's address and the RFC 7273 reference clock line of an SDP.

use std::net::Ipv4Addr;
use std::sync::OnceLock;

/// Primary IPv4 of this host (override with WBS_MXL_IP); falls back to 127.0.0.1.
pub fn wbs_host_ipv4() -> String {
    if let Ok(v) = std::env::var("WBS_MXL_IP") {
        let t = v.trim();
        if !t.is_empty() {
            return t.to_string();
        }
    }
    std::net::UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| {
            s.connect("10.255.255.255:1")?; // no packets are sent for UDP connect
            s.local_addr()
        })
        .map(|a| a.ip().to_string())
        .unwrap_or_else(|_| "127.0.0.1".to_string())
}

/// Host IPv4 resolved once (Python: WBS_HOST_IP at import).
pub fn wbs_host_ip() -> &'static str {
    static IP: OnceLock<String> = OnceLock::new();
    IP.get_or_init(wbs_host_ipv4)
}

/// This host's IPv4 address as an Ipv4Addr (loopback when there is none).
pub fn host_v4() -> Ipv4Addr {
    wbs_host_ip().parse().unwrap_or(Ipv4Addr::LOCALHOST)
}

/// RFC 7273 reference clock: the PTP leader (grandmaster identity and domain) when one
/// is heard, else "traceable".
pub fn refclk(leader: Option<(String, i64)>) -> String {
    match leader {
        Some((gm, domain)) => format!("ptp=IEEE1588-2008:{}:{domain}", gm.to_uppercase().replace(':', "-")),
        None => "ptp=IEEE1588-2008:traceable".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refclk_names_the_leader() {
        assert_eq!(refclk(Some(("00:07:f5:ff:fe:42:06:a2".into(), 44))), "ptp=IEEE1588-2008:00-07-F5-FF-FE-42-06-A2:44");
        assert_eq!(refclk(None), "ptp=IEEE1588-2008:traceable");
    }
}
