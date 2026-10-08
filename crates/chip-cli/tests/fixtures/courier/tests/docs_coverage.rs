//! The configuration reference must describe every section and key the parser accepts.

use courier::config::SECTIONS;

fn reference() -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/configuration.md");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

#[test]
fn every_section_has_a_heading() {
    let doc = reference();
    for s in SECTIONS {
        assert!(
            doc.lines().any(|l| l == format!("## [{}]", s.name)),
            "docs/configuration.md has no `## [{}]` heading",
            s.name
        );
    }
}

#[test]
fn every_key_is_documented_under_its_section() {
    let doc = reference();
    for s in SECTIONS {
        let start = doc
            .find(&format!("## [{}]", s.name))
            .unwrap_or_else(|| panic!("no section [{}]", s.name));
        let rest = &doc[start + 1..];
        let end = rest.find("\n## ").map_or(doc.len(), |i| start + 1 + i);
        let body = &doc[start..end];
        for key in s.keys {
            assert!(
                body.contains(&format!("`{key}`")),
                "[{}] key `{key}` is not documented",
                s.name
            );
        }
    }
}
