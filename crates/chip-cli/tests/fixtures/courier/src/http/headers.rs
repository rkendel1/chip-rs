//! Case-insensitive, order-preserving header list.

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Headers {
    entries: Vec<(String, String)>,
}

impl Headers {
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends a header, keeping any existing ones of the same name.
    pub fn append(&mut self, name: &str, value: &str) {
        self.entries.push((name.to_string(), value.to_string()));
    }

    /// Replaces every header of this name with a single one.
    pub fn set(&mut self, name: &str, value: &str) {
        self.remove(name);
        self.append(name, value);
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn get_all(&self, name: &str) -> Vec<&str> {
        self.entries
            .iter()
            .filter(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
            .collect()
    }

    pub fn contains(&self, name: &str) -> bool {
        self.get(name).is_some()
    }

    pub fn remove(&mut self, name: &str) {
        self.entries.retain(|(n, _)| !n.eq_ignore_ascii_case(name));
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.entries.iter().map(|(n, v)| (n.as_str(), v.as_str()))
    }

    /// Adds every header of `other` that this list does not already have.
    pub fn merge_missing(&mut self, other: &Headers) {
        for (n, v) in other.iter() {
            if !self.contains(n) {
                self.append(n, v);
            }
        }
    }
}

impl<const N: usize> From<[(&str, &str); N]> for Headers {
    fn from(pairs: [(&str, &str); N]) -> Self {
        let mut h = Headers::new();
        for (n, v) in pairs {
            h.append(n, v);
        }
        h
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_is_case_insensitive() {
        let h = Headers::from([("Content-Type", "text/plain")]);
        assert_eq!(h.get("content-type"), Some("text/plain"));
        assert_eq!(h.get("CONTENT-TYPE"), Some("text/plain"));
        assert_eq!(h.get("accept"), None);
    }

    #[test]
    fn append_keeps_duplicates_and_set_collapses_them() {
        let mut h = Headers::new();
        h.append("X-A", "1");
        h.append("x-a", "2");
        assert_eq!(h.get_all("X-A"), vec!["1", "2"]);
        h.set("X-A", "3");
        assert_eq!(h.get_all("x-a"), vec!["3"]);
        assert_eq!(h.len(), 1);
    }

    #[test]
    fn remove_is_case_insensitive() {
        let mut h = Headers::from([("A", "1"), ("B", "2")]);
        h.remove("a");
        assert!(!h.contains("A"));
        assert!(h.contains("B"));
    }

    #[test]
    fn merge_missing_does_not_overwrite() {
        let mut h = Headers::from([("A", "mine")]);
        h.merge_missing(&Headers::from([("a", "theirs"), ("B", "new")]));
        assert_eq!(h.get("A"), Some("mine"));
        assert_eq!(h.get("B"), Some("new"));
    }

    #[test]
    fn order_is_preserved() {
        let h = Headers::from([("Z", "1"), ("A", "2")]);
        let names: Vec<_> = h.iter().map(|(n, _)| n).collect();
        assert_eq!(names, vec!["Z", "A"]);
    }

    #[test]
    fn empty() {
        let h = Headers::new();
        assert!(h.is_empty());
    }
}
