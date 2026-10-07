pub struct Evidence;

impl Evidence {
    pub fn record(name: &str) -> String {
        helper::shout(name)
    }
}

pub enum Verdict {
    Kept,
    Dropped,
}
