//! Just enough of LXD's REST API, over its Unix socket: projects, the
//! sandbox's instances, disk devices for shared folders, and the bridge's
//! address. Blocking (one short request each); call it from
//! `spawn_blocking`.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

pub const SOCKET: &str = "/var/snap/lxd/common/lxd/unix.socket";

pub struct Lxd {
    socket: String,
    project: String,
}

/// Why LXD can't be used, in words for the setup card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Access {
    Ok,
    /// No LXD socket: the lxd snap isn't installed or initialised.
    Missing,
    /// The socket is there but this account may not use it (not in the
    /// `lxd` group yet, or the group membership needs a new login).
    Denied,
}

impl Lxd {
    pub fn new(project: &str) -> Self {
        Self {
            socket: std::env::var("LXD_SOCKET").unwrap_or_else(|_| SOCKET.into()),
            project: project.into(),
        }
    }

    pub fn access(&self) -> Access {
        if !std::path::Path::new(&self.socket).exists() {
            return Access::Missing;
        }
        // Probe, and close the probe before asking: LXD/Incus may not
        // answer a second connection while an idle first one is open.
        let probe = UnixStream::connect(&self.socket).map(drop);
        match probe {
            Ok(()) => match self.raw("GET", "/1.0", None) {
                Ok((200, body)) if body["metadata"]["auth"] == "trusted" => Access::Ok,
                Ok((200, _)) => Access::Denied,
                Ok(_) => Access::Denied,
                Err(_) => Access::Missing,
            },
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => Access::Denied,
            Err(_) => Access::Missing,
        }
    }

    fn raw(&self, method: &str, path: &str, body: Option<&Value>) -> Result<(u16, Value)> {
        let mut stream = UnixStream::connect(&self.socket)
            .with_context(|| format!("connect to LXD at {}", self.socket))?;
        stream.set_read_timeout(Some(Duration::from_secs(60)))?;
        let payload = body.map(|b| b.to_string()).unwrap_or_default();
        let mut request = format!(
            "{method} {path} HTTP/1.1\r\nHost: lxd\r\nConnection: close\r\nUser-Agent: umoja-agentd\r\n"
        );
        if body.is_some() {
            request.push_str(&format!(
                "Content-Type: application/json\r\nContent-Length: {}\r\n",
                payload.len()
            ));
        }
        request.push_str("\r\n");
        request.push_str(&payload);
        stream.write_all(request.as_bytes())?;
        let mut response = Vec::new();
        stream.read_to_end(&mut response)?;
        parse_response(&response)
    }

    /// `path` with this account's project added.
    fn scoped(&self, path: &str) -> String {
        let sep = if path.contains('?') { '&' } else { '?' };
        format!("{path}{sep}project={}", self.project)
    }

