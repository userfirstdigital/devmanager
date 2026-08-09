use crate::models::{AppConfig, PortConflict, PortConflictEntry, PortStatus};
use crate::process::ports::{ListenerIdentity, PortInventorySnapshot, PortObservation};
use crate::services::platform_service;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, RwLock};

/// Background-owned listener inventory with an immutable read-only snapshot.
///
/// `refresh` is intentionally synchronous so its caller can choose the
/// application's existing background executor/thread. `cached_snapshot` and
/// `publish` never enumerate listeners or inspect processes.
#[derive(Clone, Debug)]
pub struct PortInventory {
    snapshot: Arc<RwLock<Arc<PortInventorySnapshot>>>,
}

impl Default for PortInventory {
    fn default() -> Self {
        Self::new()
    }
}

impl PortInventory {
    pub fn new() -> Self {
        Self {
            snapshot: Arc::new(RwLock::new(Arc::new(PortInventorySnapshot::new(
                BTreeMap::new(),
            )))),
        }
    }

    pub fn cached_snapshot(&self) -> Arc<PortInventorySnapshot> {
        self.snapshot
            .read()
            .expect("port inventory cache lock")
            .clone()
    }

    pub fn publish(&self, snapshot: Arc<PortInventorySnapshot>) {
        *self.snapshot.write().expect("port inventory cache lock") = snapshot;
    }

    /// Run one batched native probe and publish its immutable result.
    ///
    /// The operation is designed for a scheduler/background executor. If the
    /// listener table itself cannot be read, an explicit per-port error
    /// snapshot is published before returning the error; a failed probe is
    /// never published as an empty/free result.
    pub fn refresh(&self, ports: &[u16]) -> Result<Arc<PortInventorySnapshot>, String> {
        match scan_listener_inventory(ports) {
            Ok(snapshot) => {
                let mut observations = self.cached_snapshot().observations().clone();
                observations.extend(
                    snapshot
                        .observations()
                        .iter()
                        .map(|(port, observation)| (*port, observation.clone())),
                );
                let snapshot = Arc::new(PortInventorySnapshot::new(observations));
                self.publish(snapshot.clone());
                Ok(snapshot)
            }
            Err(error) => {
                let failure =
                    PortInventorySnapshot::probe_failure(ports.iter().copied(), error.clone());
                let mut observations = self.cached_snapshot().observations().clone();
                observations.extend(
                    failure
                        .observations()
                        .iter()
                        .map(|(port, observation)| (*port, observation.clone())),
                );
                let snapshot = Arc::new(PortInventorySnapshot::new(observations));
                self.publish(snapshot);
                Err(error)
            }
        }
    }
}

/// Probe all requested ports with one native listener-table query.
///
/// Listener PID-to-creation identity enrichment is still performed outside
/// any render/input caller. An individual process that disappears between the
/// listener query and identity probe becomes an explicit per-port error.
pub fn scan_listener_inventory(ports: &[u16]) -> Result<PortInventorySnapshot, String> {
    let listener_pids = platform_service::snapshot_listener_pids(ports)?;
    let mut observations = ports
        .iter()
        .copied()
        .map(|port| (port, PortObservation::Free))
        .collect::<BTreeMap<_, _>>();

    for (port, pids) in listener_pids {
        let mut identities = Vec::with_capacity(pids.len());
        let mut errors = Vec::new();
        for pid in pids {
            match capture_listener_identity(pid) {
                Ok(identity) => identities.push(identity),
                Err(error) => errors.push(error),
            }
        }
        let observation = if errors.is_empty() {
            PortObservation::from_listeners(identities)
        } else {
            PortObservation::ProbeError(errors.join("; "))
        };
        observations.insert(port, observation);
    }

    Ok(PortInventorySnapshot::new(observations))
}

#[cfg(windows)]
fn capture_listener_identity(pid: u32) -> Result<ListenerIdentity, String> {
    use std::ffi::c_void;

    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;

    #[repr(C)]
    #[derive(Default)]
    struct FileTime {
        low_date_time: u32,
        high_date_time: u32,
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn OpenProcess(desired_access: u32, inherit_handle: i32, process_id: u32) -> *mut c_void;
        fn GetProcessTimes(
            process: *mut c_void,
            creation: *mut FileTime,
            exit: *mut FileTime,
            kernel: *mut FileTime,
            user: *mut FileTime,
        ) -> i32;
        fn CloseHandle(object: *mut c_void) -> i32;
    }

    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return Err(format!(
            "could not open listener PID {pid} for identity verification: {}",
            std::io::Error::last_os_error()
        ));
    }

    let mut creation = FileTime::default();
    let mut exit = FileTime::default();
    let mut kernel = FileTime::default();
    let mut user = FileTime::default();
    let result =
        unsafe { GetProcessTimes(process, &mut creation, &mut exit, &mut kernel, &mut user) };
    let close_result = unsafe { CloseHandle(process) };
    if result == 0 {
        return Err(format!(
            "could not read listener PID {pid} creation time: {}",
            std::io::Error::last_os_error()
        ));
    }
    if close_result == 0 {
        return Err(format!(
            "could not close listener PID {pid} identity handle: {}",
            std::io::Error::last_os_error()
        ));
    }

    let creation_time_100ns =
        ((creation.high_date_time as u64) << 32) | creation.low_date_time as u64;
    ListenerIdentity::new(pid, creation_time_100ns).map_err(|error| error.to_string())
}

