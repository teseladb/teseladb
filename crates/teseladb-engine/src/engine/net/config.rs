//! Network device configuration for the native stack.
//!
//! [`parse_config`] reads a YAML document that maps device names to per-device settings and
//! returns one [`DeviceConfig`] per device: its IPv4 settings ([`Ipv4Config`]) and its
//! hardware settings ([`HwConfig`]). Only the subset of YAML such a file needs is supported
//! (a top-level mapping whose values are mappings of scalars, in block or flow style), and it
//! is parsed in this module with no external dependency.
//!
//! The configuration is read once at start-up, before the shards start their stacks, so
//! nothing here is on a hot path.

use std::collections::HashMap;
use std::fmt;
use std::io::Read;

use crate::engine::core::future::ExceptionPtr;

/// The IPv4 configuration of a device.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Ipv4Config {
    /// Static address.
    pub ip: String,
    /// Static netmask.
    pub netmask: String,
    /// Static gateway.
    pub gateway: String,
    /// Use DHCP.
    pub dhcp: bool,
}

/// The hardware configuration of a device.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HwConfig {
    /// PCI address of the NIC.
    pub pci_address: String,
    /// DPDK port index.
    pub port_index: Option<u32>,
    /// Large receive offload.
    pub lro: bool,
    /// TCP segmentation offload.
    pub tso: bool,
    /// UDP fragmentation offload.
    pub ufo: bool,
    /// Hardware flow control.
    pub hw_fc: bool,
    /// Virtio event index.
    pub event_index: bool,
    /// Checksum offload.
    pub csum_offload: bool,
    /// Ring size.
    pub ring_size: Option<u32>,
}

impl Default for HwConfig {
    fn default() -> Self {
        HwConfig {
            pci_address: String::new(),
            port_index: None,
            lro: true,
            tso: true,
            ufo: true,
            hw_fc: true,
            event_index: true,
            csum_offload: true,
            ring_size: None,
        }
    }
}

/// The configuration of one device.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeviceConfig {
    /// IPv4 configuration.
    pub ip_cfg: Ipv4Config,
    /// Hardware configuration.
    pub hw_cfg: HwConfig,
}

/// An invalid network configuration; carries a human-readable message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigException(pub String);

impl ConfigException {
    /// An exception with the given message.
    pub fn new(msg: &str) -> Self {
        ConfigException(msg.to_string())
    }
}

