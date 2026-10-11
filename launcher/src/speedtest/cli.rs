//! The command line and the choice of role.

use std::path::Path;

use super::PEER_FILE;
use crate::steamnet::parse_steam_id;

/// What the player asked for on the command line.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Cli {
    pub peer: Option<u64>,
    pub listen: bool,
    pub help: bool,
    /// `--rate`: the Steam send rate in bytes per second for this run (sets FEVERLINK3_STEAM_SENDRATE).
    pub rate: Option<u32>,
    /// `--quick`: only the first (8 MB) run, to try several send rates one after another.
    pub quick: bool,
}

/// What this run does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Connects to this Steam ID and runs the whole suite.
    Tester(u64),
    /// Waits for testers.
    Responder,
}

/// Where the role came from, for the first lines of output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    CommandLine,
    PeerFile,
    NoPeerFile,
}

pub const USAGE: &str = "Usage: feverlink3-speedtest.exe [--peer <SteamID64> | --listen]\n\
  (no option)        with a peer.txt next to the program: test against that Steam ID; without it: wait for testers\n\
  --peer <SteamID64> test against that Steam ID (ignores peer.txt)\n\
  --listen           wait for testers (ignores peer.txt)\n\
  --help             show this text";

/// Parses the arguments after the program name.
pub fn parse_args(args: &[String]) -> Result<Cli, String> {
    let mut cli = Cli::default();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let (name, inline) = match arg.split_once('=') {
            Some((n, v)) if n.starts_with("--") => (n, Some(v.to_string())),
            _ => (arg.as_str(), None),
        };
        match name {
            "--help" | "-h" | "/?" => cli.help = true,
            "--listen" => cli.listen = true,
            "--quick" => cli.quick = true,
            "--rate" => {
                let value = inline.or_else(|| it.next().cloned()).ok_or("--rate needs a speed after it, like 4m")?;
                cli.rate = Some(parse_rate(&value)?);
            }
            "--peer" => {
                let value = inline.or_else(|| it.next().cloned()).ok_or("--peer needs a Steam ID after it")?;
                let id = parse_steam_id(&value).ok_or_else(|| {
                    format!(
                        "\"{value}\" is not a Steam ID: expected the 17 digits of a SteamID64, like 76561190000000001"
                    )
                })?;
                cli.peer = Some(id);
            }
            other => return Err(format!("unknown option \"{other}\"")),
        }
    }
    if cli.listen && cli.peer.is_some() {
        return Err("--peer and --listen cannot be used together".into());
    }
    Ok(cli)
}

/// A send rate: bytes per second, or a number with `k` (KiB) or `m` (MiB) after it. 32 KiB to 64 MiB, as the Steam core accepts.
pub fn parse_rate(text: &str) -> Result<u32, String> {
    let t = text.trim().to_ascii_lowercase();
    let (digits, unit) = match t.strip_suffix('m') {
        Some(d) => (d, 1u64 << 20),
        None => match t.strip_suffix('k') {
            Some(d) => (d, 1u64 << 10),
            None => (t.as_str(), 1),
        },
    };
    let n: u64 = digits.trim().parse().map_err(|_| format!("\"{text}\" is not a speed: use bytes per second, or a number with k or m, like 4m"))?;
    let bytes = n.saturating_mul(unit);
    if !(32 * 1024..=64 << 20).contains(&bytes) {
        return Err(format!("\"{text}\" is outside 32k to 64m"));
    }
    Ok(bytes as u32)
}

/// The Steam ID in the text of a peer.txt: the first non-empty line (a byte order mark is ignored).
pub fn parse_peer_text(text: &str) -> Result<u64, String> {
    let line = text.trim_start_matches('\u{feff}').lines().map(str::trim).find(|l| !l.is_empty());
    match line {
        None => {
            Err(format!("{PEER_FILE} is empty. Put the other player's SteamID64 (17 digits) in it, or delete the file"))
        }
        Some(line) => parse_steam_id(line).ok_or_else(|| {
            format!(
                "{PEER_FILE} does not hold a SteamID64: expected the 17 digits like 76561190000000001, found \"{line}\""
            )
        }),
    }
}

