//! The notification centre's memory: what apps sent, newest first, until
//! the user clears it. Transient notifications (the `transient` hint) never
//! get in. Pure data, drawn by the calendar popover.

/// Most kept; the oldest go first.
pub const CAP: usize = 100;

#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    /// Ours alone; the app's notification id can repeat or be replaced.
    pub seq: u64,
    /// The id the app knows it by (for `ActionInvoked`).
    pub id: u32,
    pub app_name: String,
    /// An icon name or a path.
    pub icon: String,
    pub summary: String,
    pub body: String,
    /// Seconds since the epoch.
    pub at: u64,
    /// (key, label).
    pub actions: Vec<(String, String)>,
    pub desktop_entry: Option<String>,
}

impl Entry {
    pub fn has_default(&self) -> bool {
        self.actions.iter().any(|(key, _)| key == "default")
    }

    /// What the group header calls it.
    pub fn app(&self) -> &str {
        if self.app_name.is_empty() {
            "Notification"
        } else {
            &self.app_name
        }
    }
}

#[derive(Default)]
pub struct History {
    /// Newest first.
    entries: Vec<Entry>,
    next_seq: u64,
    /// Received since the popover was last opened.
    unread: usize,
}

/// One app's notifications, newest first.
#[derive(Debug, PartialEq)]
pub struct Group<'a> {
    pub app: &'a str,
    pub icon: &'a str,
    pub entries: Vec<&'a Entry>,
}

impl History {
    /// Remember one. An update of a notification still listed (same app,
    /// same id) takes its place at the top. Transient ones are kept out.
    pub fn push(&mut self, mut entry: Entry, transient: bool) {
        if transient {
            return;
        }
        entry.seq = self.next_seq;
        self.next_seq += 1;
        self.unread += 1;
        if let Some(old) = self
            .entries
            .iter()
            .position(|e| e.id == entry.id && e.app_name == entry.app_name)
        {
            self.entries.remove(old);
        }
        self.entries.insert(0, entry);
        self.entries.truncate(CAP);
        self.unread = self.unread.min(self.entries.len());
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn unread(&self) -> bool {
        self.unread > 0
    }

    pub fn mark_read(&mut self) {
        self.unread = 0;
    }

    pub fn get(&self, seq: u64) -> Option<&Entry> {
        self.entries.iter().find(|e| e.seq == seq)
    }

    pub fn remove(&mut self, seq: u64) {
        self.entries.retain(|e| e.seq != seq);
        self.unread = self.unread.min(self.entries.len());
    }

    pub fn clear_app(&mut self, app: &str) {
        self.entries.retain(|e| e.app() != app);
        self.unread = self.unread.min(self.entries.len());
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.unread = 0;
    }

    /// By app, the app with the newest notification first.
    pub fn groups(&self) -> Vec<Group<'_>> {
        let mut groups: Vec<Group<'_>> = Vec::new();
        for entry in &self.entries {
            match groups.iter_mut().find(|g| g.app == entry.app()) {
                Some(group) => group.entries.push(entry),
                None => groups.push(Group {
                    app: entry.app(),
                    icon: &entry.icon,
                    entries: vec![entry],
                }),
            }
        }
        groups
    }
}

pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// "Just now", "5 min ago", "2 h ago", "Yesterday", "3 days ago".
pub fn relative(now: u64, at: u64) -> String {
    let secs = now.saturating_sub(at);
    match secs {
        0..=59 => "Just now".into(),
        60..=3599 => format!("{} min ago", secs / 60),
        3600..=86399 => format!("{} h ago", secs / 3600),
        86400..=172799 => "Yesterday".into(),
        _ => format!("{} days ago", secs / 86400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: u32, app: &str, summary: &str) -> Entry {
        Entry {
            seq: 0,
            id,
            app_name: app.into(),
            icon: String::new(),
            summary: summary.into(),
            body: String::new(),
            at: 0,
            actions: Vec::new(),
            desktop_entry: None,
        }
    }

    #[test]
    fn groups_by_app_newest_first() {
        let mut h = History::default();
        h.push(entry(1, "Mail", "a"), false);
        h.push(entry(2, "Chat", "b"), false);
        h.push(entry(3, "Mail", "c"), false);
        let groups = h.groups();
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].app, "Mail");
        assert_eq!(groups[0].entries[0].summary, "c");
        assert_eq!(groups[0].entries[1].summary, "a");
        assert_eq!(groups[1].app, "Chat");
    }

    #[test]
    fn caps_and_drops_the_oldest() {
        let mut h = History::default();
        for i in 0..(CAP as u32 + 20) {
            h.push(entry(i + 1, "App", &i.to_string()), false);
        }
        assert_eq!(h.len(), CAP);
        let groups = h.groups();
        assert_eq!(groups[0].entries[0].summary, (CAP + 19).to_string());
        assert_eq!(groups[0].entries.last().unwrap().summary, "20");
    }

    #[test]
    fn transient_is_not_kept() {
        let mut h = History::default();
        h.push(entry(1, "Vol", "50%"), true);
        assert!(h.is_empty());
        assert!(!h.unread());
    }

    #[test]
    fn replacement_updates_in_place() {
        let mut h = History::default();
        h.push(entry(1, "Player", "Song A"), false);
        h.push(entry(2, "Mail", "x"), false);
        h.push(entry(1, "Player", "Song B"), false);
        assert_eq!(h.len(), 2);
        assert_eq!(h.groups()[0].entries[0].summary, "Song B");
    }

    #[test]
    fn clearing_and_unread() {
        let mut h = History::default();
        h.push(entry(1, "A", "a"), false);
        h.push(entry(2, "B", "b"), false);
        assert!(h.unread());
        h.mark_read();
        assert!(!h.unread());
        h.clear_app("A");
        assert_eq!(h.len(), 1);
        let seq = h.groups()[0].entries[0].seq;
        assert!(h.get(seq).is_some());
        h.remove(seq);
        assert!(h.is_empty());
    }

    #[test]
    fn relative_times() {
        assert_eq!(relative(100, 100), "Just now");
        assert_eq!(relative(400, 100), "5 min ago");
        assert_eq!(relative(100 + 7200, 100), "2 h ago");
        assert_eq!(relative(100 + 86400, 100), "Yesterday");
        assert_eq!(relative(100 + 3 * 86400, 100), "3 days ago");
        // A clock that went backwards doesn't underflow.
        assert_eq!(relative(10, 100), "Just now");
    }
}
