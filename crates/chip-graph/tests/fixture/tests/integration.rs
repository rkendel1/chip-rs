use fixture::api::{handle, Request};

#[test]
fn handles_a_request() {
    let request = Request { name: "x".to_string() };
    assert_eq!(handle(&request), "X");
}
