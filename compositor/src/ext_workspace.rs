//! ext-workspace-v1: publishes the workspace strip to bars and docks (waybar's
//! `ext/workspaces`, and cnde-shell later), and lets them switch workspaces.
//! ARCHITECTURE.md §6: the shell gets workspaces the same way as any bar.
//!
//! Mapping: one group spanning every monitor (they share one strip), or in
//! individual mode (`workspaces.mode`) one group per monitor. In joined mode
//! it's one group again, for the workspaces every monitor shows at once. We
//! publish workspaces that have windows or are on screen; the spare empty ones at
//! the strip's ends are an implementation detail. Names are their 1-based
//! position (in the group), "active" means on a monitor right now, and
//! activating one slides its strip to bring it onto the main monitor (its
//! own monitor, in individual mode).

use smithay::{
    output::Output,
    reexports::{
        wayland_protocols::ext::workspace::v1::server::{
            ext_workspace_group_handle_v1::{self, ExtWorkspaceGroupHandleV1, GroupCapabilities},
            ext_workspace_handle_v1::{self, ExtWorkspaceHandleV1, WorkspaceCapabilities},
            ext_workspace_manager_v1::{self, ExtWorkspaceManagerV1},
        },
        wayland_server::{
            Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
            backend::ClientId, protocol::wl_output::WlOutput,
        },
    },
};
use tracing::warn;

use crate::{state::State, workspaces::WorkspaceId};

#[derive(Default)]
pub struct ExtWorkspaceState {
    clients: Vec<ClientWorkspaces>,
}

/// What one bound manager has been told so far.
struct ClientWorkspaces {
    manager: ExtWorkspaceManagerV1,
    groups: Vec<GroupState>,
    handles: Vec<(ExtWorkspaceHandleV1, Info)>,
    /// Set by `activate`, applied on `commit` (requests are atomic per commit).
    pending_activate: Option<WorkspaceId>,
}

struct GroupState {
    /// "" for the one group of continuous mode, else the monitor's name.
    key: String,
    handle: ExtWorkspaceGroupHandleV1,
    outputs: Vec<WlOutput>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Info {
    id: WorkspaceId,
    group: String,
    name: String,
    position: u32,
    active: bool,
}

pub fn init(dh: &DisplayHandle) {
    dh.create_global::<State, ExtWorkspaceManagerV1, ()>(1, ());
}

impl State {
    /// The workspaces as bars should see them. Mid-slide, the destination
    /// counts as current, so indicators update the moment you switch.
    fn ext_workspace_infos(&self) -> Vec<Info> {
        let (outputs, main) = self.strip_outputs();
        if self.workspaces.individual {
            let mut infos = Vec::new();
            // Joined mode: the monitors share positions, so the main one
            // speaks for all in a single group.
            let shown: Vec<&Output> = match self.workspaces.joined {
                true => outputs.get(main).into_iter().collect(),
                false => outputs.iter().collect(),
            };
            for output in shown {
                let name = output.name();
                let target = self.workspaces.target_in(Some(&name));
                let mut position = 0;
                for p in 0..self.workspaces.len_of(Some(&name)) {
                    let Some(ws) = self
                        .workspaces
                        .index_at(Some(&name), p)
                        .map(|i| &self.workspaces.list[i])
                    else {
                        continue;
                    };
                    let visible = p == target;
                    let empty = match self.workspaces.joined {
                        true => !self.workspaces.occupied_at(p),
                        false => ws.windows.is_empty(),
                    };
                    if empty && !visible {
                        continue;
                    }
                    infos.push(Info {
                        id: ws.id,
                        group: if self.workspaces.joined {
                            String::new()
                        } else {
                            name.clone()
                        },
                        name: (position + 1).to_string(),
                        position,
                        active: visible,
                    });
                    position += 1;
                }
            }
            return infos;
        }
        let base = self.workspaces.target_in(None);
        let first_visible = base as isize - main as isize;
        let last_visible = first_visible + outputs.len() as isize - 1;

        let mut infos = Vec::new();
        for (index, ws) in self.workspaces.list.iter().enumerate() {
            let visible = (first_visible..=last_visible).contains(&(index as isize));
            if ws.windows.is_empty() && !visible {
                continue;
            }
            let position = infos.len() as u32;
            infos.push(Info {
                id: ws.id,
                group: String::new(),
                name: (position + 1).to_string(),
                position,
                active: visible,
            });
        }
        infos
    }

