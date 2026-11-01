//! The in-game "What's new": the mod's window shows the notes of the version it runs once after an update (owner, 2026-10-30).
//! They come from that version's section of `docs/CHANGELOG.md`, written into the package as `content/feverlink3_whatsnew.lua`,
//! so the changelog stays the one source and nothing is downloaded in the game.
//!
//! A section may start with `### In short`: a few short lines written for the window, shown alone. Without it the window shows
//! the first sentence of every entry of the section's lists (Added, Changed, Fixed, Known issues), each under its heading.

/// The public changelog page, named in the window for the full text.
pub const CHANGELOG_URL: &str = "https://azalshehri7.github.io/FeverLink3/CHANGELOG.html";

/// The mod file the window reads (`ug_require("feverlink3::/feverlink3_whatsnew.lua")`).
pub const FILE: &str = "content/feverlink3_whatsnew.lua";

/// A window line: an entry longer than this is cut at a word.
const MAX_CHARS: usize = 180;

/// The lists of `version`'s section: (heading, entries). `### In short` alone when the section has it.
pub fn notes(changelog: &str, version: &str) -> Result<Vec<(String, Vec<String>)>, String> {
    let mut lines = changelog.lines();
    let found = lines
        .by_ref()
        .any(|l| l.strip_prefix("## ").is_some_and(|h| h == version || h.starts_with(&format!("{version} "))));
    if !found {
        return Err(format!("docs/CHANGELOG.md has no section \"## {version}\": the in-game notes come from it"));
    }
    let mut groups: Vec<(String, Vec<String>)> = Vec::new();
    for line in lines {
        if line.starts_with("## ") {
            break;
        }
        if let Some(h) = line.strip_prefix("### ") {
            groups.push((h.trim().to_owned(), Vec::new()));
        } else if let (Some(item), Some(g)) = (line.strip_prefix("- "), groups.last_mut()) {
            g.1.push(item.trim().to_owned());
        }
    }
    groups.retain(|g| !g.1.is_empty());
    if let Some(short) = groups.iter().find(|g| g.0.eq_ignore_ascii_case("In short")) {
        return Ok(vec![(String::new(), short.1.iter().map(|s| plain(s)).collect())]);
    }
    if groups.is_empty() {
        return Err(format!("the section \"## {version}\" of docs/CHANGELOG.md has no entries"));
    }
    Ok(groups.into_iter().map(|(h, items)| (h, items.iter().map(|s| first_sentence(&plain(s))).collect())).collect())
}

