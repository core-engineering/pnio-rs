//! Persistence of the DCP-assigned identity (`NetParams`): a four-line text file,
//! written atomically (temp file + rename), the same recipe as `ImStore`.

use crate::dcp::NetParams;
use std::path::PathBuf;

/// Backing file for [`NetParams`]; `path = None` means RAM only.
#[derive(Debug, Clone)]
pub struct NetStore {
    /// The file; `None` = nothing is ever read or written.
    pub path: Option<PathBuf>,
}

impl NetStore {
    /// A store on `path` (or RAM only).
    pub fn new(path: Option<PathBuf>) -> Self {
        NetStore { path }
    }

    /// Read the file. `None` when there is no path, no file, the file is malformed, or
    /// its name fails [`validate_station_name`](crate::dcp::validate_station_name) (an
    /// empty name — the blank identity after a ResetToFactory — is accepted); the last
    /// two are logged at warn and the caller falls back to its configured defaults.
    pub fn load(&self) -> Option<NetParams> {
        let path = self.path.as_ref()?;
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(e) => {
                log::warn!("net store {}: {e}", path.display());
                return None;
            }
        };
        match parse(&text) {
            Some(p) => match crate::dcp::validate_station_name(&p.name) {
                Err(e) if !p.name.is_empty() => {
                    log::warn!(
                        "net store {}: invalid name {:?} ({e}), ignoring",
                        path.display(),
                        p.name
                    );
                    None
                }
                _ => Some(p),
            },
            None => {
                log::warn!("net store {}: malformed, ignoring", path.display());
                None
            }
        }
    }

    /// Write `p` (temp file + rename). Failures are logged, never returned: the
    /// in-memory identity stands regardless.
    pub fn save(&self, p: &NetParams) {
        let Some(path) = &self.path else { return };
        let text = format!(
            "name={}\nip={}\nsubnet={}\ngateway={}\n",
            p.name,
            dotted(p.ip),
            dotted(p.subnet),
            dotted(p.gateway)
        );
        let tmp = path.with_file_name(format!(".net-{}.tmp", std::process::id()));
        let r = std::fs::write(&tmp, text).and_then(|_| std::fs::rename(&tmp, path));
        if let Err(e) = r {
            log::error!("net store {}: {e}", path.display());
            let _ = std::fs::remove_file(&tmp);
        }
    }
}

/// `a.b.c.d` text form of an IPv4 address (also used by `dcp::ipconf`).
pub(crate) fn dotted(a: [u8; 4]) -> String {
    format!("{}.{}.{}.{}", a[0], a[1], a[2], a[3])
}

fn quad(s: &str) -> Option<[u8; 4]> {
    let mut out = [0u8; 4];
    let mut n = 0;
    for part in s.split('.') {
        if n == 4 {
            return None;
        }
        out[n] = part.parse().ok()?;
        n += 1;
    }
    (n == 4).then_some(out)
}

fn parse(text: &str) -> Option<NetParams> {
    let mut p = NetParams::default();
    let mut seen = [false; 4];
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        let (k, v) = line.split_once('=')?;
        match k {
            "name" => {
                p.name = v.to_string();
                seen[0] = true;
            }
            "ip" => {
                p.ip = quad(v)?;
                seen[1] = true;
            }
            "subnet" => {
                p.subnet = quad(v)?;
                seen[2] = true;
            }
            "gateway" => {
                p.gateway = quad(v)?;
                seen[3] = true;
            }
            _ => return None,
        }
    }
    seen.iter().all(|&s| s).then_some(p)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dcp::NetParams;

    fn params() -> NetParams {
        NetParams {
            name: "pnio-dev-b".into(),
            ip: [172, 16, 2, 10],
            subnet: [255, 255, 255, 0],
            gateway: [172, 16, 2, 1],
        }
    }

    #[test]
    fn round_trips_through_the_file() {
        let dir = std::env::temp_dir().join(format!("pnio-netstore-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = NetStore::new(Some(dir.join("net")));
        assert_eq!(store.load(), None);
        store.save(&params());
        assert_eq!(store.load(), Some(params()));
        let text = std::fs::read_to_string(dir.join("net")).unwrap();
        assert_eq!(
            text,
            "name=pnio-dev-b\nip=172.16.2.10\nsubnet=255.255.255.0\ngateway=172.16.2.1\n"
        );
        assert!(
            std::fs::read_dir(&dir).unwrap().all(|e| !e
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".net-")),
            "no temp file left"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn blank_params_round_trip_and_crlf_loads() {
        let dir = std::env::temp_dir().join(format!("pnio-netstore-crlf-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("net");
        let store = NetStore::new(Some(path.clone()));
        store.save(&NetParams::default());
        assert_eq!(store.load(), Some(NetParams::default()));
        std::fs::write(
            &path,
            "name=pnio-dev\r\nip=10.0.0.2\r\nsubnet=255.0.0.0\r\ngateway=0.0.0.0\r\n",
        )
        .unwrap();
        assert_eq!(store.load().unwrap().name, "pnio-dev");
        assert_eq!(store.load().unwrap().ip, [10, 0, 0, 2]);
        std::fs::write(&path, "name=pnio-dev\nip=not-an-ip\n").unwrap();
        assert_eq!(store.load(), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_invalid_station_name_is_rejected_and_a_blank_one_accepted() {
        let dir = std::env::temp_dir().join(format!("pnio-netstore-name-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("net");
        let store = NetStore::new(Some(path.clone()));
        std::fs::write(
            &path,
            "name=Bad_Name\nip=10.0.0.2\nsubnet=255.0.0.0\ngateway=0.0.0.0\n",
        )
        .unwrap();
        assert_eq!(store.load(), None);
        std::fs::write(
            &path,
            "name=\nip=0.0.0.0\nsubnet=0.0.0.0\ngateway=0.0.0.0\n",
        )
        .unwrap();
        assert_eq!(store.load(), Some(NetParams::default()));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn no_path_is_a_noop() {
        let store = NetStore::new(None);
        store.save(&params());
        assert_eq!(store.load(), None);
    }
}
