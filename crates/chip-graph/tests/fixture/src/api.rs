use crate::execution::Runner;
use helper::Summarize;

pub struct Request {
    pub name: String,
}

pub fn handle(request: &Request) -> String {
    Runner::new().run(&request.name)
}

impl Summarize for Request {
    fn summary(&self) -> String {
        format!("request {}", self.name)
    }
}

fn private_helper() {}
