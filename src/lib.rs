pub mod rules;
use anyhow::{Result, bail, ensure};
use prost::Message;
use std::net::{Ipv4Addr, Ipv6Addr};

#[derive(Clone, PartialEq, Message)]
pub struct GeoIpList {
    #[prost(message, repeated, tag = "1")]
    pub entry: Vec<GeoIp>,
}

#[derive(Clone, PartialEq, Message)]
pub struct GeoIp {
    #[prost(string, tag = "1")]
    pub country_code: String,
    #[prost(message, repeated, tag = "2")]
    pub cidr: Vec<Cidr>,
    #[prost(bool, tag = "3")]
    pub inverse_match: bool,
}

#[derive(Clone, PartialEq, Message)]
pub struct Cidr {
    #[prost(bytes = "vec", tag = "1")]
    pub ip: Vec<u8>,
    #[prost(uint32, tag = "2")]
    pub prefix: u32,
}

/// IPv4 and IPv6 use separate vectors; addresses are unsigned network-order integers.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct Network {
    pub address: u128,
    pub prefix: u8,
}

fn mask(bits: u8, prefix: u8) -> u128 {
    if prefix == 0 {
        0
    } else {
        u128::MAX << (bits - prefix)
    }
}

impl Network {
    pub fn text(self, bits: u8) -> String {
        if bits == 32 {
            format!("{}/{}", Ipv4Addr::from(self.address as u32), self.prefix)
        } else {
            format!("{}/{}", Ipv6Addr::from(self.address), self.prefix)
        }
    }
}

pub fn extract(bytes: &[u8], code: &str) -> Result<(Vec<Network>, Vec<Network>)> {
    let list = GeoIpList::decode(bytes)?;
    extract_entry(&list, code)
}

pub fn extract_entry(list: &GeoIpList, code: &str) -> Result<(Vec<Network>, Vec<Network>)> {
    let (mut v4, mut v6) = (Vec::new(), Vec::new());
    let mut found = false;
    for entry in list
        .entry
        .iter()
        .filter(|e| e.country_code.eq_ignore_ascii_case(code))
    {
        found = true;
        ensure!(!entry.inverse_match, "{code}: inverse_match is unsupported");
        for cidr in &entry.cidr {
            let (bits, address) = match cidr.ip.len() {
                4 => (
                    32,
                    u32::from_be_bytes(cidr.ip.as_slice().try_into().unwrap()) as u128,
                ),
                16 => (
                    128,
                    u128::from_be_bytes(cidr.ip.as_slice().try_into().unwrap()),
                ),
                n => bail!("{code}: invalid IP length {n}; expected 4 or 16"),
            };
            ensure!(
                cidr.prefix <= u32::from(bits),
                "{code}: invalid IPv{version} prefix {prefix}",
                version = if bits == 32 { 4 } else { 6 },
                prefix = cidr.prefix
            );
            let prefix = cidr.prefix as u8;
            let network = Network {
                address: address & mask(bits, prefix),
                prefix,
            };
            if bits == 32 {
                v4.push(network);
            } else {
                v6.push(network);
            }
        }
    }
    ensure!(found, "country/tag '{code}' not found");
    Ok((v4, v6))
}

pub fn prepare(mut networks: Vec<Network>, bits: u8, optimize: bool) -> Vec<Network> {
    if !networks.is_sorted() {
        networks.sort_unstable();
    }
    if !optimize {
        return networks;
    }
    let mut output: Vec<Network> = Vec::with_capacity(networks.len());
    for mut current in networks {
        loop {
            let Some(previous) = output.last().copied() else {
                output.push(current);
                break;
            };
            if previous.prefix <= current.prefix
                && current.address & mask(bits, previous.prefix) == previous.address
            {
                break;
            }
            if current.prefix > 0 && previous.prefix == current.prefix {
                let parent = mask(bits, current.prefix - 1);
                if previous.address & parent == current.address & parent {
                    output.pop();
                    current = Network {
                        address: current.address & parent,
                        prefix: current.prefix - 1,
                    };
                    continue;
                }
            }
            output.push(current);
            break;
        }
    }
    output
}
