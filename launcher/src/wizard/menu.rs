//! The numbered menu: what a typed line means, and the lines that draw it.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Choice {
    Install,
    Uninstall,
    CheckVersion,
    StartGame,
    OpenLogs,
    Diagnostics,
    Exit,
}

/// The menu in the order it is drawn: the choice, the key that selects it, its label. One table for drawing and
/// parsing, so the two cannot disagree.
const MENU: &[(Choice, &str, &str)] = &[
    (Choice::Install, "1", "Install or repair FeverLink3"),
    (Choice::Uninstall, "2", "Uninstall FeverLink3"),
    (Choice::CheckVersion, "3", "Check this folder's version against the installed one"),
    (Choice::StartGame, "4", "Start Transport Fever 3 through Steam"),
    (Choice::OpenLogs, "5", "Open the log folder"),
    (Choice::Diagnostics, "6", "Copy a diagnostics text to the clipboard"),
    (Choice::Exit, "0", "Exit"),
];

/// What a typed line means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Typed {
    Choice(Choice),
    /// Only Enter.
    Nothing,
    /// Anything else, as typed.
    Unknown(String),
}

/// Reads one typed line: the menu keys, `q`, `quit` and `exit` for leaving, a trailing `.` or `)` ignored, digits
/// from Arabic and full-width keyboard layouts accepted. Never fails; unknown text is handed back.
pub(super) fn parse_choice(line: &str) -> Typed {
    let text = line.trim();
    if text.is_empty() {
        return Typed::Nothing;
    }
    let key = ascii_digits(text.trim_end_matches(['.', ')'])).to_lowercase();
    if let Some((choice, _, _)) = MENU.iter().find(|(_, k, _)| *k == key) {
        return Typed::Choice(*choice);
    }
    match key.as_str() {
        "q" | "quit" | "exit" => Typed::Choice(Choice::Exit),
        _ => Typed::Unknown(text.to_owned()),
    }
}

/// Arabic-Indic, extended Arabic-Indic and full-width digits as ASCII digits, everything else unchanged. A player
/// whose keyboard is set to Arabic can type `١` for `1`; the menu should not answer "not on the menu".
fn ascii_digits(text: &str) -> String {
    text.chars()
        .map(|c| {
            let code = u32::from(c);
            let value = match code {
                0x0660..=0x0669 => code - 0x0660,
                0x06F0..=0x06F9 => code - 0x06F0,
                0xFF10..=0xFF19 => code - 0xFF10,
                _ => return c,
            };
            char::from_digit(value, 10).unwrap_or(c)
        })
        .collect()
}

pub(super) fn menu_lines() -> Vec<String> {
    let mut lines = vec!["What would you like to do?".to_owned()];
    lines.extend(MENU.iter().map(|(_, key, label)| format!("  {key}  {label}")));
    lines.push(String::new());
    lines.push("Type a number and press Enter.".to_owned());
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_keys_parse_and_other_text_is_handed_back() {
        let keys = [
            ("1", Choice::Install),
            ("2", Choice::Uninstall),
            ("3", Choice::CheckVersion),
            ("4", Choice::StartGame),
            ("5", Choice::OpenLogs),
            ("6", Choice::Diagnostics),
            ("0", Choice::Exit),
        ];
        for (key, choice) in keys {
            assert_eq!(parse_choice(key), Typed::Choice(choice), "{key}");
        }
        assert_eq!(parse_choice("  2 \r\n"), Typed::Choice(Choice::Uninstall));
        assert_eq!(parse_choice("3."), Typed::Choice(Choice::CheckVersion));
        assert_eq!(parse_choice("4)"), Typed::Choice(Choice::StartGame));
        for word in ["q", "Q", "quit", "EXIT", " exit "] {
            assert_eq!(parse_choice(word), Typed::Choice(Choice::Exit), "{word}");
        }
        assert_eq!(parse_choice(""), Typed::Nothing);
        assert_eq!(parse_choice(" \r\n"), Typed::Nothing);
        for other in ["7", "10", "-1", "install", "1 2"] {
            assert_eq!(parse_choice(other), Typed::Unknown(other.to_owned()), "{other}");
        }
    }

    #[test]
    fn digits_typed_on_other_keyboard_layouts_count() {
        assert_eq!(parse_choice("\u{0661}"), Typed::Choice(Choice::Install), "Arabic-Indic 1");
        assert_eq!(parse_choice("\u{0663}"), Typed::Choice(Choice::CheckVersion), "Arabic-Indic 3");
        assert_eq!(parse_choice("\u{0660}"), Typed::Choice(Choice::Exit), "Arabic-Indic 0");
        assert_eq!(parse_choice("\u{06F4}"), Typed::Choice(Choice::StartGame), "Persian 4");
        assert_eq!(parse_choice("\u{FF16}"), Typed::Choice(Choice::Diagnostics), "full-width 6");
        assert_eq!(ascii_digits("a\u{0662}b"), "a2b");
    }

    #[test]
    fn the_menu_is_drawn_from_the_table_it_is_parsed_from() {
        let lines = menu_lines();
        for (choice, key, label) in MENU {
            assert!(lines.contains(&format!("  {key}  {label}")), "{label} is drawn");
            assert_eq!(parse_choice(key), Typed::Choice(*choice));
        }
        let keys: Vec<&str> = MENU.iter().map(|(_, key, _)| *key).collect();
        assert_eq!(keys, ["1", "2", "3", "4", "5", "6", "0"], "numbered in order, Exit last");
        let text = lines.join("\n");
        for wanted in [
            "Install or repair",
            "Uninstall",
            "Check this folder's version against the installed one",
            "Start Transport Fever 3 through Steam",
            "Open the log folder",
            "Copy a diagnostics text to the clipboard",
            "Exit",
        ] {
            assert!(text.contains(wanted), "{wanted}");
        }
    }
}