/// `peer.txt` in `dir`: `Ok(None)` when there is none.
pub fn read_peer_file(dir: &Path) -> Result<Option<u64>, String> {
    let path = dir.join(PEER_FILE);
    match std::fs::read(&path) {
        Ok(bytes) => parse_peer_text(&String::from_utf8_lossy(&bytes)).map(Some),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{} could not be read: {e}", path.display())),
    }
}

/// The role: the command line first, then `peer.txt`, otherwise responder.
pub fn choose_role(cli: &Cli, dir: &Path) -> Result<(Role, Source), String> {
    if let Some(id) = cli.peer {
        return Ok((Role::Tester(id), Source::CommandLine));
    }
    if cli.listen {
        return Ok((Role::Responder, Source::CommandLine));
    }
    Ok(match read_peer_file(dir)? {
        Some(id) => (Role::Tester(id), Source::PeerFile),
        None => (Role::Responder, Source::NoPeerFile),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const FAKE: u64 = 76561190000000001;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn arguments_are_parsed() {
        assert_eq!(parse_args(&args(&[])).unwrap(), Cli::default());
        assert_eq!(parse_args(&args(&["--peer", "76561190000000001"])).unwrap().peer, Some(FAKE));
        assert_eq!(parse_args(&args(&["--peer=76561190000000001"])).unwrap().peer, Some(FAKE));
        assert!(parse_args(&args(&["--listen"])).unwrap().listen);
        assert!(parse_args(&args(&["--help"])).unwrap().help);
        let c = parse_args(&args(&["--rate", "4m", "--quick"])).unwrap();
        assert_eq!((c.rate, c.quick), (Some(4 << 20), true));
        assert_eq!(parse_args(&args(&["--rate=512k"])).unwrap().rate, Some(512 << 10));
        assert_eq!(parse_rate("2097152").unwrap(), 2 << 20);
        assert!(parse_rate("fast").is_err() && parse_rate("1k").is_err() && parse_rate("100m").is_err());
        assert!(parse_args(&args(&["--rate"])).unwrap_err().contains("needs a speed"));
    }

    #[test]
    fn bad_arguments_are_explained() {
        assert!(parse_args(&args(&["--peer"])).unwrap_err().contains("needs a Steam ID"));
        assert!(parse_args(&args(&["--peer", "abc"])).unwrap_err().contains("not a Steam ID"));
        assert!(parse_args(&args(&["--wat"])).unwrap_err().contains("unknown option"));
        assert!(parse_args(&args(&["--listen", "--peer", "76561190000000001"])).unwrap_err().contains("together"));
    }

    #[test]
    fn no_peer_file_means_responder() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(choose_role(&Cli::default(), dir.path()).unwrap(), (Role::Responder, Source::NoPeerFile));
    }

    #[test]
    fn a_peer_file_means_tester() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(PEER_FILE), "\u{feff}76561190000000001\r\n").unwrap();
        assert_eq!(choose_role(&Cli::default(), dir.path()).unwrap(), (Role::Tester(FAKE), Source::PeerFile));
    }

    #[test]
    fn a_broken_peer_file_is_an_error_not_a_silent_responder() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(PEER_FILE), "my friend\n").unwrap();
        assert!(choose_role(&Cli::default(), dir.path()).unwrap_err().contains("does not hold a SteamID64"));
        std::fs::write(dir.path().join(PEER_FILE), "  \n").unwrap();
        assert!(choose_role(&Cli::default(), dir.path()).unwrap_err().contains("is empty"));
    }

    #[test]
    fn the_command_line_overrides_the_peer_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(PEER_FILE), "76561190000000001").unwrap();
        let listen = Cli { listen: true, ..Cli::default() };
        assert_eq!(choose_role(&listen, dir.path()).unwrap(), (Role::Responder, Source::CommandLine));
        let peer = Cli { peer: Some(76561190000000002), ..Cli::default() };
        assert_eq!(choose_role(&peer, dir.path()).unwrap(), (Role::Tester(76561190000000002), Source::CommandLine));
    }
}
