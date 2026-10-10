//! Pushing a DCP-assigned IP suite to the operating system.

use crate::dcp::NetParams;
use std::sync::{Arc, Mutex};

/// Applies an IP suite to a network interface. `Device` calls it before persisting a
/// DCP Set-IP; an `Err` turns the block's answer into `SetNotPossible`.
pub trait IpConfigurator: Send {
    /// Make `iface` carry `p.ip/p.subnet` with `p.gateway` as default route. A blank
    /// suite (`0.0.0.0`) removes every address from the interface.
    fn apply(&self, iface: &str, p: &NetParams) -> std::io::Result<()>;
}

/// Does nothing and succeeds (`StartOptions { apply_ip: false }`).
#[derive(Debug, Default)]
pub struct NoopIp;

impl IpConfigurator for NoopIp {
    fn apply(&self, _iface: &str, _p: &NetParams) -> std::io::Result<()> {
        Ok(())
    }
}

/// Records every call; fails on demand. Test double.
#[derive(Debug, Default)]
pub struct MockIp {
    /// Every `NetParams` passed to `apply`, in order.
    pub calls: Arc<Mutex<Vec<NetParams>>>,
    /// When true, `apply` returns an `Other` I/O error.
    pub fail: bool,
}

impl IpConfigurator for MockIp {
    fn apply(&self, _iface: &str, p: &NetParams) -> std::io::Result<()> {
        self.calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(p.clone());
        if self.fail {
            Err(std::io::Error::other("mock failure"))
        } else {
            Ok(())
        }
    }
}

/// Runs the iproute2 `ip` binary: `addr flush`, `addr add <ip>/<prefix>`, and
/// `route replace default via <gw>` when the gateway is neither blank nor the
/// device's own address. Needs `CAP_NET_ADMIN` (already required for `AF_PACKET`).
#[cfg(target_os = "linux")]
#[derive(Debug, Default)]
pub struct IpCommand;

#[cfg(target_os = "linux")]
use crate::dcp::store::dotted;

#[cfg(target_os = "linux")]
impl IpCommand {
    /// The argument vectors `apply` runs, in order (exposed for tests).
    pub fn commands(iface: &str, p: &NetParams) -> Vec<Vec<String>> {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let mut out = vec![s(&["addr", "flush", "dev", iface])];
        if p.ip != [0; 4] {
            let cidr = format!("{}/{}", dotted(p.ip), p.prefix_len());
            out.push(s(&["addr", "add", &cidr, "dev", iface]));
            if p.gateway != [0; 4] && p.gateway != p.ip {
                let gw = dotted(p.gateway);
                out.push(s(&[
                    "route", "replace", "default", "via", &gw, "dev", iface,
                ]));
            }
        }
        out
    }
}

#[cfg(target_os = "linux")]
impl IpConfigurator for IpCommand {
    fn apply(&self, iface: &str, p: &NetParams) -> std::io::Result<()> {
        for args in Self::commands(iface, p) {
            let st = std::process::Command::new("ip").args(&args).status()?;
            if !st.success() {
                return Err(std::io::Error::other(format!(
                    "ip {} exited with {st}",
                    args.join(" ")
                )));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dcp::NetParams;

    #[test]
    fn mock_records_calls_and_can_fail() {
        let ok = MockIp::default();
        ok.apply("eth0", &NetParams::default()).unwrap();
        assert_eq!(ok.calls.lock().unwrap().len(), 1);
        let bad = MockIp {
            fail: true,
            ..Default::default()
        };
        assert!(bad.apply("eth0", &NetParams::default()).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn ip_command_lines_are_built_from_params() {
        let p = NetParams {
            name: String::new(),
            ip: [172, 16, 2, 10],
            subnet: [255, 255, 255, 0],
            gateway: [172, 16, 2, 1],
        };
        assert_eq!(
            IpCommand::commands("eno2", &p),
            vec![
                vec!["addr", "flush", "dev", "eno2"],
                vec!["addr", "add", "172.16.2.10/24", "dev", "eno2"],
                vec![
                    "route",
                    "replace",
                    "default",
                    "via",
                    "172.16.2.1",
                    "dev",
                    "eno2"
                ],
            ]
        );
        let blank = NetParams::default();
        assert_eq!(
            IpCommand::commands("eno2", &blank),
            vec![vec!["addr", "flush", "dev", "eno2"]]
        );
        let self_gw = NetParams {
            gateway: [172, 16, 2, 10],
            ..p.clone()
        };
        assert_eq!(IpCommand::commands("eno2", &self_gw).len(), 2);
    }
}
