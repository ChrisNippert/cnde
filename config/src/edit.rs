//! Changing config.toml from a program (the settings app) while keeping
//! everything a person wrote in it: comments, ordering, formatting.
//!
//! Every change is validated by parsing the result as a [`Config`] before
//! it's saved, so the GUI can never write a file the compositor rejects.

use std::path::{Path, PathBuf};

use toml_edit::{ArrayOfTables, DocumentMut, Item, Table, Value};

use crate::{Config, DEFAULT_CONFIG, Error};

/// config.toml, parsed for editing.
pub struct ConfigFile {
    path: PathBuf,
    doc: DocumentMut,
}

impl ConfigFile {
    /// Read `path`; a missing file starts from [`DEFAULT_CONFIG`].
    pub fn open(path: &Path) -> Result<Self, Error> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => DEFAULT_CONFIG.into(),
            Err(err) => return Err(Error::Io(path.into(), err)),
        };
        let doc = text
            .parse::<DocumentMut>()
            .map_err(|err| Error::Invalid(format!("{}: {err}", path.display())))?;
        Ok(Self {
            path: path.into(),
            doc,
        })
    }

    /// What the file currently says, or why it's not a valid config.
    pub fn config(&self) -> Result<Config, Error> {
        Config::parse(&self.doc.to_string())
    }

    /// Set a dotted key, like "input.mouse.speed", creating tables as needed.
    pub fn set(&mut self, key: &str, value: impl Into<Value>) {
        let (tables, name) = split(key);
        let table = table_mut(self.doc.as_table_mut(), &tables);
        set_in(table, name, value.into());
    }

    /// Remove a dotted key, so the default applies again.
    pub fn remove(&mut self, key: &str) {
        let (tables, name) = split(key);
        let mut table = self.doc.as_table_mut();
        for part in tables {
            match table.get_mut(part).and_then(Item::as_table_mut) {
                Some(next) => table = next,
                None => return,
            }
        }
        table.remove(name);
    }

    /// Set `key` in the `[[output]]` entry named `output`, adding one if
    /// there isn't one yet.
    pub fn set_output(&mut self, output: &str, key: &str, value: impl Into<Value>) {
        let outputs = self.outputs_mut();
        let index = match find_output(outputs, output) {
            Some(index) => index,
            None => {
                let mut table = Table::new();
                table.insert("name", toml_edit::value(output));
                outputs.push(table);
                outputs.len() - 1
            }
        };
        if let Some(table) = outputs.get_mut(index) {
            set_in(table, key, value.into());
        }
    }

    /// Remove `key` from the `[[output]]` entry named `output`; an entry left
    /// with only its name goes too.
    pub fn remove_output(&mut self, output: &str, key: &str) {
        let outputs = self.outputs_mut();
        let Some(index) = find_output(outputs, output) else {
            return;
        };
        if let Some(table) = outputs.get_mut(index) {
            table.remove(key);
            if table.len() <= 1 {
                outputs.remove(index);
            }
        }
        if outputs.is_empty() {
            self.doc.remove("output");
        }
    }

    fn outputs_mut(&mut self) -> &mut ArrayOfTables {
        let item = self
            .doc
            .entry("output")
            .or_insert_with(|| Item::ArrayOfTables(ArrayOfTables::new()));
        if !item.is_array_of_tables() {
            *item = Item::ArrayOfTables(ArrayOfTables::new());
        }
        item.as_array_of_tables_mut()
            .expect("just made it an array of tables")
    }

    /// Write the file if it's a valid config; on error nothing is written.
    ///
    /// Saved to a temporary file and renamed over the original, so readers
    /// never see half a file. A symlinked config (dotfiles) is followed, and
    /// the file it points to is replaced, not the link.
    pub fn save(&self) -> Result<Config, Error> {
        let text = self.doc.to_string();
        let config = Config::parse(&text)?;
        let target = std::fs::canonicalize(&self.path).unwrap_or_else(|_| self.path.clone());
        if let Some(dir) = target.parent() {
            std::fs::create_dir_all(dir).map_err(|err| Error::Io(dir.into(), err))?;
        }
        let mut temp = target.clone().into_os_string();
        temp.push(".new");
        let temp = PathBuf::from(temp);
        std::fs::write(&temp, &text).map_err(|err| Error::Io(temp.clone(), err))?;
        std::fs::rename(&temp, &target).map_err(|err| Error::Io(target.clone(), err))?;
        Ok(config)
    }

    pub fn text(&self) -> String {
        self.doc.to_string()
    }
}

