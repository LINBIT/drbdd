use crate::drbd::{
    BackingDevice, Connection, ConnectionState, Device, DiskState, EventType, EventUpdate,
    PeerDevice, ReplicationState, Resource, Role,
};
use anyhow::Result;
use log::{debug, warn};
use regex::Regex;
use std::io::BufRead;
use std::io::BufReader;
use std::process::{Command, Stdio};
use std::str::FromStr;
use std::sync::mpsc::{SendError, Sender};
use std::thread;
use std::time::Duration;

pub fn events2(tx: Sender<EventUpdate>) -> Result<()> {
    // minimum version check
    let version = Command::new("drbdadm").arg("--version").output()?;
    if !version.status.success() {
        return Err(anyhow::anyhow!(
            "'drbdadm --version' not executed successfully, stdout: '{}', stderr: '{}'",
            String::from_utf8(version.stdout).unwrap_or("<Could not convert stdout>".to_string()),
            String::from_utf8(version.stderr).unwrap_or("<Could not convert stderr>".to_string())
        ));
    }

    // check drbdsetup events2 version
    let pattern = Regex::new(r"^DRBDADM_VERSION_CODE=0x([[:xdigit:]]+)$")?;
    let (major, minor, patch) = split_version(pattern, version.stdout.clone())?;
    if let Err(e) = min_version((major, minor, patch), (9, 16, 0)) {
        return Err(anyhow::anyhow!(
            "drbdsetup minimum version ('9.16.0') not fulfilled: {}",
            e
        ));
    }
    let mut has_backing_dev = min_version((major, minor, patch), (9, 17, 0)).is_ok();

    if has_backing_dev {
        // minimal kernel version for backing_dev
        let pattern = Regex::new(r"^DRBD_KERNEL_VERSION_CODE=0x([[:xdigit:]]+)$")?;
        let (major, minor, patch) = split_version(pattern, version.stdout)?;
        let drbd90 = min_version((major, minor, patch), (9, 0, 28));
        let drbd911plus = min_version((major, minor, patch), (9, 1, 1));

        has_backing_dev =
            drbd911plus.is_ok() || (drbd90.is_ok() && !(major == 9 && minor == 1 && patch == 0));
    }

    if !has_backing_dev {
        warn!("backing device information will be missing!");
    }

    // TODO(): add some duration, like if we failed 5 times in the last minute or so
    let mut failed = 0;
    loop {
        if failed == 5 {
            return Err(anyhow::anyhow!(
                "events: events2_loop: Restarted events tracking too often, giving up"
            ));
        }

        debug!("events: events2_loop: starting process_events2 loop");
        match process_events2(&tx) {
            Ok(()) => break,
            Err(e) => {
                if e.is::<SendError<EventUpdate>>() {
                    debug!("events: events2: send error on chanel, bye");
                    return Err(e);
                }
                failed += 1;
                thread::sleep(Duration::from_secs(2));
            }
        }
    }

    Ok(())
}

fn process_events2(tx: &Sender<EventUpdate>) -> Result<()> {
    let mut cmd = Command::new("drbdsetup")
        .arg("events2")
        .arg("--full")
        .stdout(Stdio::piped())
        .spawn()
        .expect("events: process_event: could not spawn 'drbdsetup events2 --full'");
    let stdout = cmd
        .stdout
        .take()
        .expect("events: process_event: stdout set to Stdio::piped()");

    let mut reader = BufReader::new(stdout);

    let mut buf = String::new();
    while reader.read_line(&mut buf)? != 0 {
        // be careful here, every continue needs a buf.clear()!
        let line = buf.trim();
        if line == "exists -" {
            buf.clear();
            continue;
        }

        match parse_events2_line(&line) {
            Ok(update) => tx.send(update)?,
            Err(e) => debug!("could not parse line '{}', because {}", line, e),
        }
        buf.clear();
    }

    warn!("events: process_events2: exit");
    Err(anyhow::anyhow!("events: process_events2: exit"))
}

