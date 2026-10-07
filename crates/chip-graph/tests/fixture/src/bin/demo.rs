use fixture::api::{handle, Request};

fn main() {
    let request = Request { name: "demo".to_string() };
    println!("{}", handle(&request));
}