    fn call(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Value> {
        let (status, value) = self.raw(method, path, body)?;
        if !(200..300).contains(&status) {
            bail!(
                "LXD {method} {path}: {}",
                value["error"].as_str().unwrap_or("request failed")
            );
        }
        // Async operations: wait for them.
        if value["type"] == "async" {
            if let Some(op) = value["operation"].as_str() {
                let (_, done) = self.raw("GET", &format!("{op}/wait?timeout=120"), None)?;
                if done["metadata"]["status"] != "Success" {
                    bail!(
                        "LXD {method} {path}: {}",
                        done["metadata"]["err"].as_str().unwrap_or("operation failed")
                    );
                }
            }
        }
        Ok(value["metadata"].clone())
    }

    /// Makes this account's project unless it exists. Images and profiles
    /// come from the default project (so a downloaded image is shared and
    /// the driver finds the bridge); instances and volumes are private.
    pub fn ensure_project(&self) -> Result<()> {
        if self.raw("GET", &format!("/1.0/projects/{}", self.project), None)?.0 == 200 {
            return Ok(());
        }
        self.call(
            "POST",
            "/1.0/projects",
            Some(&json!({
                "name": self.project,
                "description": "Umoja agent sandboxes (OpenShell)",
                "config": {
                    "features.images": "false",
                    "features.profiles": "false",
                    "features.networks": "false",
                    "features.storage.volumes": "true",
                    "features.storage.buckets": "true",
                }
            })),
        )
        .map(|_| ())
    }

    /// The managed bridge's IPv4 address (lxdbr0 → 10.152.39.1) and its
    /// subnet prefix length.
    pub fn bridge_address(&self, network: &str) -> Result<(std::net::Ipv4Addr, u8)> {
        let net = self.call("GET", &format!("/1.0/networks/{network}"), None)?;
        let cidr = net["config"]["ipv4.address"]
            .as_str()
            .ok_or_else(|| anyhow!("{network} has no IPv4 address"))?;
        let (ip, len) = cidr.split_once('/').unwrap_or((cidr, "24"));
        Ok((ip.parse()?, len.parse().unwrap_or(24)))
    }

    /// The project's instances with their config and devices.
    pub fn instances(&self) -> Result<Vec<Instance>> {
        let v = self.call("GET", &self.scoped("/1.0/instances?recursion=2"), None)?;
        Ok(serde_json::from_value(v).unwrap_or_default())
    }

    /// The sandbox's two instances: (workload, supervisor).
    pub fn sandbox(&self, name: &str) -> Result<(Option<Instance>, Option<Instance>)> {
        let mut workload = None;
        let mut supervisor = None;
        for i in self.instances()? {
            if i.config.get("user.openshell.sandbox-name").map(String::as_str) != Some(name) {
                continue;
            }
            match i.config.get("user.openshell.role").map(String::as_str) {
                Some("supervisor") => supervisor = Some(i),
                _ if !i.name.starts_with("openshell-sup-") => workload = Some(i),
                _ => supervisor = Some(i),
            }
        }
        Ok((workload, supervisor))
    }

    /// Mounts host `source` at `path` in `instance`, idmapped so files keep
    /// their owner. Hot-plugged into a running container.
    pub fn add_disk(&self, instance: &str, device: &str, source: &str, path: &str, readonly: bool) -> Result<()> {
        let body = json!({ "devices": { device: {
            "type": "disk",
            "source": source,
            "path": path,
            "readonly": if readonly { "true" } else { "false" },
            "shift": "true",
            // A folder deleted while the sandbox is stopped mustn't keep
            // it from starting.
            "required": "false",
        }}});
        self.call("PATCH", &self.scoped(&format!("/1.0/instances/{instance}")), Some(&body))
            .map(|_| ())
    }

    pub fn remove_device(&self, instance: &str, device: &str) -> Result<()> {
        let path = self.scoped(&format!("/1.0/instances/{instance}"));
        let mut config = self.call("GET", &path, None)?;
        if let Some(devices) = config["devices"].as_object_mut() {
            devices.remove(device);
        }
        let put = json!({
            "architecture": config["architecture"],
            "config": config["config"],
            "devices": config["devices"],
            "ephemeral": config["ephemeral"],
            "profiles": config["profiles"],
            "description": config["description"],
        });
        self.call("PUT", &path, Some(&put)).map(|_| ())
    }

    /// Memory the instance uses now, in bytes.
    pub fn memory_usage(&self, instance: &str) -> Option<u64> {
        let state = self
            .call("GET", &self.scoped(&format!("/1.0/instances/{instance}/state")), None)
            .ok()?;
        state["memory"]["usage"].as_u64()
    }
}

#[derive(Debug, Clone, Default, serde::Deserialize)]
pub struct Instance {
    pub name: String,
    #[serde(default)]
    #[allow(dead_code)]
    pub status: String,
    #[serde(default)]
    pub config: HashMap<String, String>,
    #[serde(default)]
    pub devices: HashMap<String, HashMap<String, String>>,
    #[serde(default)]
    pub expanded_devices: HashMap<String, HashMap<String, String>>,
}

impl Instance {
    /// The static address the driver gave a supervisor's NIC.
    pub fn ipv4(&self) -> Option<std::net::Ipv4Addr> {
        self.expanded_devices
            .values()
            .chain(self.devices.values())
            .filter(|d| d.get("type").map(String::as_str) == Some("nic"))
            .find_map(|d| d.get("ipv4.address")?.parse().ok())
            .or_else(|| {
                // raw.lxc "lxc.net.0.ipv4.address = 10.0.0.5/24"
                self.config.get("raw.lxc").and_then(|raw| {
                    raw.lines()
                        .filter_map(|l| l.split_once('='))
                        .find(|(k, _)| k.trim() == "lxc.net.0.ipv4.address")
                        .and_then(|(_, v)| v.trim().split('/').next()?.parse().ok())
                })
            })
    }
}

fn parse_response(raw: &[u8]) -> Result<(u16, Value)> {
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| anyhow!("malformed LXD response"))?;
    let head = String::from_utf8_lossy(&raw[..split]);
    let mut body = raw[split + 4..].to_vec();
    let status: u16 = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| anyhow!("no HTTP status from LXD"))?;
    if head.lines().any(|l| l.to_ascii_lowercase().starts_with("transfer-encoding: chunked")) {
        body = dechunk(&body);
    }
    let value = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&body).context("LXD sent something that isn't JSON")?
    };
    Ok((status, value))
}

fn dechunk(mut data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let Some(eol) = data.windows(2).position(|w| w == b"\r\n") else { break };
        let size = usize::from_str_radix(String::from_utf8_lossy(&data[..eol]).trim(), 16).unwrap_or(0);
        if size == 0 {
            break;
        }
        let start = eol + 2;
        let end = (start + size).min(data.len());
        out.extend_from_slice(&data[start..end]);
        data = &data[(end + 2).min(data.len())..];
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_and_chunked() {
        let plain = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{\"type\":\"sync\",\"metadata\":{\"a\":1}}";
        let (s, v) = parse_response(plain).unwrap();
        assert_eq!(s, 200);
        assert_eq!(v["metadata"]["a"], 1);
        let chunked = b"HTTP/1.1 404 Not Found\r\nTransfer-Encoding: chunked\r\n\r\n5\r\n{\"a\":\r\n2\r\n2}\r\n0\r\n\r\n";
        let (s, v) = parse_response(chunked).unwrap();
        assert_eq!(s, 404);
        assert_eq!(v["a"], 2);
    }

    #[test]
    fn supervisor_address_from_device_or_raw_lxc() {
        let mut i = Instance::default();
        i.devices.insert(
            "eth0".into(),
            HashMap::from([("type".into(), "nic".into()), ("ipv4.address".into(), "10.1.2.3".into())]),
        );
        assert_eq!(i.ipv4(), Some("10.1.2.3".parse().unwrap()));
        let mut j = Instance::default();
        j.config.insert("raw.lxc".into(), "lxc.net.0.type = veth\nlxc.net.0.ipv4.address = 10.9.8.7/24\n".into());
        assert_eq!(j.ipv4(), Some("10.9.8.7".parse().unwrap()));
    }
}

#[cfg(test)]
mod live {
    #[test]
    #[ignore]
    fn live_access() {
        let l = super::Lxd::new("umoja-live");
        eprintln!("access: {:?}", std::mem::discriminant(&l.access()));
        eprintln!("project: {:?}", l.ensure_project());
        eprintln!("bridge: {:?}", l.bridge_address(&std::env::var("UMOJA_AGENTS_NETWORK").unwrap_or("lxdbr0".into())));
    }
}
