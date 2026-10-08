//! A small key/value file with atomic replacement: written beside the target and renamed.

use super::codec::{decode_line, encode_line};
use crate::error::CourierError;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub struct Store {
    path: PathBuf,
}

impl Store {
    pub fn new(path: &Path) -> Self {
        Self {
            path: path.to_path_buf(),
        }
    }

    /// An absent file is an empty store. A present file that does not parse is an error.
    pub fn load(&self) -> Result<BTreeMap<String, String>, CourierError> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
            Err(e) => return Err(e.into()),
        };
        let mut map = BTreeMap::new();
        for (i, line) in text.lines().enumerate() {
            if line.is_empty() {
                continue;
            }
            let f = decode_line(line)
                .map_err(|e| CourierError::io(format!("{}: line {}: {e}", self.path.display(), i + 1)))?;
            if f.len() != 2 {
                return Err(CourierError::io(format!(
                    "{}: line {}: expected key and value",
                    self.path.display(),
                    i + 1
                )));
            }
            map.insert(f[0].clone(), f[1].clone());
        }
        Ok(map)
    }

    pub fn save(&self, map: &BTreeMap<String, String>) -> Result<(), CourierError> {
        if let Some(dir) = self.path.parent() {
            if !dir.as_os_str().is_empty() {
                std::fs::create_dir_all(dir)?;
            }
        }
        let mut text = String::new();
        for (k, v) in map {
            text.push_str(&encode_line(&[k, v]));
            text.push('\n');
        }
        let tmp = self.path.with_extension("tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("courier-store-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("state.db")
    }

    #[test]
    fn missing_file_is_empty() {
        assert!(Store::new(&temp("missing")).load().unwrap().is_empty());
    }

    #[test]
    fn save_then_load() {
        let s = Store::new(&temp("roundtrip"));
        let mut m = BTreeMap::new();
        m.insert("a".to_string(), "1\t2".to_string());
        m.insert("b".to_string(), String::new());
        s.save(&m).unwrap();
        assert_eq!(s.load().unwrap(), m);
    }

    #[test]
    fn save_replaces_the_previous_contents() {
        let s = Store::new(&temp("replace"));
        let mut m = BTreeMap::new();
        m.insert("a".to_string(), "1".to_string());
        s.save(&m).unwrap();
        m.clear();
        m.insert("b".to_string(), "2".to_string());
        s.save(&m).unwrap();
        assert_eq!(s.load().unwrap().len(), 1);
    }

    #[test]
    fn malformed_files_are_errors() {
        let path = temp("malformed");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "only-one-field\n").unwrap();
        assert!(Store::new(&path).load().is_err());
    }
}
