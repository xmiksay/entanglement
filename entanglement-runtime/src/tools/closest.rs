//! The "did you mean" hint for an unknown tool name (#437) and an unknown
//! schema property (`arg_validate`).

/// Levenshtein edit distance between two strings.
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut curr = vec![0; b.len() + 1];
    for i in 1..=a.len() {
        curr[0] = i;
        for (j, &bc) in b.iter().enumerate() {
            let cost = usize::from(a[i - 1] != bc);
            curr[j + 1] = (prev[j + 1] + 1).min(curr[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b.len()]
}

/// The closest registered name to `name` by edit distance, capped so a wildly
/// different name (or an empty/tiny registry) doesn't surface a useless hint.
/// `pub(crate)`: also reused by `arg_validate` for its "did you mean" hint
/// over a schema's property names.
pub(crate) fn closest_name<'a>(name: &str, candidates: &[&'a str]) -> Option<&'a str> {
    let max_distance = (name.chars().count() / 2).max(2);
    candidates
        .iter()
        .map(|&c| (c, levenshtein(name, c)))
        .filter(|(_, d)| *d <= max_distance)
        .min_by_key(|(_, d)| *d)
        .map(|(c, _)| c)
}
