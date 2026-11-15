//! Pairing Bluetooth devices with the relay, through BlueZ on the system bus: finding them,
//! pairing, trusting and connecting them, and forgetting them. Forwarding them is `hid`'s job.

use serde::{Deserialize, Serialize};

/// A Bluetooth input device the relay can see or knows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(test, derive(ts_rs::TS), ts(export))]
pub struct Nearby {
    /// Like 8C:41:F2:97:4F:28.
    pub address: String,
    pub name: String,
    pub paired: bool,
    pub connected: bool,
    /// BlueZ's icon name: input-gaming, input-keyboard, input-mouse.
    pub icon: String,
}

/// Checks an address such as 8C:41:F2:97:4F:28 and puts it in BlueZ's form.
pub fn address(input: &str) -> anyhow::Result<String> {
    let a = input.trim().to_uppercase();
    let ok = a.len() == 17 && a.split(':').count() == 6 && a.split(':').all(|p| p.len() == 2 && p.bytes().all(|b| b.is_ascii_hexdigit()));
    anyhow::ensure!(ok, "{input:?} is not a Bluetooth address; they look like 8C:41:F2:97:4F:28");
    Ok(a)
}

#[cfg(target_os = "linux")]
pub use linux::{forget, pair, scan};

#[cfg(target_os = "linux")]
mod linux {
    use super::Nearby;
    use anyhow::{Context, Result, bail};
    use std::{collections::HashMap, thread, time::Duration};
    use zbus::{Connection, ObjectPath, OwnedObjectPath, OwnedVariant, Proxy, interface};

    type Objects = HashMap<OwnedObjectPath, HashMap<String, HashMap<String, OwnedVariant>>>;
    const AGENT: &str = "/plugover/agent";
    /// How long pairing looks for a device it has not seen yet.
    const FIND: Duration = Duration::from_secs(15);

    async fn proxy<'a>(c: &Connection, path: &'a str, iface: &'a str) -> Result<Proxy<'a>> {
        Ok(Proxy::new(c, "org.bluez", path, iface).await?)
    }

    async fn objects(c: &Connection) -> Result<Objects> {
        let om = proxy(c, "/", "org.freedesktop.DBus.ObjectManager").await.context("BlueZ is not running (is the bluetooth service on?)")?;
        om.call("GetManagedObjects", &()).await.context("BlueZ is not running (is the bluetooth service on?)")
    }

    fn adapter(objs: &Objects) -> Result<String> {
        let mut paths: Vec<String> = objs.iter().filter(|(_, i)| i.contains_key("org.bluez.Adapter1")).map(|(p, _)| p.to_string()).collect();
        paths.sort();
        paths.into_iter().next().context("this computer has no Bluetooth adapter")
    }

    /// Input devices BlueZ knows, with their object paths.
    fn devices(objs: &Objects) -> Vec<(String, Nearby)> {
        let mut found: Vec<(String, Nearby)> = objs
            .iter()
            .filter_map(|(path, ifaces)| {
                let p = ifaces.get("org.bluez.Device1")?;
                let text = |k: &str| p.get(k).and_then(|v| String::try_from(v.clone()).ok()).unwrap_or_default();
                let flag = |k: &str| p.get(k).and_then(|v| bool::try_from(v.clone()).ok()).unwrap_or(false);
                let icon = text("Icon");
                let class = p.get("Class").and_then(|v| u32::try_from(v.clone()).ok()).unwrap_or(0);
                let appearance = p.get("Appearance").and_then(|v| u16::try_from(v.clone()).ok()).unwrap_or(0);
                // Only input devices can be forwarded; headphones and phones stay out of the list.
                // Controllers paired by cable (a DualShock 4 through the sixaxis plugin) have no icon.
                let input = icon.starts_with("input")
                    || (class >> 8) & 0x1f == 0x05 // Class of Device: peripheral
                    || appearance >> 6 == 0x0f // BLE appearance: HID
                    || flag("CablePairing");
                input.then(|| {
                    let name = Some(text("Alias")).filter(|n| !n.is_empty()).unwrap_or_else(|| text("Name"));
                    let paired = flag("Paired") || flag("CablePairing");
                    (path.to_string(), Nearby { address: text("Address"), name, paired, connected: flag("Connected"), icon })
                })
            })
            .collect();
        found.sort_by(|a, b| (b.1.connected, b.1.paired, &a.1.name).cmp(&(a.1.connected, a.1.paired, &b.1.name)));
        found
    }

    async fn power_on(c: &Connection, adapter: &str) -> Result<()> {
        let a = proxy(c, adapter, "org.bluez.Adapter1").await?;
        if !a.get_property::<bool>("Powered").await.unwrap_or(false) {
            a.set_property("Powered", true).await.context("turning the Bluetooth adapter on")?;
        }
        Ok(())
    }