    /// Bring every client's view up to date; cheap when nothing changed.
    /// Called once per loop iteration.
    pub fn refresh_ext_workspaces(&mut self) {
        if self.ext_workspace_state.clients.is_empty() {
            return;
        }
        let infos = self.ext_workspace_infos();
        let groups: Vec<(String, Vec<Output>)> =
            if self.workspaces.individual && !self.workspaces.joined {
                self.strip_outputs()
                    .0
                    .into_iter()
                    .map(|o| (o.name(), vec![o]))
                    .collect()
            } else {
                vec![(String::new(), self.space.outputs().cloned().collect())]
            };
        let dh = self.display_handle.clone();

        for client in &mut self.ext_workspace_state.clients {
            let Some(wl_client) = client.manager.client() else {
                continue;
            };
            let mut changed = false;

            // New groups first, so workspaces can enter them.
            for (key, group_outputs) in &groups {
                if !client.groups.iter().any(|g| &g.key == key) {
                    match wl_client.create_resource::<ExtWorkspaceGroupHandleV1, (), State>(
                        &dh,
                        client.manager.version(),
                        (),
                    ) {
                        Ok(handle) => {
                            client.manager.workspace_group(&handle);
                            // Workspaces come and go by themselves; clients
                            // can't create them.
                            handle.capabilities(GroupCapabilities::empty());
                            client.groups.push(GroupState {
                                key: key.clone(),
                                handle,
                                outputs: Vec::new(),
                            });
                            changed = true;
                        }
                        Err(err) => warn!(?err, "failed to create workspace group"),
                    }
                }
                // Monitors (a group spans all of its own).
                let Some(group) = client.groups.iter_mut().find(|g| &g.key == key) else {
                    continue;
                };
                let current: Vec<WlOutput> = group_outputs
                    .iter()
                    .flat_map(|o| o.client_outputs(&wl_client))
                    .collect();
                for output in &current {
                    if !group.outputs.contains(output) {
                        group.handle.output_enter(output);
                        changed = true;
                    }
                }
                for output in &group.outputs {
                    if !current.contains(output) && output.is_alive() {
                        group.handle.output_leave(output);
                        changed = true;
                    }
                }
                group.outputs = current;
            }

            // Workspaces that went away.
            let group_handles = &client.groups;
            client.handles.retain(|(handle, info)| {
                if infos.iter().any(|i| i.id == info.id) {
                    return true;
                }
                if handle.is_alive() {
                    if let Some(group) = group_handles.iter().find(|g| g.key == info.group) {
                        group.handle.workspace_leave(handle);
                    }
                    handle.removed();
                }
                changed = true;
                false
            });

            // New and changed ones.
            for info in &infos {
                if let Some((handle, sent)) =
                    client.handles.iter_mut().find(|(_, s)| s.id == info.id)
                {
                    if sent != info {
                        if sent.group != info.group {
                            // Moved to another monitor's group.
                            let find = |key: &str| client.groups.iter().find(|g| g.key == key);
                            if let Some(old) = find(&sent.group) {
                                old.handle.workspace_leave(handle);
                            }
                            if let Some(new) = find(&info.group) {
                                new.handle.workspace_enter(handle);
                            }
                        }
                        send_details(handle, info, Some(sent));
                        *sent = info.clone();
                        changed = true;
                    }
                    continue;
                }
                let handle = match wl_client
                    .create_resource::<ExtWorkspaceHandleV1, WorkspaceId, State>(
                        &dh,
                        client.manager.version(),
                        info.id,
                    ) {
                    Ok(handle) => handle,
                    Err(err) => {
                        warn!(?err, "failed to create workspace handle");
                        continue;
                    }
                };
                client.manager.workspace(&handle);
                handle.id(format!("cnde-{}", info.id));
                handle.capabilities(WorkspaceCapabilities::Activate);
                send_details(&handle, info, None);
                if let Some(group) = client.groups.iter().find(|g| g.key == info.group) {
                    group.handle.workspace_enter(&handle);
                }
                client.handles.push((handle, info.clone()));
                changed = true;
            }

            // Groups that went away (monitors unplugged, or the mode changed).
            client.groups.retain(|group| {
                if groups.iter().any(|(key, _)| *key == group.key) {
                    return true;
                }
                if group.handle.is_alive() {
                    group.handle.removed();
                }
                changed = true;
                false
            });

            if changed {
                client.manager.done();
            }
        }
    }

