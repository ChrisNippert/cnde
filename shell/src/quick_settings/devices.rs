//! The Bluetooth devices list under quick settings' Bluetooth tile: the
//! paired devices, a row each. Rows are kept by device, so a row that's
//! still there glides to its new place when others come or go, a new one
//! fades in, and one that's gone fades out where it was while the rows
//! below glide up to close the gap. The list opens and folds up like the
//! audio lists, and a connection change cross-fades the status line.

use cnde_ui::{Animator, anim::MEDIUM};

use super::{DEVICE_ROW_HEIGHT, Moving, Rect, audio_list};
use crate::status::{Bluetooth, BluetoothDevice};

/// More devices than this are left to the Settings page.
const MAX_DEVICES: usize = 6;

/// The devices the list shows: the paired ones, while Bluetooth is on.
pub fn shown(bluetooth: Option<&Bluetooth>) -> &[BluetoothDevice] {
    match bluetooth {
        Some(b) if b.powered => &b.devices[..b.devices.len().min(MAX_DEVICES)],
        _ => &[],
    }
}

/// One device's row, as it's drawn: where it's headed and how far it's got.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    /// Keys the row's animations, and stays with the device while it's listed.
    pub id: u32,
    /// The device as last reported. A row that's leaving is drawn from this.
    pub device: BluetoothDevice,
    /// Its place in the status's devices, while it's still there (None:
    /// it's leaving and fading out).
    pub index: Option<usize>,
    /// The slot (in rows, from the top) it's headed for. A row that's
    /// leaving keeps the slot it had.
    pub target: f32,
    /// The slot it's at now.
    pub slot: f32,
    /// 0-1: how far in it is (it fades in, and out when it leaves).
    pub presence: f32,
    /// 0-1: connected, eased (the status line cross-fades on it).
    pub link: f32,
}

/// The Bluetooth devices list's rows and how the list is open.
#[derive(Debug, Clone, Default)]
pub struct DeviceRows {
    /// The list is open (or opening): its target.
    pub open: bool,
    /// 0-1: how far open the list is now.
    pub progress: f32,
    /// How many rows the list is tall for now (eases as rows come and go).
    pub count: f32,
    /// Top to bottom: the devices, and any that have just left.
    pub rows: Vec<Row>,
    next_id: u32,
}

/// The list as it's drawn this frame: where it is, and each row's place.
#[derive(Debug, Clone, PartialEq)]
pub struct DeviceList {
    /// The part showing: the header, and the rows as far as it's opened.
    pub tile: Rect,
    pub header: Rect,
    /// 0-1: how far open (the header's arrow turns on it).
    pub open: f32,
    pub rows: Vec<PlacedRow>,
}

/// A device row where it's drawn.
#[derive(Debug, Clone, PartialEq)]
pub struct PlacedRow {
    pub rect: Rect,
    /// Its place in the status's devices, while it's there. A row that's
    /// leaving has none, so it doesn't take clicks.
    pub index: Option<usize>,
    pub device: BluetoothDevice,
    /// 0-1: how far in the row is, with its share of the list's opening.
    pub alpha: f32,
    /// 0-1: connected, for the status line to cross-fade on.
    pub link: f32,
}

impl DeviceRows {
    /// Rows for `shown`, settled: no animation to wait for. For tests and
    /// the preview.
    #[cfg(test)]
    pub fn settled(shown: &[BluetoothDevice], open: bool) -> Self {
        let mut rows = Self::default();
        rows.sync(shown);
        rows.open = open;
        rows.progress = if open { 1.0 } else { 0.0 };
        rows.count = rows.shown_count() as f32;
        for row in &mut rows.rows {
            row.slot = row.target;
            row.presence = 1.0;
            row.link = if row.device.connected { 1.0 } else { 0.0 };
        }
        rows
    }

    /// How many devices are listed (not leaving).
    pub fn shown_count(&self) -> usize {
        self.rows.iter().filter(|r| r.index.is_some()).count()
    }