    /// Looks for devices for `secs` seconds; lists them with the ones already paired.
    pub fn scan(secs: u64) -> Result<Vec<Nearby>> {
        zbus::block_on(async {
            let c = Connection::system().await.context("connecting to the system bus")?;
            let adapter = adapter(&objects(&c).await?)?;
            power_on(&c, &adapter).await?;
            let a = proxy(&c, &adapter, "org.bluez.Adapter1").await?;
            a.call::<_, _, ()>("StartDiscovery", &()).await.context("starting to look for devices")?;
            // zbus answers the bus on its own thread meanwhile.
            thread::sleep(Duration::from_secs(secs));
            let _ = a.call::<_, _, ()>("StopDiscovery", &()).await;
            Ok(devices(&objects(&c).await?).into_iter().map(|(_, d)| d).collect())
        })
    }

    /// Answers BlueZ during pairing. Game controllers pair without a code; this accepts what they
    /// ask, and offers 0000 to old devices that want a PIN.
    struct Agent;

    #[interface(name = "org.bluez.Agent1")]
    impl Agent {
        fn release(&self) {}
        fn request_pin_code(&self, _device: ObjectPath<'_>) -> String {
            "0000".into()
        }
        fn display_pin_code(&self, _device: ObjectPath<'_>, _pin: &str) {}
        fn request_passkey(&self, _device: ObjectPath<'_>) -> u32 {
            0
        }
        fn display_passkey(&self, _device: ObjectPath<'_>, _passkey: u32, _entered: u16) {}
        fn request_confirmation(&self, _device: ObjectPath<'_>, _passkey: u32) {}
        fn request_authorization(&self, _device: ObjectPath<'_>) {}
        fn authorize_service(&self, _device: ObjectPath<'_>, _uuid: &str) {}
        fn cancel(&self) {}
    }

    /// Pairs the device at `address` (in pairing mode), trusts it so it reconnects by itself, and
    /// connects it.
    pub fn pair(address: &str) -> Result<()> {
        zbus::block_on(async {
            let c = zbus::connection::Builder::system().serve_at(AGENT, Agent).build().await.context("connecting to the system bus")?;
            let objs = objects(&c).await?;
            let adapter = adapter(&objs)?;
            power_on(&c, &adapter).await?;
            let manager = proxy(&c, "/org/bluez", "org.bluez.AgentManager1").await?;
            manager.call::<_, _, ()>("RegisterAgent", &(ObjectPath::try_from(AGENT)?, "NoInputNoOutput")).await.context("registering with BlueZ")?;
            let result = pair_with(&c, &adapter, address).await;
            let _ = manager.call::<_, _, ()>("UnregisterAgent", &(ObjectPath::try_from(AGENT)?,)).await;
            result
        })
    }

    async fn pair_with(c: &Connection, adapter: &str, address: &str) -> Result<()> {
        let a = proxy(c, adapter, "org.bluez.Adapter1").await?;
        let find = |objs: &Objects| devices(objs).into_iter().find(|(_, d)| d.address.eq_ignore_ascii_case(address));
        let mut found = find(&objects(c).await?);
        if found.is_none() {
            // Not seen lately: look for it while it is in pairing mode.
            let _ = a.call::<_, _, ()>("StartDiscovery", &()).await;
            let start = std::time::Instant::now();
            while found.is_none() && start.elapsed() < FIND {
                thread::sleep(Duration::from_millis(500));
                found = find(&objects(c).await?);
            }
        }
        let _ = a.call::<_, _, ()>("StopDiscovery", &()).await;
        let Some((path, dev)) = found else { bail!("no input device {address} found; put it in pairing mode and try again") };
        let d = proxy(c, &path, "org.bluez.Device1").await?;
        if !dev.paired
            && let Err(e) = d.call::<_, _, ()>("Pair", &()).await
            && !e.to_string().contains("AlreadyExists")
        {
            bail!("pairing {} failed: {e}", dev.name);
        }
        d.set_property("Trusted", true).await.context("trusting it, so it reconnects by itself")?;
        if !dev.connected {
            d.call::<_, _, ()>("Connect", &()).await.with_context(|| format!("connecting {}", dev.name))?;
        }
        Ok(())
    }

    /// Unpairs the device at `address`.
    pub fn forget(address: &str) -> Result<()> {
        zbus::block_on(async {
            let c = Connection::system().await.context("connecting to the system bus")?;
            let objs = objects(&c).await?;
            let adapter = adapter(&objs)?;
            let (path, _) = devices(&objs).into_iter().find(|(_, d)| d.address.eq_ignore_ascii_case(address)).with_context(|| format!("{address} is not paired here"))?;
            let a = proxy(&c, &adapter, "org.bluez.Adapter1").await?;
            a.call::<_, _, ()>("RemoveDevice", &(ObjectPath::try_from(path.as_str())?,)).await.with_context(|| format!("forgetting {address}"))?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_are_checked() {
        assert_eq!(address(" 8c:41:f2:97:4f:28 ").unwrap(), "8C:41:F2:97:4F:28");
        assert!(address("8C:41:F2:97:4F").is_err());
        assert!(address("8C:41:F2:97:4F:2G").is_err());
    }
}
