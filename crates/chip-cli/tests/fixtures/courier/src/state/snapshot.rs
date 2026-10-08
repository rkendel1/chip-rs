//! Saving and restoring circuit-breaker state through the [`Store`](super::Store), so a route
//! that was open when the process stopped is still open when it starts again.

use super::store::Store;
use crate::error::CourierError;
use crate::middleware::circuit::RouteState;
use std::collections::BTreeMap;
use std::time::Duration;

const PREFIX: &str = "circuit.";

pub fn save(store: &Store, states: &BTreeMap<String, RouteState>) -> Result<(), CourierError> {
    let mut map = store.load()?;
    map.retain(|k, _| !k.starts_with(PREFIX));
    for (route, s) in states {
        let until = s.open_until.map_or("-".to_string(), |d| d.as_millis().to_string());
        map.insert(
            format!("{PREFIX}{route}"),
            format!("{} {until}", s.consecutive_failures),
        );
    }
    store.save(&map)
}

pub fn load(store: &Store) -> Result<BTreeMap<String, RouteState>, CourierError> {
    let mut out = BTreeMap::new();
    for (key, value) in store.load()? {
        let Some(route) = key.strip_prefix(PREFIX) else {
            continue;
        };
        let bad = || CourierError::io(format!("snapshot entry `{key}` is malformed: `{value}`"));
        let (failures, until) = value.split_once(' ').ok_or_else(bad)?;
        let open_until = match until {
            "-" => None,
            ms => Some(Duration::from_millis(ms.parse().map_err(|_| bad())?)),
        };
        out.insert(
            route.to_string(),
            RouteState {
                consecutive_failures: failures.parse().map_err(|_| bad())?,
                open_until,
                probing: false,
            },
        );
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("courier-snap-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("state.db")
    }

    #[test]
    fn round_trip() {
        let store = Store::new(&temp("rt"));
        let mut states = BTreeMap::new();
        states.insert(
            "slow".to_string(),
            RouteState {
                consecutive_failures: 4,
                open_until: Some(Duration::from_millis(1500)),
                probing: false,
            },
        );
        states.insert("fast".to_string(), RouteState::default());
        save(&store, &states).unwrap();
        assert_eq!(load(&store).unwrap(), states);
    }

    #[test]
    fn unrelated_keys_survive_a_save() {
        let store = Store::new(&temp("keep"));
        let mut m = BTreeMap::new();
        m.insert("note".to_string(), "hi".to_string());
        store.save(&m).unwrap();
        save(&store, &BTreeMap::new()).unwrap();
        assert_eq!(store.load().unwrap().get("note").map(String::as_str), Some("hi"));
    }

    #[test]
    fn malformed_entries_are_errors() {
        let store = Store::new(&temp("bad"));
        let mut m = BTreeMap::new();
        m.insert("circuit.x".to_string(), "not-a-state".to_string());
        store.save(&m).unwrap();
        assert!(load(&store).is_err());
    }
}
