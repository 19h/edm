//! Completion: what a commander is likely to be typing \[C53\].
//!
//! Pure. The sources — the journal's history, the atlas's nearby page,
//! Ardent's station prefix search, the commodity catalogue — are gathered
//! elsewhere and handed in as candidates; this module only ranks them.

/// Where a candidate came from, which the popup shows beside it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    System,
    Station,
    Commodity,
    Category,
}

impl Kind {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Station => "station",
            Self::Commodity => "commodity",
            Self::Category => "category",
        }
    }
}

/// One thing the field could become.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Candidate {
    /// What the popup shows.
    pub label: String,
    /// What the field is set to on acceptance.
    pub insert: String,
    pub kind: Kind,
    /// A distance, a system, a category — whatever helps tell two apart.
    pub hint: String,
    /// Higher is more recent; breaks ties among equal matches.
    pub recency: u32,
}

/// How well `candidate` matches `query`; `None` when it does not.
fn score(query: &str, candidate: &str) -> Option<u8> {
    let query = query.to_lowercase();
    let candidate = candidate.to_lowercase();
    if query.is_empty() {
        return Some(3);
    }
    if candidate.starts_with(&query) {
        return Some(0);
    }
    if candidate
        .split(|c: char| !c.is_alphanumeric())
        .any(|word| word.starts_with(&query))
    {
        return Some(1);
    }
    candidate.contains(&query).then_some(2)
}

/// The candidates that match `query`, best first, de-duplicated by what they
/// would insert. Exact prefix beats a word prefix beats a substring; among
/// equals, the more recent, then the shorter, then the alphabetical.
pub(crate) fn rank(query: &str, candidates: &[Candidate], limit: usize) -> Vec<Candidate> {
    let query = edm_core::js::text::js_trim(query);
    let mut scored: Vec<(u8, &Candidate)> = candidates
        .iter()
        .filter_map(|candidate| score(query, &candidate.label).map(|s| (s, candidate)))
        .collect();
    scored.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| b.1.recency.cmp(&a.1.recency))
            .then_with(|| a.1.label.len().cmp(&b.1.label.len()))
            .then_with(|| a.1.label.to_lowercase().cmp(&b.1.label.to_lowercase()))
    });
    let mut seen: Vec<String> = Vec::new();
    let mut out = Vec::new();
    for (_, candidate) in scored {
        let key = candidate.insert.to_lowercase();
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        out.push(candidate.clone());
        if out.len() >= limit {
            break;
        }
    }
    out
}

/// The token completion applies to in a comma-separated field, and where it
/// starts.
pub(crate) fn last_token(text: &str) -> (usize, &str) {
    match text.rfind(',') {
        Some(comma) => {
            let start = comma + 1;
            let token = &text[start..];
            let lead = token.find(|c: char| !c.is_whitespace()).unwrap_or(token.len());
            (start + lead, &token[lead..])
        }
        None => (0, text),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(label: &str, kind: Kind, recency: u32) -> Candidate {
        Candidate {
            label: label.to_owned(),
            insert: label.to_owned(),
            kind,
            hint: String::new(),
            recency,
        }
    }

    #[test]
    fn a_prefix_beats_a_word_prefix_beats_a_substring_and_recency_breaks_ties() {
        let all = vec![
            c("Alpha Centauri", Kind::System, 0),
            c("Sol", Kind::System, 5),
            c("Solati", Kind::System, 1),
            c("LHS 3447", Kind::System, 9),
            c("Wolf 359", Kind::System, 2),
            c("Ross 154", Kind::System, 0),
        ];
        let got: Vec<String> = rank("sol", &all, 8).into_iter().map(|c| c.label).collect();
        assert_eq!(got, ["Sol", "Solati"]);
        let got: Vec<String> = rank("3", &all, 8).into_iter().map(|c| c.label).collect();
        assert_eq!(got, ["LHS 3447", "Wolf 359"], "word prefix before substring");
        let got: Vec<String> = rank("", &all, 3).into_iter().map(|c| c.label).collect();
        assert_eq!(got, ["LHS 3447", "Sol", "Wolf 359"], "recency, then the limit");
    }

    #[test]
    fn duplicates_collapse_on_what_they_insert() {
        let all = vec![c("Sol", Kind::System, 1), c("sol", Kind::System, 0)];
        assert_eq!(rank("s", &all, 8).len(), 1);
    }

    #[test]
    fn the_last_token_is_what_a_list_field_completes() {
        assert_eq!(last_token("gold, sil"), (6, "sil"));
        assert_eq!(last_token("gold"), (0, "gold"));
        assert_eq!(last_token("gold,"), (5, ""));
    }
}
