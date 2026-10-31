//! ext-workspace-v1 client: the same view of workspaces any bar gets
//! (ARCHITECTURE.md §6), kept as an ordered list for the indicator. With
//! per-monitor workspaces the compositor sends one group per monitor, and
//! a bar shows the workspaces of the group its monitor is in.

use smithay_client_toolkit::{
    dispatch2::Dispatch2,
    reexports::client::{
        Connection, Proxy, QueueHandle, backend::ObjectData, protocol::wl_output::WlOutput,
    },
};
use std::sync::Arc;
use wayland_protocols::ext::workspace::v1::client::{
    ext_workspace_group_handle_v1::{self, ExtWorkspaceGroupHandleV1},
    ext_workspace_handle_v1::{self, ExtWorkspaceHandleV1},
    ext_workspace_manager_v1::{self, ExtWorkspaceManagerV1},
};

use crate::App;

#[derive(Debug, Clone)]
pub struct Workspace {
    pub handle: ExtWorkspaceHandleV1,
    pub name: String,
    pub position: u32,
    pub active: bool,
}

/// The monitors a set of workspaces is shown on.
#[derive(Debug, Clone)]
struct Group {
    handle: ExtWorkspaceGroupHandleV1,
    outputs: Vec<WlOutput>,
    workspaces: Vec<ExtWorkspaceHandleV1>,
}

#[derive(Default)]
pub struct Workspaces {
    pub manager: Option<ExtWorkspaceManagerV1>,
    /// Updated as events arrive; published to `list` on `done`.
    pending: Vec<Workspace>,
    /// In strip order.
    pub list: Vec<Workspace>,
    pending_groups: Vec<Group>,
    groups: Vec<Group>,
}

impl Workspaces {
    /// The workspaces to show on a bar on `output`: those of its group, or
    /// all of them if no group claims it.
    pub fn on_output(&self, output: &WlOutput) -> Vec<Workspace> {
        let Some(group) = self.groups.iter().find(|g| g.outputs.contains(output)) else {
            return self.list.clone();
        };
        self.list
            .iter()
            .filter(|w| group.workspaces.contains(&w.handle))
            .cloned()
            .collect()
    }

    fn group(&mut self, handle: &ExtWorkspaceGroupHandleV1) -> Option<&mut Group> {
        self.pending_groups.iter_mut().find(|g| &g.handle == handle)
    }

    fn entry(&mut self, handle: &ExtWorkspaceHandleV1) -> Option<&mut Workspace> {
        self.pending.iter_mut().find(|w| &w.handle == handle)
    }

    /// Clicking a workspace: ask the compositor to bring it to the main
    /// monitor. Requests take effect on commit.
    pub fn activate(&self, handle: &ExtWorkspaceHandleV1) {
        if let Some(manager) = &self.manager {
            handle.activate();
            manager.commit();
        }
    }
}

pub struct ManagerData;
pub struct GroupData;
pub struct HandleData;

impl Dispatch2<ExtWorkspaceManagerV1, App> for ManagerData {
    fn event(
        &self,
        app: &mut App,
        _manager: &ExtWorkspaceManagerV1,
        event: ext_workspace_manager_v1::Event,
        _conn: &Connection,
        _qh: &QueueHandle<App>,
    ) {
        match event {
            ext_workspace_manager_v1::Event::Workspace { workspace } => {
                app.workspaces.pending.push(Workspace {
                    handle: workspace,
                    name: String::new(),
                    position: 0,
                    active: false,
                });
            }
            ext_workspace_manager_v1::Event::WorkspaceGroup { workspace_group } => {
                app.workspaces.pending_groups.push(Group {
                    handle: workspace_group,
                    outputs: Vec::new(),
                    workspaces: Vec::new(),
                });
            }
            ext_workspace_manager_v1::Event::Done => {
                let mut list = app.workspaces.pending.clone();
                list.sort_by_key(|w| w.position);
                app.workspaces.list = list;
                app.workspaces.groups = app.workspaces.pending_groups.clone();
                app.redraw_all();
            }
            ext_workspace_manager_v1::Event::Finished => {
                app.workspaces = Workspaces::default();
                app.redraw_all();
            }
            _ => {}
        }
    }

    fn event_created_child(opcode: u16, qh: &QueueHandle<App>) -> Arc<dyn ObjectData> {
        match opcode {
            ext_workspace_manager_v1::EVT_WORKSPACE_GROUP_OPCODE => {
                qh.make_data::<ExtWorkspaceGroupHandleV1, _>(GroupData)
            }
            _ => qh.make_data::<ExtWorkspaceHandleV1, _>(HandleData),
        }
    }
}

impl Dispatch2<ExtWorkspaceGroupHandleV1, App> for GroupData {
    fn event(
        &self,
        app: &mut App,
        group: &ExtWorkspaceGroupHandleV1,
        event: <ExtWorkspaceGroupHandleV1 as Proxy>::Event,
        _conn: &Connection,
        _qh: &QueueHandle<App>,
    ) {
        match event {
            ext_workspace_group_handle_v1::Event::OutputEnter { output } => {
                if let Some(g) = app.workspaces.group(group) {
                    g.outputs.push(output);
                }
            }
            ext_workspace_group_handle_v1::Event::OutputLeave { output } => {
                if let Some(g) = app.workspaces.group(group) {
                    g.outputs.retain(|o| o != &output);
                }
            }
            ext_workspace_group_handle_v1::Event::WorkspaceEnter { workspace } => {
                if let Some(g) = app.workspaces.group(group) {
                    g.workspaces.push(workspace);
                }
            }
            ext_workspace_group_handle_v1::Event::WorkspaceLeave { workspace } => {
                if let Some(g) = app.workspaces.group(group) {
                    g.workspaces.retain(|w| w != &workspace);
                }
            }
            ext_workspace_group_handle_v1::Event::Removed => {
                app.workspaces.pending_groups.retain(|g| &g.handle != group);
                group.destroy();
            }
            _ => {}
        }
    }
}

impl Dispatch2<ExtWorkspaceHandleV1, App> for HandleData {
    fn event(
        &self,
        app: &mut App,
        handle: &ExtWorkspaceHandleV1,
        event: ext_workspace_handle_v1::Event,
        _conn: &Connection,
        _qh: &QueueHandle<App>,
    ) {
        match event {
            ext_workspace_handle_v1::Event::Name { name } => {
                if let Some(w) = app.workspaces.entry(handle) {
                    w.name = name;
                }
            }
            ext_workspace_handle_v1::Event::Coordinates { coordinates } => {
                let position = coordinates
                    .get(..4)
                    .and_then(|bytes| bytes.try_into().ok())
                    .map_or(0, u32::from_ne_bytes);
                if let Some(w) = app.workspaces.entry(handle) {
                    w.position = position;
                }
            }
            ext_workspace_handle_v1::Event::State { state } => {
                let active = state
                    .into_result()
                    .is_ok_and(|s| s.contains(ext_workspace_handle_v1::State::Active));
                if let Some(w) = app.workspaces.entry(handle) {
                    w.active = active;
                }
            }
            ext_workspace_handle_v1::Event::Removed => {
                app.workspaces.pending.retain(|w| &w.handle != handle);
                handle.destroy();
            }
            _ => {}
        }
    }
}
