use crate::evidence::Evidence;

pub struct Runner;

impl Runner {
    pub fn new() -> Runner {
        Runner
    }

    pub fn run(&self, name: &str) -> String {
        Evidence::record(name)
    }

    fn internal(&self) {}
}

impl Default for Runner {
    fn default() -> Self {
        Runner::new()
    }
}
