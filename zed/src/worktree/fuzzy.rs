use super::{char_bag::CharBag, EntryKind, Snapshot};
use gpui::scoped_pool;
use std::{
    cmp::{max, min, Ordering, Reverse},
    collections::BinaryHeap,
    path::Path,
    sync::atomic::{self, AtomicBool},
    sync::Arc,
};

const BASE_DISTANCE_PENALTY: f64 = 0.6;
const ADDITIONAL_DISTANCE_PENALTY: f64 = 0.05;
const MIN_DISTANCE_PENALTY: f64 = 0.2;

#[derive(Clone, Debug)]
pub struct MatchCandidate<'a> {
    pub path: &'a Arc<Path>,
    pub char_bag: CharBag,
}

#[derive(Clone, Debug)]
pub struct PathMatch {
    pub score: f64,
    pub positions: Vec<usize>,
    pub tree_id: usize,
    pub path: Arc<Path>,
}

impl PartialEq for PathMatch {
    fn eq(&self, other: &Self) -> bool {
        self.score.eq(&other.score)
    }
}

impl Eq for PathMatch {}

impl PartialOrd for PathMatch {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        self.score.partial_cmp(&other.score)
    }
}

impl Ord for PathMatch {
    fn cmp(&self, other: &Self) -> Ordering {
        self.partial_cmp(other).unwrap_or(Ordering::Equal)
    }
}

pub fn match_paths<'a, T>(
    snapshots: T,
    query: &str,
    include_root_name: bool,
    include_ignored: bool,
    smart_case: bool,
    max_results: usize,
    cancel_flag: Arc<AtomicBool>,
    pool: scoped_pool::Pool,
) -> Vec<PathMatch>
where
    T: Clone + Send + Iterator<Item = &'a Snapshot> + 'a,
{
    let lowercase_query = query.to_lowercase().chars().collect::<Vec<_>>();
    let query = query.chars().collect::<Vec<_>>();

    let lowercase_query = &lowercase_query;
    let query = &query;
    let query_chars = CharBag::from(&lowercase_query[..]);

    let cpus = num_cpus::get();
    let path_count: usize = if include_ignored {
        snapshots.clone().map(Snapshot::file_count).sum()
    } else {
        snapshots.clone().map(Snapshot::visible_file_count).sum()
    };

    let segment_size = (path_count + cpus - 1) / cpus;
    let mut segment_results = (0..cpus).map(|_| BinaryHeap::new()).collect::<Vec<_>>();

    pool.scoped(|scope| {
        for (segment_idx, results) in segment_results.iter_mut().enumerate() {
            let trees = snapshots.clone();
            let cancel_flag = &cancel_flag;
            scope.execute(move || {
                let segment_start = segment_idx * segment_size;
                let segment_end = segment_start + segment_size;

                let mut min_score = 0.0;
                let mut last_positions = Vec::new();
                last_positions.resize(query.len(), 0);
                let mut match_positions = Vec::new();
                match_positions.resize(query.len(), 0);
                let mut score_matrix = Vec::new();
                let mut best_position_matrix = Vec::new();

                let mut tree_start = 0;
                for snapshot in trees {
                    let tree_end = if include_ignored {
                        tree_start + snapshot.file_count()
                    } else {
                        tree_start + snapshot.visible_file_count()
                    };
                    if tree_start < segment_end && segment_start < tree_end {
                        let start = max(tree_start, segment_start) - tree_start;
                        let end = min(tree_end, segment_end) - tree_start;
                        let entries = if include_ignored {
                            snapshot.files(start).take(end - start)
                        } else {
                            snapshot.visible_files(start).take(end - start)
                        };
                        let paths = entries.map(|entry| {
                            if let EntryKind::File(char_bag) = entry.kind {
                                MatchCandidate {
                                    path: &entry.path,
                                    char_bag,
                                }
                            } else {
                                unreachable!()
                            }
                        });

                        match_single_tree_paths(
                            snapshot,
                            include_root_name,
                            paths,
                            query,
                            lowercase_query,
                            query_chars,
                            smart_case,
                            results,
                            max_results,
                            &mut min_score,
                            &mut match_positions,
                            &mut last_positions,
                            &mut score_matrix,
                            &mut best_position_matrix,
                            &cancel_flag,
                        );
                    }
                    if tree_end >= segment_end {
                        break;
                    }
                    tree_start = tree_end;
                }
            })
        }
    });

    let mut results = segment_results
        .into_iter()
        .flatten()
        .map(|r| r.0)
        .collect::<Vec<_>>();
    results.sort_unstable_by(|a, b| b.score.partial_cmp(&a.score).unwrap());
    results.truncate(max_results);
    results
}

