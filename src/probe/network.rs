//! DNS answers are validated in the resolver used for the actual connection.
//! Only approved addresses reach the connector; its two-family race is budgeted.

use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use std::{
    io,
    net::{IpAddr, Ipv6Addr, ToSocketAddrs},
    sync::Arc,
};
use tokio::sync::Semaphore;

/// Conservative public-address policy. Metadata/link-local access is always denied.
#[must_use]
pub fn address_allowed(address: IpAddr, private: bool) -> bool {
    match address {
        IpAddr::V4(address) => {
            let [a, b, c, d] = address.octets();
            if a == 0 || a >= 224 || (a == 169 && b == 254) || [a, b, c, d] == [168, 63, 129, 16] {
                return false;
            }
            let local = a == 10
                || a == 127
                || (a == 172 && (16..=31).contains(&b))
                || (a == 192 && b == 168);
            if local {
                return private;
            }
            !((a == 100 && (64..=127).contains(&b))
                || (a == 192 && b == 0)
                || (a == 192 && b == 88 && c == 99)
                || (a == 198 && matches!(b, 18 | 19))
                || (a == 198 && b == 51 && c == 100)
                || (a == 203 && b == 0 && c == 113))
        }
        IpAddr::V6(address) => {
            if let Some(ipv4) = address.to_ipv4_mapped() {
                return address_allowed(IpAddr::V4(ipv4), private);
            }
            if address.is_unspecified()
                || address.is_multicast()
                || address.is_unicast_link_local()
                || address == Ipv6Addr::new(0xfd00, 0xec2, 0, 0, 0, 0, 0, 0x254)
            {
                return false;
            }
            if address.is_loopback() || (u128::from(address) >> 121) == 0x7e {
                return private;
            }
            let prefix = u128::from(address) >> 96;
            (u128::from(address) >> 125) == 1
                && prefix != 0x2001_0db8
                && (u128::from(address) >> 112) != 0x2002
                && (u128::from(address) >> 105) != (0x2001_u128 << 7)
        }
    }
}

pub(super) struct Resolver {
    pub private: bool,
    pub slots: Arc<Semaphore>,
}

impl Resolve for Resolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_owned();
        let private = self.private;
        let slots = self.slots.clone();
        Box::pin(async move {
            let permit = slots.acquire_owned().await?;
            // The permit lives inside libc resolution, even if the request times out.
            let address = tokio::task::spawn_blocking(move || {
                let _permit = permit;
                let addresses: Vec<_> = (host.as_str(), 0).to_socket_addrs()?.take(17).collect();
                if addresses.len() > 16
                    || addresses
                        .iter()
                        .any(|address| !address_allowed(address.ip(), private))
                {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "DNS address rejected by local policy",
                    ));
                }
                if addresses.is_empty() {
                    return Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        "DNS returned no usable address",
                    ));
                }
                Ok(addresses)
            })
            .await??;
            Ok(Box::new(address.into_iter()) as Addrs)
        })
    }
}