#[cfg(not(windows))]
fn capture_listener_identity(pid: u32) -> Result<ListenerIdentity, String> {
    let identity = platform_service::capture_process_identity(pid)
        .ok_or_else(|| format!("listener PID {pid} exited before identity verification"))?;
    ListenerIdentity::new(pid, identity.started_at_unix_secs)
        .map_err(|error| format!("could not verify listener PID {pid}: {error}"))
}

pub fn snapshot_ports(ports: &[u16]) -> Result<HashMap<u16, PortStatus>, String> {
    let snapshot = scan_listener_inventory(ports)?;
    legacy_statuses_from_snapshot(&snapshot, ports)
}

/// Convert an inventory snapshot to the existing UI/remote port model.
///
/// The legacy model has one optional PID, so an ambiguous multi-listener
/// observation intentionally retains only `in_use = true` and no PID. This
/// preserves the blue external presentation instead of claiming ownership
/// from an arbitrary listener.
pub fn legacy_statuses_from_snapshot(
    snapshot: &PortInventorySnapshot,
    ports: &[u16],
) -> Result<HashMap<u16, PortStatus>, String> {
    let mut statuses = HashMap::with_capacity(ports.len());

    for &port in ports {
        let status = match snapshot.observation(port) {
            Some(PortObservation::Listeners(listeners)) => PortStatus {
                port,
                in_use: true,
                pid: (listeners.len() == 1).then(|| listeners[0].pid()),
                process_name: None,
            },
            Some(PortObservation::Free) => PortStatus {
                port,
                in_use: false,
                pid: None,
                process_name: None,
            },
            Some(PortObservation::ProbeError(error)) => return Err(error.clone()),
            None => {
                return Err(format!(
                    "port {port} was not included in listener inventory"
                ))
            }
        };
        statuses.insert(port, status);
    }

    Ok(statuses)
}

pub fn check_port_in_use(port: u16) -> Result<PortStatus, String> {
    let mut status = snapshot_ports(&[port])?
        .remove(&port)
        .unwrap_or(PortStatus {
            port,
            in_use: false,
            pid: None,
            process_name: None,
        });
    if let Some(pid) = status.pid {
        status.process_name = platform_service::get_process_name(pid)?;
    }
    Ok(status)
}

pub fn kill_port(port: u16) -> Result<(), String> {
    let _ = port;
    Err("refusing to kill port: this legacy API has no exact managed resource fence; external or unknown listeners are never controlled".to_string())
}

pub fn get_port_conflicts(config: &AppConfig) -> Vec<PortConflict> {
    let mut port_map: BTreeMap<u16, Vec<PortConflictEntry>> = BTreeMap::new();

    for project in &config.projects {
        for folder in &project.folders {
            for command in &folder.commands {
                if let Some(port) = command.port {
                    port_map.entry(port).or_default().push(PortConflictEntry {
                        project_name: project.name.clone(),
                        command_label: command.label.clone(),
                        command_id: command.id.clone(),
                    });
                }
            }
        }
    }

    port_map
        .into_iter()
        .filter(|(_, commands)| commands.len() > 1)
        .map(|(port, commands)| PortConflict { port, commands })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::get_port_conflicts;
    use crate::models::{AppConfig, Project, ProjectFolder, RunCommand};

    #[test]
    fn duplicate_ports_are_reported_once() {
        let config = AppConfig {
            projects: vec![
                Project {
                    id: "project-a".to_string(),
                    name: "Project A".to_string(),
                    folders: vec![ProjectFolder {
                        id: "folder-a".to_string(),
                        name: "api".to_string(),
                        commands: vec![RunCommand {
                            id: "command-a".to_string(),
                            label: "dev".to_string(),
                            port: Some(3000),
                            ..Default::default()
                        }],
                        ..Default::default()
                    }],
                    ..Default::default()
                },
                Project {
                    id: "project-b".to_string(),
                    name: "Project B".to_string(),
                    folders: vec![ProjectFolder {
                        id: "folder-b".to_string(),
                        name: "web".to_string(),
                        commands: vec![
                            RunCommand {
                                id: "command-b".to_string(),
                                label: "serve".to_string(),
                                port: Some(3000),
                                ..Default::default()
                            },
                            RunCommand {
                                id: "command-c".to_string(),
                                label: "admin".to_string(),
                                port: Some(4100),
                                ..Default::default()
                            },
                        ],
                        ..Default::default()
                    }],
                    ..Default::default()
                },
            ],
            ..Default::default()
        };

        let conflicts = get_port_conflicts(&config);

        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].port, 3000);
        assert_eq!(conflicts[0].commands.len(), 2);
    }
}