fn match_single_tree_paths<'a>(
    snapshot: &Snapshot,
    include_root_name: bool,
    path_entries: impl Iterator<Item = MatchCandidate<'a>>,
    query: &[char],
    lowercase_query: &[char],
    query_chars: CharBag,
    smart_case: bool,
    results: &mut BinaryHeap<Reverse<PathMatch>>,
    max_results: usize,
    min_score: &mut f64,
    match_positions: &mut Vec<usize>,
    last_positions: &mut Vec<usize>,
    score_matrix: &mut Vec<Option<f64>>,
    best_position_matrix: &mut Vec<usize>,
    cancel_flag: &AtomicBool,
) {
    let mut path_chars = Vec::new();
    let mut lowercase_path_chars = Vec::new();

    let prefix = if include_root_name {
        snapshot.root_name()
    } else {
        ""
    }
    .chars()
    .collect::<Vec<_>>();
    let lowercase_prefix = prefix
        .iter()
        .map(|c| c.to_ascii_lowercase())
        .collect::<Vec<_>>();

    for path_entry in path_entries {
        if !path_entry.char_bag.is_superset(query_chars) {
            continue;
        }

        if cancel_flag.load(atomic::Ordering::Relaxed) {
            break;
        }

        path_chars.clear();
        lowercase_path_chars.clear();
        for c in path_entry.path.to_string_lossy().chars() {
            path_chars.push(c);
            lowercase_path_chars.push(c.to_ascii_lowercase());
        }

        if !find_last_positions(
            last_positions,
            &lowercase_prefix,
            &lowercase_path_chars,
            &lowercase_query[..],
        ) {
            continue;
        }

        let matrix_len = query.len() * (path_chars.len() + prefix.len());
        score_matrix.clear();
        score_matrix.resize(matrix_len, None);
        best_position_matrix.clear();
        best_position_matrix.resize(matrix_len, 0);

        let score = score_match(
            &query[..],
            &lowercase_query[..],
            &path_chars,
            &lowercase_path_chars,
            &prefix,
            &lowercase_prefix,
            smart_case,
            &last_positions,
            score_matrix,
            best_position_matrix,
            match_positions,
            *min_score,
        );

        if score > 0.0 {
            results.push(Reverse(PathMatch {
                tree_id: snapshot.id,
                path: path_entry.path.clone(),
                score,
                positions: match_positions.clone(),
            }));
            if results.len() == max_results {
                *min_score = results.peek().unwrap().0.score;
            }
        }
    }
}

fn find_last_positions(
    last_positions: &mut Vec<usize>,
    prefix: &[char],
    path: &[char],
    query: &[char],
) -> bool {
    let mut path = path.iter();
    let mut prefix_iter = prefix.iter();
    for (i, char) in query.iter().enumerate().rev() {
        if let Some(j) = path.rposition(|c| c == char) {
            last_positions[i] = j + prefix.len();
        } else if let Some(j) = prefix_iter.rposition(|c| c == char) {
            last_positions[i] = j;
        } else {
            return false;
        }
    }
    true
}

fn score_match(
    query: &[char],
    query_cased: &[char],
    path: &[char],
    path_cased: &[char],
    prefix: &[char],
    lowercase_prefix: &[char],
    smart_case: bool,
    last_positions: &[usize],
    score_matrix: &mut [Option<f64>],
    best_position_matrix: &mut [usize],
    match_positions: &mut [usize],
    min_score: f64,
) -> f64 {
    let score = recursive_score_match(
        query,
        query_cased,
        path,
        path_cased,
        prefix,
        lowercase_prefix,
        smart_case,
        last_positions,
        score_matrix,
        best_position_matrix,
        min_score,
        0,
        0,
        query.len() as f64,
    ) * query.len() as f64;

    if score <= 0.0 {
        return 0.0;
    }

    let path_len = path.len() + prefix.len();
    let mut cur_start = 0;
    for i in 0..query.len() {
        match_positions[i] = best_position_matrix[i * path_len + cur_start];
        cur_start = match_positions[i] + 1;
    }

    score
}