impl fmt::Display for ConfigException {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ConfigException {}

/// The keys a device configuration may use.
pub const CONFIG_KEYS: [&str; 13] = [
    "pci-address",
    "port-index",
    "ip",
    "gateway",
    "netmask",
    "dhcp",
    "lro",
    "tso",
    "ufo",
    "hw-fc",
    "event-index",
    "csum-offload",
    "ring-size",
];

/// Parses the network configuration in `input` into one [`DeviceConfig`] per device name.
///
/// # Errors
///
/// Returns a [`ConfigException`] (wrapped in an [`ExceptionPtr`]) when `input` cannot be read
/// or is not valid YAML of the supported subset, when a device uses a key outside
/// [`CONFIG_KEYS`] or a value of the wrong type, when `port-index` and `pci-address` are both
/// used, or when the `dhcp`/static address combination is inconsistent.
pub fn parse_config(input: &mut dyn Read) -> Result<HashMap<String, DeviceConfig>, ExceptionPtr> {
    let mut device_configs: HashMap<String, DeviceConfig> = HashMap::new();

    let mut text = String::new();
    input
        .read_to_string(&mut text)
        .map_err(|e| ExceptionPtr::from(ConfigException::new(&e.to_string())))?;
    let doc = yaml::load(&text).map_err(|e| ExceptionPtr::from(ConfigException::new(&e)))?;
    for (key, value) in doc {
        device_configs.insert(key, decode_device_config(&value)?);
    }

    let mut port_index_used = false;
    let mut pci_address_used = false;

    for item in device_configs.values() {
        if item.hw_cfg.port_index.is_some() {
            port_index_used = true;
        }

        if !item.hw_cfg.pci_address.is_empty() {
            pci_address_used = true;
        }

        if port_index_used && pci_address_used {
            return Err(ExceptionPtr::from(ConfigException::new(
                "port_index and pci_address cannot be used together",
            )));
        }
    }

    // check each device: when dhcp is off, all of ip, gw, nm must be specified
    if device_configs.values().any(|p| {
        !p.ip_cfg.dhcp
            && (p.ip_cfg.ip.is_empty() || p.ip_cfg.gateway.is_empty() || p.ip_cfg.netmask.is_empty())
    }) {
        return Err(ExceptionPtr::from(ConfigException::new(
            "when dhcp is off then all of ip, gateway, netmask has to be specified",
        )));
    }

    // check each device: dhcp cannot be used together with static ip/gw/nm
    if device_configs.values().any(|p| {
        p.ip_cfg.dhcp
            && (!p.ip_cfg.ip.is_empty() || !p.ip_cfg.gateway.is_empty() || !p.ip_cfg.netmask.is_empty())
    }) {
        return Err(ExceptionPtr::from(ConfigException::new(
            "dhcp and ip cannot be used together",
        )));
    }
    Ok(device_configs)
}

/// Builds a [`DeviceConfig`] from the mapping of one device, rejecting unknown keys.
fn decode_device_config(node: &[(String, String)]) -> Result<DeviceConfig, ExceptionPtr> {
    let mut dev_cfg = DeviceConfig::default();
    // test for unsupported key

    for (k, _) in node {
        if !CONFIG_KEYS.iter().any(|s| s == k) {
            return Err(ExceptionPtr::from(ConfigException::new(&format!("unsuppoted key {}", k))));
        }
    }

    let get = |k: &str| node.iter().rev().find(|(key, _)| key == k).map(|(_, v)| v.as_str());

    if let Some(v) = get("pci-address") {
        dev_cfg.hw_cfg.pci_address = v.to_string();
    }

    if let Some(v) = get("port-index") {
        dev_cfg.hw_cfg.port_index = Some(yaml::as_unsigned(v)?);
    }

    if let Some(v) = get("lro") {
        dev_cfg.hw_cfg.lro = yaml::as_bool(v)?;
    }

    if let Some(v) = get("tso") {
        dev_cfg.hw_cfg.tso = yaml::as_bool(v)?;
    }

    if let Some(v) = get("ufo") {
        dev_cfg.hw_cfg.ufo = yaml::as_bool(v)?;
    }

    if let Some(v) = get("hw-fc") {
        dev_cfg.hw_cfg.hw_fc = yaml::as_bool(v)?;
    }

    if let Some(v) = get("event-index") {
        dev_cfg.hw_cfg.event_index = yaml::as_bool(v)?;
    }

    if let Some(v) = get("ring-size") {
        dev_cfg.hw_cfg.ring_size = Some(yaml::as_unsigned(v)?);
    }

    if let Some(v) = get("ip") {
        dev_cfg.ip_cfg.ip = v.to_string();
    }

    if let Some(v) = get("netmask") {
        dev_cfg.ip_cfg.netmask = v.to_string();
    }

    if let Some(v) = get("gateway") {
        dev_cfg.ip_cfg.gateway = v.to_string();
    }

    if let Some(v) = get("dhcp") {
        dev_cfg.ip_cfg.dhcp = yaml::as_bool(v)?;
    }

    Ok(dev_cfg)
}

/// The YAML subset of a network configuration: a top-level mapping whose values are
/// mappings of scalars, each mapping in block style (indentation) or flow style (`{}`).
mod yaml {
    use super::ConfigException;
    use crate::engine::core::future::ExceptionPtr;

    /// A mapping of scalars, in document order.
    pub type Map = Vec<(String, String)>;

    fn strip_comment(line: &str) -> &str {
        let mut in_quote: Option<char> = None;
        for (i, c) in line.char_indices() {
            match (in_quote, c) {
                (None, '"') | (None, '\'') => in_quote = Some(c),
                (Some(q), c) if c == q => in_quote = None,
                (None, '#') if i == 0 || line[..i].ends_with(char::is_whitespace) => {
                    return &line[..i];
                }
                _ => {}
            }
        }
        line
    }

    fn unquote(s: &str) -> String {
        let s = s.trim();
        if s.len() >= 2
            && ((s.starts_with('"') && s.ends_with('"'))
                || (s.starts_with('\'') && s.ends_with('\'')))
        {
            s[1..s.len() - 1].to_string()
        } else {
            s.to_string()
        }
    }

