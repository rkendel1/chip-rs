//! Compatibility rule for configuration: a key or section this release does not know is a
//! warning, never an error. A file written for a newer release (or one with a typo) must still
//! load, so rolling deployments can change configuration before code.

use courier::config::{self, SECTIONS};

#[test]
fn unknown_keys_in_every_section_warn_and_continue() {
    for spec in SECTIONS {
        let text = format!("[{}]\nsome_future_key = 1\n", spec.name);
        let loaded = config::load(&text, &[])
            .unwrap_or_else(|e| panic!("[{}] with an unknown key must still load: {e}", spec.name));
        let expected = format!("[{}] unknown key `some_future_key` ignored", spec.name);
        assert!(
            loaded.warnings.contains(&expected),
            "[{}] should warn about the unknown key; warnings were {:?}",
            spec.name,
            loaded.warnings
        );
    }
}

#[test]
fn unknown_sections_warn_and_continue() {
    let loaded = config::load("[from_the_future]\nx = 1\n", &[]).unwrap();
    assert_eq!(loaded.warnings, vec!["unknown section [from_the_future] ignored"]);
}

#[test]
fn known_keys_never_warn() {
    for spec in SECTIONS {
        // Setting a key to the value the defaults already give it must not produce a warning.
        let defaults = config::defaults::default_raw();
        for key in spec.keys {
            if let Some(v) = defaults.get(spec.name, key) {
                let loaded = config::load(&format!("[{}]\n{key} = {v}\n", spec.name), &[]).unwrap();
                assert!(loaded.warnings.is_empty(), "[{}] {key}", spec.name);
            }
        }
    }
}
