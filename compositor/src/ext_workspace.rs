//! ext-workspace-v1: publishes the workspace strip to bars and docks (waybar's
//! `ext/workspaces`, and cnde-shell later), and lets them switch workspaces.
//! ARCHITECTURE.md §6: the shell gets workspaces the same way as any bar.
//!
//! Mapping: one group spanning every monitor (they share one strip). We
//! publish workspaces that have windows or are on screen; the spare empty
//! ones at the strip's ends are an implementation detail. Names are their
//! 1-based position, "active" means on a monitor right now, and activating
//! one slides the strip to bring it onto the main monitor.

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
    group: ExtWorkspaceGroupHandleV1,
    handles: Vec<(ExtWorkspaceHandleV1, Info)>,
    outputs: Vec<WlOutput>,
    /// Set by `activate`, applied on `commit` (requests are atomic per commit).
    pending_activate: Option<WorkspaceId>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Info {
    id: WorkspaceId,
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
        let base = self
            .workspaces
            .motion
            .as_ref()
            .map_or(self.workspaces.active, |m| m.to);
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
        let outputs: Vec<Output> = self.space.outputs().cloned().collect();
        let dh = self.display_handle.clone();

        for client in &mut self.ext_workspace_state.clients {
            let Some(wl_client) = client.manager.client() else {
                continue;
            };
            let mut changed = false;

            // Monitors (the group spans all of them).
            let current: Vec<WlOutput> = outputs
                .iter()
                .flat_map(|o| o.client_outputs(&wl_client))
                .collect();
            for output in &current {
                if !client.outputs.contains(output) {
                    client.group.output_enter(output);
                    changed = true;
                }
            }
            for output in &client.outputs {
                if !current.contains(output) && output.is_alive() {
                    client.group.output_leave(output);
                    changed = true;
                }
            }
            client.outputs = current;

            // Workspaces that went away.
            client.handles.retain(|(handle, info)| {
                if infos.iter().any(|i| i.id == info.id) {
                    return true;
                }
                if handle.is_alive() {
                    client.group.workspace_leave(handle);
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
                client.group.workspace_enter(&handle);
                client.handles.push((handle, info.clone()));
                changed = true;
            }

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
        let Some(index) = self.workspaces.list.iter().position(|ws| ws.id == id) else {
            return;
        };
        let current = self
            .workspaces
            .motion
            .as_ref()
            .map_or(self.workspaces.active, |m| m.to);
        let step = index as isize - current as isize;
        if step != 0 {
            self.step_workspace(step, false);
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
    fn bind(
        state: &mut Self,
        dh: &DisplayHandle,
        client: &Client,
        resource: New<ExtWorkspaceManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        let manager = data_init.init(resource, ());
        let group = match client.create_resource::<ExtWorkspaceGroupHandleV1, (), State>(
            dh,
            manager.version(),
            (),
        ) {
            Ok(group) => group,
            Err(err) => {
                warn!(?err, "failed to create workspace group");
                return;
            }
        };
        manager.workspace_group(&group);
        // Workspaces come and go by themselves; clients can't create them.
        group.capabilities(GroupCapabilities::empty());
        state.ext_workspace_state.clients.push(ClientWorkspaces {
            manager,
            group,
            handles: Vec::new(),
            outputs: Vec::new(),
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
