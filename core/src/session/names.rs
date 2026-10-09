/// Display-name rules shared by host and player.
#[derive(Debug)]
pub struct Names;

impl Names {
    pub const MAX_LENGTH: usize = 24;

    /// Letters, digits, `_` and `-` only, at most 24 characters; "player" when nothing is left.
    pub fn sanitize(name: &str) -> String {
        let t: String =
            name.chars().filter(|c| c.is_alphanumeric() || *c == '_' || *c == '-').take(Self::MAX_LENGTH).collect();
        if t.is_empty() {
            "player".to_owned()
        } else {
            t
        }
    }

    /// The name itself when free, else `name#id`: two players never share a display name. Case-insensitive.
    pub fn unique<'a>(name: &str, id: u16, taken: impl IntoIterator<Item = &'a str>) -> String {
        let wanted = name.to_uppercase();
        if taken.into_iter().any(|t| t.to_uppercase() == wanted) {
            format!("{name}#{id}")
        } else {
            name.to_owned()
        }
    }
}