    /// Bring the rows in line with the devices `shown`. Returns the ids of
    /// the new rows, which the caller fades in.
    pub fn sync(&mut self, shown: &[BluetoothDevice]) -> Vec<u32> {
        let mut old: Vec<(usize, Row)> = std::mem::take(&mut self.rows)
            .into_iter()
            .enumerate()
            .collect();
        let mut fresh = Vec::new();
        let mut present = Vec::new();
        for (i, device) in shown.iter().enumerate() {
            let mut row = match old.iter().position(|(_, r)| r.device.address == device.address) {
                Some(at) => old.remove(at).1,
                None => {
                    let id = self.next_id;
                    self.next_id += 1;
                    fresh.push(id);
                    Row {
                        id,
                        device: device.clone(),
                        index: None,
                        target: 0.0,
                        slot: 0.0,
                        presence: 0.0,
                        link: 0.0,
                    }
                }
            };
            row.device = device.clone();
            row.index = Some(i);
            present.push(row);
        }
        // Rows that have gone stay where they were, fading, and the rest
        // close round them.
        let mut left: Vec<(usize, Row)> = old
            .into_iter()
            .map(|(at, mut row)| {
                row.index = None;
                (at, row)
            })
            .collect();
        left.sort_by_key(|(at, _)| *at);
        let mut left = left.into_iter().peekable();
        let mut present = present.into_iter().peekable();
        let mut rows = Vec::new();
        loop {
            let due = left.peek().is_some_and(|(at, _)| *at <= rows.len());
            let next = if due || present.peek().is_none() {
                left.next().map(|(_, row)| row)
            } else {
                present.next()
            };
            match next {
                Some(row) => rows.push(row),
                None => break,
            }
        }
        self.rows = rows;
        self.aim();
        for row in self.rows.iter_mut().filter(|r| fresh.contains(&r.id)) {
            row.slot = row.target;
        }
        fresh
    }

    /// Where each shown row is headed: its place among the shown ones.
    fn aim(&mut self) {
        let mut place = 0.0;
        for row in self.rows.iter_mut().filter(|r| r.index.is_some()) {
            row.target = place;
            place += 1.0;
        }
    }

    /// Move everything to where the animator has it this frame, and drop
    /// the rows that have faded out.
    pub fn animate(&mut self, anim: &mut Animator<Moving>) {
        let shown = self.shown_count() as f32;
        self.progress = anim.get(
            Moving::Devices,
            if self.open { 1.0 } else { 0.0 },
            MEDIUM,
        );
        self.count = anim.get(Moving::DevicesCount, shown, MEDIUM);
        self.rows.retain(|row| {
            row.index.is_some() || anim.get(Moving::DevicePresence(row.id), 0.0, MEDIUM) > 0.0
        });
        for row in &mut self.rows {
            let here = row.index.is_some();
            row.slot = anim.get(Moving::DeviceSlot(row.id), row.target, MEDIUM);
            row.presence = anim.get(
                Moving::DevicePresence(row.id),
                if here { 1.0 } else { 0.0 },
                MEDIUM,
            );
            let connected = if row.device.connected { 1.0 } else { 0.0 };
            row.link = anim.get(Moving::DeviceLink(row.id), connected, MEDIUM);
        }
    }

