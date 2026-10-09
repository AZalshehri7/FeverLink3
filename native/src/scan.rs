//! Byte-pattern scanning with wildcards, for code sites that must survive small game updates.

/// A pattern such as `"48 8B ?? 05 ?? ?? ?? ?? E8"`: hex bytes and `??` wildcards.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pattern(Vec<Option<u8>>);

impl Pattern {
    pub fn parse(text: &str) -> Option<Pattern> {
        let mut out = Vec::new();
        for tok in text.split_whitespace() {
            if tok == "??" || tok == "?" {
                out.push(None);
            } else if tok.len() == 2 {
                out.push(Some(u8::from_str_radix(tok, 16).ok()?));
            } else {
                return None;
            }
        }
        if out.is_empty() || out.iter().all(Option::is_none) {
            return None;
        }
        Some(Pattern(out))
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn matches_at(&self, hay: &[u8], at: usize) -> bool {
        hay.len() >= at + self.0.len() && self.0.iter().zip(&hay[at..]).all(|(p, b)| p.map_or(true, |x| x == *b))
    }

    /// Every offset where the pattern matches.
    pub fn find_all(&self, hay: &[u8]) -> Vec<usize> {
        if hay.len() < self.0.len() {
            return Vec::new();
        }
        (0..=hay.len() - self.0.len()).filter(|&i| self.matches_at(hay, i)).collect()
    }

    /// The one offset where the pattern matches, or `Err(count)` with how many matches there were (0 or 2+).
    /// Hooks are only installed on unique sites: an ambiguous pattern after a game update must fail loudly.
    pub fn find_unique(&self, hay: &[u8]) -> Result<usize, usize> {
        let all = self.find_all(hay);
        match all.len() {
            1 => Ok(all[0]),
            n => Err(n),
        }
    }

    /// Like `find_unique`, but a `hint` offset where the site was in a previous build is checked first and is
    /// accepted when it still matches and is the only match within `hint ± radius`.
    pub fn find_near(&self, hay: &[u8], hint: usize, radius: usize) -> Result<usize, usize> {
        let lo = hint.saturating_sub(radius);
        let hi = (hint + radius).min(hay.len());
        if lo >= hi {
            return self.find_unique(hay);
        }
        let local: Vec<usize> = self.find_all(&hay[lo..hi]).into_iter().map(|i| i + lo).collect();
        match local.len() {
            1 => Ok(local[0]),
            _ => self.find_unique(hay),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_matches_with_wildcards() {
        let p = Pattern::parse("48 8B ?? 05").unwrap();
        assert_eq!(p.len(), 4);
        let hay = [0x90, 0x48, 0x8B, 0xC1, 0x05, 0x48, 0x8B, 0xFF, 0x05];
        assert_eq!(p.find_all(&hay), vec![1, 5]);
        assert_eq!(p.find_unique(&hay), Err(2));
        assert_eq!(Pattern::parse("8B C1 05").unwrap().find_unique(&hay), Ok(2));
        assert_eq!(Pattern::parse("AA").unwrap().find_unique(&hay), Err(0));
    }

    #[test]
    fn rejects_bad_patterns() {
        assert!(Pattern::parse("").is_none());
        assert!(Pattern::parse("?? ??").is_none());
        assert!(Pattern::parse("4 8B").is_none());
        assert!(Pattern::parse("ZZ").is_none());
    }

    #[test]
    fn hint_picks_the_local_match_and_falls_back_to_unique() {
        let p = Pattern::parse("01 02").unwrap();
        let mut hay = vec![0u8; 1000];
        hay[100] = 1;
        hay[101] = 2;
        hay[800] = 1;
        hay[801] = 2;
        assert_eq!(p.find_near(&hay, 110, 50), Ok(100));
        assert_eq!(p.find_near(&hay, 790, 50), Ok(800));
        assert_eq!(p.find_near(&hay, 500, 50), Err(2)); // nothing near, ambiguous globally
        hay[800] = 0;
        assert_eq!(p.find_near(&hay, 500, 50), Ok(100)); // nothing near, unique globally
    }

    #[test]
    fn short_haystack() {
        let p = Pattern::parse("01 02 03").unwrap();
        assert!(p.find_all(&[1, 2]).is_empty());
        assert!(!p.matches_at(&[1, 2, 3], 1));
    }
}