fn parse_events2_line(line: &str) -> Result<EventUpdate> {
    let mut words = line.split_whitespace();

    let verb = words.next().unwrap_or_default();
    let et = match EventType::from_str(verb) {
        Ok(et) => et,
        Err(_) => {
            return Err(anyhow::anyhow!(
                "events: parse_events2_line: unknown events type: {}",
                verb
            ));
        }
    };

    let what = words.next().unwrap_or_default();
    let kvs = words.filter_map(parse_kv);
    if what == "resource" {
        let mut resource = Resource {
            ..Default::default()
        };

        for (k, v) in kvs {
            match (k, v) {
                ("name", v) => resource.name = v.into(),
                ("role", v) => resource.role = Role::from_str(v)?,
                ("suspended", v) => resource.suspended = str_to_bool(v),
                ("write-ordering", v) => resource.write_ordering = v.to_string(),
                ("may_promote", v) => resource.may_promote = str_to_bool(v),
                ("promotion_score", v) => resource.promotion_score = v.parse::<_>()?,
                _ => {
                    return Err(anyhow::anyhow!(
                        "events: process_events2: resource: unknown keyword '{}'",
                        k
                    ))
                }
            };
        }
        return Ok(EventUpdate::Resource(et, resource));
    } else if what == "device" {
        let mut device = Device {
            ..Default::default()
        };
        for (k, v) in kvs {
            match (k, v) {
                ("name", v) => device.name = v.into(),
                ("volume", v) => device.volume = v.parse::<_>()?,
                ("minor", v) => device.minor = v.parse::<_>()?,
                ("disk", v) => device.disk_state = DiskState::from_str(v.into())?,
                ("client", v) => device.client = str_to_bool(v),
                ("backing_dev", v) => device.backing_dev = BackingDevice::from_str(v.into())?,
                ("quorum", v) => device.quorum = str_to_bool(v),
                ("size", v) => device.size = v.parse::<_>()?,
                ("read", v) => device.read = v.parse::<_>()?,
                ("written", v) => device.written = v.parse::<_>()?,
                ("al-writes", v) => device.al_writes = v.parse::<_>()?,
                ("bm-writes", v) => device.bm_writes = v.parse::<_>()?,
                ("upper-pending", v) => device.upper_pending = v.parse::<_>()?,
                ("lower-pending", v) => device.lower_pending = v.parse::<_>()?,
                ("al-suspended", v) => device.al_suspended = str_to_bool(v),
                ("blocked", v) => device.blocked = str_to_bool(v),
                _ => {
                    return Err(anyhow::anyhow!(
                        "events: process_events2: device: unknown keyword '{}'",
                        k
                    ))
                }
            };
        }
        return Ok(EventUpdate::Device(et, device));
    } else if what == "connection" {
        let mut conn = Connection {
            ..Default::default()
        };
        for (k, v) in kvs {
            match (k, v) {
                ("name", v) => conn.name = v.into(),
                ("peer-node-id", v) => conn.peer_node_id = v.parse::<_>()?,
                ("conn-name", v) => conn.conn_name = v.to_string(),
                ("connection", v) => conn.connection = ConnectionState::from_str(v.into())?,
                ("role", v) => conn.peer_role = Role::from_str(v.into())?,
                ("congested", v) => conn.congested = str_to_bool(v),
                ("ap-in-flight", v) => conn.ap_in_flight = v.parse::<_>()?,
                ("rs-in-flight", v) => conn.rs_in_flight = v.parse::<_>()?,
                _ => {
                    return Err(anyhow::anyhow!(
                        "events: process_events2: connection: unknown keyword '{}'",
                        k
                    ))
                }
            };
        }
        return Ok(EventUpdate::Connection(et, conn));
    } else if what == "peer-device" {
        let mut peerdevice = PeerDevice {
            has_sync_details: false,
            has_online_verify_details: false,
            ..Default::default()
        };
        for (k, v) in kvs {
            match (k, v) {
                ("name", v) => peerdevice.name = v.into(),
                ("conn-name", v) => peerdevice.conn_name = v.into(),
                ("volume", v) => peerdevice.volume = v.parse::<_>()?,
                ("peer-node-id", v) => peerdevice.peer_node_id = v.parse::<_>()?,
                ("replication", v) => {
                    peerdevice.replication_state = ReplicationState::from_str(v.into())?
                }
                ("peer-disk", v) => peerdevice.peer_disk_state = DiskState::from_str(v.into())?,
                ("peer-client", v) => peerdevice.peer_client = str_to_bool(v),
                ("resync-suspended", v) => peerdevice.resync_suspended = str_to_bool(v),
                ("received", v) => peerdevice.received = v.parse::<_>()?,
                ("sent", v) => peerdevice.sent = v.parse::<_>()?,
                ("out-of-sync", v) => peerdevice.out_of_sync = v.parse::<_>()?,
                ("pending", v) => peerdevice.pending = v.parse::<_>()?,
                ("unacked", v) => peerdevice.unacked = v.parse::<_>()?,
                ("done", _) => (),
                ("eta", _) => (),
                ("dbdt1", _) => (),
                _ => {
                    return Err(anyhow::anyhow!(
                        "events: process_events2: peer-device: unknown keyword '{}'",
                        k
                    ))
                }
            };
        }
        return Ok(EventUpdate::PeerDevice(et, peerdevice));
    }

    Err(anyhow::anyhow!(
        "events: process_events2: unknown keyword '{}'",
        what
    ))
}

