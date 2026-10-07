/// Something that can be summarized.
pub trait Summarize {
    fn summary(&self) -> String;
}

pub fn shout(text: &str) -> String {
    text.to_uppercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shout_uppercases() {
        assert_eq!(shout("a"), "A");
    }
}
