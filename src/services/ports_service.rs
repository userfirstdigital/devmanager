use crate::models::{AppConfig, PortConflict, PortConflictEntry, PortStatus};
use crate::services::{pid_file, platform_service};
use std::collections::{BTreeMap, HashMap};

pub fn snapshot_ports(ports: &[u16]) -> Result<HashMap<u16, PortStatus>, String> {
    let listener_pids = platform_service::snapshot_listener_pids(ports)?;
    let mut statuses = HashMap::with_capacity(ports.len());

    for &port in ports {
        let status = match listener_pids.get(&port).copied() {
            Some(pid) => PortStatus {
                port,
                in_use: true,
                pid: Some(pid),
                process_name: None,
            },
            None => PortStatus {
                port,
                in_use: false,
                pid: None,
                process_name: None,
            },
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
    let pid = platform_service::find_pid_on_port(port)?
        .ok_or_else(|| format!("No process found listening on port {port}"))?;
    platform_service::kill_process_tree(pid)?;
    let _ = pid_file::prune_inactive_entries();
    Ok(())
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