fn parse_kv(item: &str) -> Option<(&str, &str)> {
    let mut iter = item.splitn(2, ':');
    match (iter.next(), iter.next()) {
        (Some(k), Some(v)) => Some((k, v)),
        _ => None,
    }
}

fn str_to_bool(s: &str) -> bool {
    s == "yes" || s == "true"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn string_to_bool() {
        assert_eq!(str_to_bool(&"yes"), true);
        assert_eq!(str_to_bool(&"true"), true);
        assert_eq!(str_to_bool(&"no"), false);
    }

    #[test]
    fn all_parsed_resource_update() {
        let up = parse_events2_line("exists resource name:foo role:Primary suspended:yes write-ordering:foo may_promote:yes promotion_score:23").unwrap();
        let expected = EventUpdate::Resource(
            EventType::Exists,
            Resource {
                name: "foo".to_string(),
                role: Role::Primary,
                suspended: true,
                write_ordering: "foo".to_string(),
                may_promote: true,
                promotion_score: 23,
                devices: vec![],
                connections: vec![],
            },
        );
        assert_eq!(up, expected);
    }

    #[test]
    fn all_parsed_device_update() {
        let up = parse_events2_line("change device name:foo volume:1 minor:1 disk:Attaching backing_dev:/dev/sda1 client:no quorum:yes size:1 read:1 written:1 al-writes:1 bm-writes:1 upper-pending:1 lower-pending:1 al-suspended:yes blocked:yes").unwrap();
        let expected = EventUpdate::Device(
            EventType::Change,
            Device {
                name: "foo".to_string(),
                volume: 1,
                minor: 1,
                disk_state: DiskState::Attaching,
                client: false,
                backing_dev: BackingDevice {
                    0: Some("/dev/sda1".to_string()),
                },
                quorum: true,
                size: 1,
                read: 1,
                written: 1,
                al_writes: 1,
                bm_writes: 1,
                upper_pending: 1,
                lower_pending: 1,
                al_suspended: true,
                blocked: true,
            },
        );
        assert_eq!(up, expected);

        // backing_dev as none
        let up = parse_events2_line("change device name:foo volume:1 minor:1 disk:Attaching backing_dev:none client:yes quorum:yes size:1 read:1 written:1 al-writes:1 bm-writes:1 upper-pending:1 lower-pending:1 al-suspended:yes blocked:yes").unwrap();
        let expected = EventUpdate::Device(
            EventType::Change,
            Device {
                name: "foo".to_string(),
                volume: 1,
                minor: 1,
                disk_state: DiskState::Attaching,
                client: true,
                backing_dev: BackingDevice { 0: None },
                quorum: true,
                size: 1,
                read: 1,
                written: 1,
                al_writes: 1,
                bm_writes: 1,
                upper_pending: 1,
                lower_pending: 1,
                al_suspended: true,
                blocked: true,
            },
        );
        assert_eq!(up, expected);
    }

    #[test]
    fn all_parsed_connection_update() {
        let up = parse_events2_line("exists connection name:foo peer-node-id:1 conn-name:bar connection:Connected role:Primary congested:yes ap-in-flight:1 rs-in-flight:1").unwrap();
        let expected = EventUpdate::Connection(
            EventType::Exists,
            Connection {
                name: "foo".to_string(),
                peer_node_id: 1,
                conn_name: "bar".to_string(),
                connection: ConnectionState::Connected,
                peer_role: Role::Primary,
                congested: true,
                ap_in_flight: 1,
                rs_in_flight: 1,
                peerdevices: vec![],
            },
        );
        assert_eq!(up, expected);
    }

    #[test]
    fn all_parsed_peerdevice_update() {
        let up = parse_events2_line("exists peer-device name:foo peer-node-id:1 conn-name:bar volume:1 replication:Established peer-disk:UpToDate peer-client:yes resync-suspended:yes received:1 sent:1 out-of-sync:1 pending:1 unacked:1").unwrap();
        let expected = EventUpdate::PeerDevice(
            EventType::Exists,
            PeerDevice {
                name: "foo".to_string(),
                peer_node_id: 1,
                conn_name: "bar".to_string(),
                volume: 1,
                replication_state: ReplicationState::Established,
                peer_disk_state: DiskState::UpToDate,
                peer_client: true,
                resync_suspended: true,
                received: 1,
                sent: 1,
                out_of_sync: 1,
                pending: 1,
                unacked: 1,
                has_sync_details: false,
                has_online_verify_details: false,
            },
        );
        assert_eq!(up, expected);
    }

    #[test]
    fn wrong_keys() {
        assert!(parse_events2_line("exists resource name:foo xxx:23").is_err());
        assert!(parse_events2_line("exists peer-device name:foo xxx:23").is_err());
        assert!(parse_events2_line("exists connection name:foo xxx:23").is_err());
        assert!(parse_events2_line("exists device name:foo xxx:23").is_err());
    }

    #[test]
    fn wrong_et() {
        assert!(parse_events2_line("xxx resource name:foo").is_err());
        // these will be implemented soon, but for now they are errors
        assert!(parse_events2_line("call helper").is_err());
        assert!(parse_events2_line("response helper").is_err());
    }

    #[test]
    fn wrong_what() {
        assert!(parse_events2_line("exists xxx name:foo").is_err());
        // path not implemented
        assert!(parse_events2_line("create path name:foo").is_err());
    }
}