/// Markdown marks a window cannot draw: `**bold**`, `` `code` ``, `[text](link)` become their text.
fn plain(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('[') {
        let after = &rest[i + 1..];
        match (after.find("]("), after.find(']')) {
            (Some(j), Some(k)) if j == k => match after[j + 2..].find(')') {
                Some(e) => {
                    out.push_str(&rest[..i]);
                    out.push_str(&after[..j]);
                    rest = &after[j + 2 + e + 1..];
                }
                None => break,
            },
            _ => {
                out.push_str(&rest[..=i]);
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out.replace("**", "").replace('`', "")
}

/// The entry up to its first sentence end (". " or a final "."), at most MAX_CHARS, cut at a word with "...".
fn first_sentence(s: &str) -> String {
    let end = s.find(". ").map(|i| i + 1).unwrap_or(s.len());
    let one = &s[..end];
    if one.chars().count() <= MAX_CHARS {
        return one.to_owned();
    }
    let cut: String = one.chars().take(MAX_CHARS).collect();
    let at = cut.rfind(' ').unwrap_or(cut.len());
    format!("{}...", cut[..at].trim_end_matches([',', ';', ':']))
}

/// The mod's `_content.json` with the notes file in its `files` list (sorted in, the file's 4-space layout kept).
pub fn listed(content_json: &[u8]) -> Result<Vec<u8>, String> {
    use serde::Serialize;
    let mut v: serde_json::Value = serde_json::from_slice(content_json).map_err(|e| format!("_content.json: {e}"))?;
    let name = FILE.trim_start_matches("content/");
    let list = v.get_mut("files").and_then(|f| f.as_array_mut()).ok_or("_content.json has no files list")?;
    if !list.iter().any(|f| f.as_str() == Some(name)) {
        let at = list.iter().position(|f| f.as_str().is_some_and(|s| s > name)).unwrap_or(list.len());
        list.insert(at, serde_json::Value::from(name));
    }
    let mut out = Vec::new();
    let mut ser =
        serde_json::Serializer::with_formatter(&mut out, serde_json::ser::PrettyFormatter::with_indent(b"    "));
    v.serialize(&mut ser).map_err(|e| e.to_string())?;
    out.push(b'\n');
    Ok(out)
}

/// A Lua string literal (the module's only kind of value besides tables).
fn lua_str(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => {}
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The module the window reads: `{ version, url, groups = { { title, items = { ... } }, ... } }`.
pub fn lua(changelog: &str, version: &str) -> Result<String, String> {
    let groups = notes(changelog, version)?;
    let mut out = String::from(
        "-- Made by feverlink3_release from docs/CHANGELOG.md when the package was built: do not edit.\nreturn {\n",
    );
    out.push_str(&format!("\tversion = {},\n\turl = {},\n\tgroups = {{\n", lua_str(version), lua_str(CHANGELOG_URL)));
    for (title, items) in groups {
        out.push_str(&format!("\t\t{{ title = {}, items = {{\n", lua_str(&title)));
        for item in items {
            out.push_str(&format!("\t\t\t{},\n", lua_str(&item)));
        }
        out.push_str("\t\t} },\n");
    }
    out.push_str("\t},\n}\n");
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOG: &str = "# Changelog\n\n## 1.2.4 (2026-11-02)\n\n### In short\n- Short **one**.\n- Two, see [the page](x.html).\n\n### Fixed\n- Long fixed entry. More detail.\n\n## 1.2.3 (2026-11-01)\n\n### Added\n- The `thing` is new. It does more.\n\n### Fixed\n- A \"quoted\" fix\n\n## 1.2.2\n- old\n";

    #[test]
    fn the_notes_of_a_version_come_from_its_changelog_section() {
        // a section with "In short": those lines alone, marks removed
        assert_eq!(
            notes(LOG, "1.2.4").unwrap(),
            vec![(String::new(), vec!["Short one.".to_owned(), "Two, see the page.".to_owned()])]
        );
        // without it: every list under its heading, the first sentence of each entry
        assert_eq!(
            notes(LOG, "1.2.3").unwrap(),
            vec![
                ("Added".to_owned(), vec!["The thing is new.".to_owned()]),
                ("Fixed".to_owned(), vec!["A \"quoted\" fix".to_owned()])
            ]
        );
        assert!(notes(LOG, "1.2.5").unwrap_err().contains("no section"), "a release without its section is refused");
        assert!(notes(LOG, "1.2").is_err(), "1.2 is not 1.2.3");
        let long = format!("## 9.9.9\n### Fixed\n- {}\n", "word ".repeat(60));
        let cut = &notes(&long, "9.9.9").unwrap()[0].1[0];
        assert!(cut.ends_with("...") && cut.chars().count() <= MAX_CHARS + 3, "{cut}");
        let index = listed(b"{\"archives\": null, \"files\": [\"a.lua\", \"z.lua\"]}").unwrap();
        let v: serde_json::Value = serde_json::from_slice(&index).unwrap();
        assert_eq!(v["files"], serde_json::json!(["a.lua", "feverlink3_whatsnew.lua", "z.lua"]), "listed, sorted in");
        assert_eq!(listed(&index).unwrap(), index, "listed once");
        let module = lua(LOG, "1.2.3").unwrap();
        assert!(module.contains("version = \"1.2.3\"") && module.contains("\"A \\\"quoted\\\" fix\""), "{module}");
    }
}