fn recursive_score_match(
    query: &[char],
    query_cased: &[char],
    path: &[char],
    path_cased: &[char],
    prefix: &[char],
    lowercase_prefix: &[char],
    smart_case: bool,
    last_positions: &[usize],
    score_matrix: &mut [Option<f64>],
    best_position_matrix: &mut [usize],
    min_score: f64,
    query_idx: usize,
    path_idx: usize,
    cur_score: f64,
) -> f64 {
    if query_idx == query.len() {
        return 1.0;
    }

    let path_len = prefix.len() + path.len();

    if let Some(memoized) = score_matrix[query_idx * path_len + path_idx] {
        return memoized;
    }

    let mut score = 0.0;
    let mut best_position = 0;

    let query_char = query_cased[query_idx];
    let limit = last_positions[query_idx];

    let mut last_slash = 0;
    for j in path_idx..=limit {
        let path_char = if j < prefix.len() {
            lowercase_prefix[j]
        } else {
            path_cased[j - prefix.len()]
        };
        let is_path_sep = path_char == '/' || path_char == '\\';

        if query_idx == 0 && is_path_sep {
            last_slash = j;
        }

        if query_char == path_char || (is_path_sep && query_char == '_' || query_char == '\\') {
            let curr = if j < prefix.len() {
                prefix[j]
            } else {
                path[j - prefix.len()]
            };

            let mut char_score = 1.0;
            if j > path_idx {
                let last = if j - 1 < prefix.len() {
                    prefix[j - 1]
                } else {
                    path[j - 1 - prefix.len()]
                };

                if last == '/' {
                    char_score = 0.9;
                } else if last == '-' || last == '_' || last == ' ' || last.is_numeric() {
                    char_score = 0.8;
                } else if last.is_lowercase() && curr.is_uppercase() {
                    char_score = 0.8;
                } else if last == '.' {
                    char_score = 0.7;
                } else if query_idx == 0 {
                    char_score = BASE_DISTANCE_PENALTY;
                } else {
                    char_score = MIN_DISTANCE_PENALTY.max(
                        BASE_DISTANCE_PENALTY
                            - (j - path_idx - 1) as f64 * ADDITIONAL_DISTANCE_PENALTY,
                    );
                }
            }

            // Apply a severe penalty if the case doesn't match.
            // This will make the exact matches have higher score than the case-insensitive and the
            // path insensitive matches.
            if (smart_case || curr == '/') && query[query_idx] != curr {
                char_score *= 0.001;
            }

            let mut multiplier = char_score;

            // Scale the score based on how deep within the path we found the match.
            if query_idx == 0 {
                multiplier /= ((prefix.len() + path.len()) - last_slash) as f64;
            }

            let mut next_score = 1.0;
            if min_score > 0.0 {
                next_score = cur_score * multiplier;
                // Scores only decrease. If we can't pass the previous best, bail
                if next_score < min_score {
                    // Ensure that score is non-zero so we use it in the memo table.
                    if score == 0.0 {
                        score = 1e-18;
                    }
                    continue;
                }
            }

            let new_score = recursive_score_match(
                query,
                query_cased,
                path,
                path_cased,
                prefix,
                lowercase_prefix,
                smart_case,
                last_positions,
                score_matrix,
                best_position_matrix,
                min_score,
                query_idx + 1,
                j + 1,
                next_score,
            ) * multiplier;

            if new_score > score {
                score = new_score;
                best_position = j;
                // Optimization: can't score better than 1.
                if new_score == 1.0 {
                    break;
                }
            }
        }
    }

    if best_position != 0 {
        best_position_matrix[query_idx * path_len + path_idx] = best_position;
    }

    score_matrix[query_idx * path_len + path_idx] = Some(score);
    score
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_get_last_positions() {
        let mut last_positions = vec![0; 2];
        let result = find_last_positions(
            &mut last_positions,
            &['a', 'b', 'c'],
            &['b', 'd', 'e', 'f'],
            &['d', 'c'],
        );
        assert_eq!(result, false);

        last_positions.resize(2, 0);
        let result = find_last_positions(
            &mut last_positions,
            &['a', 'b', 'c'],
            &['b', 'd', 'e', 'f'],
            &['c', 'd'],
        );
        assert_eq!(result, true);
        assert_eq!(last_positions, vec![2, 4]);

        last_positions.resize(4, 0);
        let result = find_last_positions(
            &mut last_positions,
            &['z', 'e', 'd', '/'],
            &['z', 'e', 'd', '/', 'f'],
            &['z', '/', 'z', 'f'],
        );
        assert_eq!(result, true);
        assert_eq!(last_positions, vec![0, 3, 4, 8]);
    }

    #[test]
    fn test_match_path_entries() {
        let paths = vec![
            "",
            "a",
            "ab",
            "abC",
            "abcd",
            "alphabravocharlie",
            "AlphaBravoCharlie",
            "thisisatestdir",
            "/////ThisIsATestDir",
            "/this/is/a/test/dir",
            "/test/tiatd",
        ];

        assert_eq!(
            match_query("abc", false, &paths),
            vec![
                ("abC", vec![0, 1, 2]),
                ("abcd", vec![0, 1, 2]),
                ("AlphaBravoCharlie", vec![0, 5, 10]),
                ("alphabravocharlie", vec![4, 5, 10]),
            ]
        );
        assert_eq!(
            match_query("t/i/a/t/d", false, &paths),
            vec![("/this/is/a/test/dir", vec![1, 5, 6, 8, 9, 10, 11, 15, 16]),]
        );

        assert_eq!(
            match_query("tiatd", false, &paths),
            vec![
                ("/test/tiatd", vec![6, 7, 8, 9, 10]),
                ("/this/is/a/test/dir", vec![1, 6, 9, 11, 16]),
                ("/////ThisIsATestDir", vec![5, 9, 11, 12, 16]),
                ("thisisatestdir", vec![0, 2, 6, 7, 11]),
            ]
        );
    }

    fn match_query<'a>(
        query: &str,
        smart_case: bool,
        paths: &Vec<&'a str>,
    ) -> Vec<(&'a str, Vec<usize>)> {
        let lowercase_query = query.to_lowercase().chars().collect::<Vec<_>>();
        let query = query.chars().collect::<Vec<_>>();
        let query_chars = CharBag::from(&lowercase_query[..]);

        let path_arcs = paths
            .iter()
            .map(|path| Arc::from(PathBuf::from(path)))
            .collect::<Vec<_>>();
        let mut path_entries = Vec::new();
        for (i, path) in paths.iter().enumerate() {
            let lowercase_path = path.to_lowercase().chars().collect::<Vec<_>>();
            let char_bag = CharBag::from(lowercase_path.as_slice());
            path_entries.push(MatchCandidate {
                char_bag,
                path: path_arcs.get(i).unwrap(),
            });
        }

        let mut match_positions = Vec::new();
        let mut last_positions = Vec::new();
        match_positions.resize(query.len(), 0);
        last_positions.resize(query.len(), 0);

        let cancel_flag = AtomicBool::new(false);
        let mut results = BinaryHeap::new();
        match_single_tree_paths(
            &Snapshot {
                id: 0,
                scan_id: 0,
                abs_path: PathBuf::new().into(),
                ignores: Default::default(),
                entries: Default::default(),
                root_name: Default::default(),
            },
            false,
            path_entries.into_iter(),
            &query[..],
            &lowercase_query[..],
            query_chars,
            smart_case,
            &mut results,
            100,
            &mut 0.0,
            &mut match_positions,
            &mut last_positions,
            &mut Vec::new(),
            &mut Vec::new(),
            &cancel_flag,
        );

        results
            .into_iter()
            .rev()
            .map(|result| {
                (
                    paths
                        .iter()
                        .copied()
                        .find(|p| result.0.path.as_ref() == Path::new(p))
                        .unwrap(),
                    result.0.positions,
                )
            })
            .collect()
    }
}