fn split_version(pattern: regex::Regex, stdout: Vec<u8>) -> Result<(u8, u8, u8)> {
    let version = String::from_utf8(stdout)?;
    let version = version
        .lines()
        .filter_map(|line| pattern.captures(line))
        .next()
        .ok_or(anyhow::anyhow!(
            "Could not determine version from pattern '{}'",
            pattern
        ))?;

    let version = u32::from_str_radix(&version[1], 16)?;

    let major = ((version >> 16) & 0xff) as u8;
    let minor = ((version >> 8) & 0xff) as u8;
    let patch = (version & 0xff) as u8;

    Ok((major, minor, patch))
}

fn min_version(have: (u8, u8, u8), want: (u8, u8, u8)) -> Result<()> {
    if have.0 > want.0 {
        return Ok(());
    }
    if have.0 < want.0 {
        return Err(anyhow::anyhow!(
            "Major version too small {} vs. {}",
            have.0,
            want.0
        ));
    }

    if have.1 > want.1 {
        return Ok(());
    }
    if have.1 < want.1 {
        return Err(anyhow::anyhow!(
            "Minor version too small {} vs. {}",
            have.1,
            want.1
        ));
    }

    if have.2 > want.2 {
        return Ok(());
    }
    if have.2 < want.2 {
        return Err(anyhow::anyhow!(
            "Patch version too small {} vs. {}",
            have.2,
            want.2
        ));
    }

    Ok(())
}