    /// Splits `key: value` at the first `": "` (or a trailing `:`), as YAML does for plain
    /// scalars such as PCI addresses that contain colons.
    fn split_kv(s: &str) -> Option<(String, String)> {
        let s = s.trim();
        if let Some(i) = s.find(": ") {
            return Some((unquote(&s[..i]), unquote(&s[i + 2..])));
        }
        s.strip_suffix(':').map(|k| (unquote(k), String::new()))
    }

    fn parse_flow_map(s: &str) -> Result<Map, String> {
        let s = s.trim();
        let inner = s
            .strip_prefix('{')
            .and_then(|r| r.strip_suffix('}'))
            .ok_or_else(|| format!("invalid flow mapping: {}", s))?;
        let mut out = Map::new();
        for part in inner.split(',') {
            if part.trim().is_empty() {
                continue;
            }
            let (k, v) =
                split_kv(part).ok_or_else(|| format!("invalid mapping entry: {}", part))?;
            out.push((k, v));
        }
        Ok(out)
    }

    /// Parses the document.
    pub fn load(text: &str) -> Result<Vec<(String, Map)>, String> {
        let lines: Vec<&str> = text
            .lines()
            .map(strip_comment)
            .filter(|l| !l.trim().is_empty() && l.trim() != "---")
            .collect();
        let trimmed_all = lines.iter().map(|l| l.trim()).collect::<Vec<_>>().join(" ");
        if trimmed_all.starts_with('{') {
            // a flow mapping of flow mappings
            let inner = trimmed_all
                .strip_prefix('{')
                .and_then(|r| r.strip_suffix('}'))
                .ok_or_else(|| "invalid flow mapping".to_string())?;
            let mut out = Vec::new();
            let mut rest = inner.trim();
            while !rest.is_empty() {
                let colon = rest.find(':').ok_or_else(|| "invalid mapping".to_string())?;
                let key = unquote(&rest[..colon]);
                let after = rest[colon + 1..].trim_start();
                let close =
                    after.find('}').ok_or_else(|| "unterminated flow mapping".to_string())?;
                out.push((key, parse_flow_map(&after[..=close])?));
                rest = after[close + 1..].trim_start().trim_start_matches(',').trim_start();
            }
            return Ok(out);
        }
        let mut out: Vec<(String, Map)> = Vec::new();
        for line in lines {
            let indent = line.len() - line.trim_start().len();
            let content = line.trim();
            if indent == 0 {
                let colon =
                    content.find(':').ok_or_else(|| format!("invalid line: {}", content))?;
                let key = unquote(&content[..colon]);
                let value = content[colon + 1..].trim();
                let map = if value.is_empty() { Map::new() } else { parse_flow_map(value)? };
                out.push((key, map));
            } else {
                let Some(last) = out.last_mut() else {
                    return Err(format!("unexpected indentation: {}", content));
                };
                let (k, v) = split_kv(content)
                    .ok_or_else(|| format!("invalid mapping entry: {}", content))?;
                last.1.push((k, v));
            }
        }
        Ok(out)
    }

    /// Converts a YAML scalar to `bool` (the `yes`/`no`, `true`/`false`, `on`/`off` forms).
    ///
    /// # Errors
    ///
    /// A [`ConfigException`] when `v` is not one of the boolean forms.
    pub fn as_bool(v: &str) -> Result<bool, ExceptionPtr> {
        match v {
            "y" | "Y" | "yes" | "Yes" | "YES" | "true" | "True" | "TRUE" | "on" | "On" | "ON" => {
                Ok(true)
            }
            "n" | "N" | "no" | "No" | "NO" | "false" | "False" | "FALSE" | "off" | "Off"
            | "OFF" => Ok(false),
            _ => Err(ExceptionPtr::from(ConfigException::new("bad conversion"))),
        }
    }

    /// Converts a YAML scalar to `u32` (decimal, `0x` hexadecimal or `0o` octal).
    ///
    /// # Errors
    ///
    /// A [`ConfigException`] when `v` is not a number that fits in `u32`.
    pub fn as_unsigned(v: &str) -> Result<u32, ExceptionPtr> {
        let r = if let Some(h) = v.strip_prefix("0x") {
            u32::from_str_radix(h, 16)
        } else if let Some(o) = v.strip_prefix("0o") {
            u32::from_str_radix(o, 8)
        } else {
            v.parse::<u32>()
        };
        r.map_err(|_| ExceptionPtr::from(ConfigException::new("bad conversion")))
    }
}