    /// The list hanging from (`x`, `y`), `w` wide, as far open as it is.
    pub fn list(&self, x: f32, y: f32, w: f32) -> DeviceList {
        let open = self.progress.clamp(0.0, 1.0);
        let header = Rect {
            x,
            y,
            w,
            h: DEVICE_ROW_HEIGHT,
        };
        let tile = Rect {
            h: DEVICE_ROW_HEIGHT * (1.0 + self.count.max(0.0) * open),
            ..header
        };
        let rows = self
            .rows
            .iter()
            .enumerate()
            .map(|(n, row)| PlacedRow {
                rect: Rect {
                    x,
                    y: y + (row.slot + 1.0) * DEVICE_ROW_HEIGHT,
                    w,
                    h: DEVICE_ROW_HEIGHT,
                },
                index: row.index,
                device: row.device.clone(),
                alpha: row.presence.clamp(0.0, 1.0) * audio_list::row_progress(open, n),
                link: row.link,
            })
            .collect();
        DeviceList {
            tile,
            header,
            open,
            rows,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn device(address: &str, connected: bool) -> BluetoothDevice {
        BluetoothDevice {
            address: address.into(),
            name: address.into(),
            icon: String::new(),
            connected,
            battery: None,
        }
    }

    fn addresses(rows: &DeviceRows) -> Vec<&str> {
        rows.rows.iter().map(|r| r.device.address.as_str()).collect()
    }

    #[test]
    fn shown_is_the_paired_devices_while_powered() {
        let mut bluetooth = Bluetooth {
            powered: false,
            devices: (0..20).map(|i| device(&i.to_string(), false)).collect(),
        };
        assert!(shown(None).is_empty());
        assert!(shown(Some(&bluetooth)).is_empty());
        bluetooth.powered = true;
        assert_eq!(shown(Some(&bluetooth)).len(), MAX_DEVICES);
    }

    #[test]
    fn rows_keep_their_identity_when_others_come_and_go() {
        let mut rows = DeviceRows::default();
        assert_eq!(rows.sync(&[device("a", false), device("b", false)]).len(), 2);
        let (a, b) = (rows.rows[0].id, rows.rows[1].id);
        // A new one in front: the others keep their rows and move down.
        let fresh = rows.sync(&[device("c", false), device("a", false), device("b", false)]);
        assert_eq!(fresh.len(), 1);
        assert_eq!(addresses(&rows), ["c", "a", "b"]);
        assert_eq!(rows.rows[1].id, a);
        assert_eq!(rows.rows[2].id, b);
        assert_eq!(rows.rows[2].target, 2.0);
        // Reordered: nothing new.
        let fresh = rows.sync(&[device("b", false), device("c", false), device("a", false)]);
        assert!(fresh.is_empty());
        assert_eq!(addresses(&rows), ["b", "c", "a"]);
        assert_eq!(rows.rows[0].target, 0.0);
        assert_eq!(rows.rows[2].target, 2.0);
    }

    #[test]
    fn a_row_that_goes_keeps_its_place_and_the_rest_close_round_it() {
        let mut rows = DeviceRows::settled(&[device("a", false), device("b", false), device("c", false)], true);
        rows.sync(&[device("a", false), device("c", false)]);
        assert_eq!(addresses(&rows), ["a", "b", "c"]);
        assert_eq!(rows.rows[1].index, None);
        assert_eq!(rows.rows[1].target, 1.0);
        // c glides up into the gap.
        assert_eq!(rows.rows[2].target, 1.0);
        assert_eq!(rows.shown_count(), 2);
    }

    #[test]
    fn a_device_back_in_its_place_is_the_same_row() {
        let mut rows = DeviceRows::settled(&[device("a", false), device("b", false)], true);
        let id = rows.rows[1].id;
        rows.sync(&[device("a", false)]);
        let fresh = rows.sync(&[device("a", false), device("b", false)]);
        assert!(fresh.is_empty());
        assert_eq!(rows.rows[1].id, id);
        assert_eq!(rows.rows[1].index, Some(1));
    }

    #[test]
    fn rows_fade_in_and_out_over_time() {
        let mut rows = DeviceRows::default();
        let fresh = rows.sync(&[device("a", false)]);
        let mut anim = Animator::new();
        // Starting the fade the way quick settings does when a row arrives.
        anim.restart(Moving::DevicePresence(fresh[0]), 0.0, 1.0, MEDIUM);
        anim.begin();
        rows.animate(&mut anim);
        assert!(rows.rows[0].presence < 0.5, "fading in from nothing");
        std::thread::sleep(MEDIUM + Duration::from_millis(10));
        anim.begin();
        rows.animate(&mut anim);
        assert_eq!(rows.rows[0].presence, 1.0);

        // Gone: still there (fading out) until the fade is over.
        rows.sync(&[]);
        anim.begin();
        rows.animate(&mut anim);
        assert_eq!(rows.rows.len(), 1);
        assert!(rows.rows[0].presence > 0.9);
        std::thread::sleep(MEDIUM + Duration::from_millis(10));
        anim.begin();
        rows.animate(&mut anim);
        assert!(rows.rows.is_empty());
    }

    #[test]
    fn list_puts_rows_under_the_header_and_glides() {
        let mut rows = DeviceRows::settled(&[device("a", false), device("b", false)], true);
        let list = rows.list(10.0, 100.0, 300.0);
        assert_eq!(list.header.y, 100.0);
        assert_eq!(list.tile.h, DEVICE_ROW_HEIGHT * 3.0);
        assert_eq!(list.rows[1].rect.y, 100.0 + 2.0 * DEVICE_ROW_HEIGHT);
        // Halfway through a glide, the row is between its two places.
        rows.rows[1].slot = 0.5;
        let list = rows.list(10.0, 100.0, 300.0);
        assert_eq!(list.rows[1].rect.y, 100.0 + 1.5 * DEVICE_ROW_HEIGHT);
        // Closed, the list is only its header.
        rows.progress = 0.0;
        assert_eq!(rows.list(10.0, 100.0, 300.0).tile.h, DEVICE_ROW_HEIGHT);
    }
}