fn split(key: &str) -> (Vec<&str>, &str) {
    let mut parts: Vec<&str> = key.split('.').collect();
    let name = parts.pop().unwrap_or(key);
    (parts, name)
}

fn table_mut<'a>(mut table: &'a mut Table, path: &[&str]) -> &'a mut Table {
    for part in path {
        let item = table.entry(part).or_insert_with(|| {
            let mut new = Table::new();
            // [input] stays implicit when only [input.mouse] is written.
            new.set_implicit(true);
            Item::Table(new)
        });
        if !item.is_table() {
            *item = Item::Table(Table::new());
        }
        table = item.as_table_mut().expect("just made it a table");
    }
    table
}

/// Replace a value but keep its comment ("speed = 0.5   # -1.0 to 1.0").
fn set_in(table: &mut Table, key: &str, mut value: Value) {
    if let Some(old) = table.get(key).and_then(Item::as_value) {
        *value.decor_mut() = old.decor().clone();
    }
    table.insert(key, Item::Value(value));
}

fn find_output(outputs: &ArrayOfTables, name: &str) -> Option<usize> {
    outputs
        .iter()
        .position(|t| t.get("name").and_then(Item::as_str) == Some(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(text: &str) -> ConfigFile {
        ConfigFile {
            path: PathBuf::from("/nonexistent/config.toml"),
            doc: text.parse().unwrap(),
        }
    }

    #[test]
    fn set_keeps_comments() {
        let mut f = file("# mine\n[input.mouse]\nspeed = 0.0 # fast?\n");
        f.set("input.mouse.speed", 0.5);
        assert_eq!(f.text(), "# mine\n[input.mouse]\nspeed = 0.5 # fast?\n");
        assert_eq!(f.config().unwrap().input.mouse.speed, 0.5);
    }

    #[test]
    fn set_creates_tables() {
        let mut f = file("");
        f.set("input.touchpad.tap_to_click", false);
        assert_eq!(f.text(), "[input.touchpad]\ntap_to_click = false\n");
    }

    #[test]
    fn workspace_mode_is_saved() {
        let mut f = file(DEFAULT_CONFIG);
        f.set("workspaces.mode", "individual");
        let config = f.config().unwrap();
        assert_eq!(config.workspaces.mode, crate::WorkspaceMode::Individual);
        f.set("workspaces.mode", "sideways");
        assert!(f.config().is_err());
    }

    #[test]
    fn set_works_on_the_default_file() {
        let mut f = file(DEFAULT_CONFIG);
        f.set("dock.position", "left");
        f.set("panel.height", 40);
        let config = f.config().unwrap();
        assert_eq!(config.dock.position, crate::DockPosition::Left);
        assert_eq!(config.panel.height, 40);
        // Everything else is untouched.
        assert!(f.text().contains("# clock_format ="));
    }

    #[test]
    fn remove_restores_default() {
        let mut f = file("[animations]\nspeed = 2.0\n");
        f.remove("animations.speed");
        f.remove("not.there");
        assert_eq!(f.config().unwrap(), Config::default());
    }

    #[test]
    fn outputs() {
        let mut f = file("[[output]]\nname = \"DP-1\"\nscale = 2.0\n");
        f.set_output("HDMI-A-1", "scale", 1.5);
        f.set_output("DP-1", "main", true);
        let config = f.config().unwrap();
        assert_eq!(config.output("HDMI-A-1").unwrap().scale, Some(1.5));
        assert!(config.output("DP-1").unwrap().main);
        f.remove_output("HDMI-A-1", "scale");
        assert!(f.config().unwrap().output("HDMI-A-1").is_none());
    }

    #[test]
    fn invalid_values_are_not_saved() {
        let mut f = file("");
        f.set("panel.height", 4000);
        assert!(f.config().is_err());
        assert!(f.save().is_err());
    }
}