    fn ext_workspace_client(
        &mut self,
        manager_client: Option<ClientId>,
    ) -> Option<&mut ClientWorkspaces> {
        let id = manager_client?;
        self.ext_workspace_state
            .clients
            .iter_mut()
            .find(|c| c.manager.client().is_some_and(|client| client.id() == id))
    }

    /// Slide the strip so workspace `id` is on the main monitor.
    fn activate_workspace(&mut self, id: WorkspaceId) {
        if let Some(index) = self.workspaces.list.iter().position(|ws| ws.id == id) {
            self.show_workspace(index, None);
        }
    }
}

/// Name, position and state; only what changed if `previous` is given.
fn send_details(handle: &ExtWorkspaceHandleV1, info: &Info, previous: Option<&Info>) {
    if previous.is_none_or(|p| p.name != info.name) {
        handle.name(info.name.clone());
    }
    if previous.is_none_or(|p| p.position != info.position) {
        // A 1-D strip: one coordinate, as native-endian u32s.
        handle.coordinates(info.position.to_ne_bytes().to_vec());
    }
    if previous.is_none_or(|p| p.active != info.active) {
        let state = if info.active {
            ext_workspace_handle_v1::State::Active
        } else {
            ext_workspace_handle_v1::State::empty()
        };
        handle.state(state);
    }
}

impl GlobalDispatch<ExtWorkspaceManagerV1, ()> for State {
    fn can_view(client: Client, _: &()) -> bool {
        crate::state::client_is_unrestricted(&client)
    }

    fn bind(
        state: &mut Self,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<ExtWorkspaceManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        let manager = data_init.init(resource, ());
        state.ext_workspace_state.clients.push(ClientWorkspaces {
            manager,
            groups: Vec::new(),
            handles: Vec::new(),
            pending_activate: None,
        });
        // Sends everything to the new client, then `done`.
        state.refresh_ext_workspaces();
    }
}

impl Dispatch<ExtWorkspaceManagerV1, ()> for State {
    fn request(
        state: &mut Self,
        client: &Client,
        manager: &ExtWorkspaceManagerV1,
        request: ext_workspace_manager_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            ext_workspace_manager_v1::Request::Commit => {
                let pending = state
                    .ext_workspace_client(Some(client.id()))
                    .and_then(|c| c.pending_activate.take());
                if let Some(id) = pending {
                    state.activate_workspace(id);
                }
            }
            ext_workspace_manager_v1::Request::Stop => {
                manager.finished();
                state
                    .ext_workspace_state
                    .clients
                    .retain(|c| &c.manager != manager);
            }
            _ => {}
        }
    }

    fn destroyed(state: &mut Self, _client: ClientId, manager: &ExtWorkspaceManagerV1, _data: &()) {
        state
            .ext_workspace_state
            .clients
            .retain(|c| &c.manager != manager);
    }
}

impl Dispatch<ExtWorkspaceGroupHandleV1, ()> for State {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _group: &ExtWorkspaceGroupHandleV1,
        request: ext_workspace_group_handle_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        // create_workspace isn't advertised (no capability); destroy needs
        // nothing from us.
        let _ = request;
    }
}

impl Dispatch<ExtWorkspaceHandleV1, WorkspaceId> for State {
    fn request(
        state: &mut Self,
        client: &Client,
        _handle: &ExtWorkspaceHandleV1,
        request: ext_workspace_handle_v1::Request,
        id: &WorkspaceId,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        // Only activate is advertised; the rest are ignored.
        if let ext_workspace_handle_v1::Request::Activate = request
            && let Some(c) = state.ext_workspace_client(Some(client.id()))
        {
            c.pending_activate = Some(*id);
        }
    }

    fn destroyed(
        state: &mut Self,
        client: ClientId,
        handle: &ExtWorkspaceHandleV1,
        _id: &WorkspaceId,
    ) {
        if let Some(c) = state.ext_workspace_client(Some(client)) {
            c.handles.retain(|(h, _)| h != handle);
        }
    }
}
