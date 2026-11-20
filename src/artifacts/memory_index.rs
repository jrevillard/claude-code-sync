//! Union merge for `MEMORY.md` memory indexes.
//!
//! An index is one line per memory (`- [Title](file.md) — hook`), listing what
//! that machine knows about. Overwriting would let a sparser index orphan
//! another machine's memory files, so both directions union instead: entries
//! are keyed by link target, the incoming side wins per entry, and entries the
//! destination lacks are appended.
//!
//! The destination keeps its own headings, blank lines, prose and entry order,
//! since this rewrites a hand-edited file. Only entry lines cross between
//! machines. Merging only adds or updates a line, so repeating it changes
//! nothing and two machines converge.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// The file name whose contents are union-merged rather than overwritten.
pub const MEMORY_INDEX_FILE: &str = "MEMORY.md";

/// One line of an index: an entry carries the link target it is keyed by.
enum IndexLine {
    Entry { targets: Vec<String>, text: String },
    Other(String),
}

/// True when this path's file name is a memory index.
pub fn is_memory_index(path: &std::path::Path) -> bool {
    path.file_name().map(|name| name == MEMORY_INDEX_FILE) == Some(true)
}

fn link_target(line: &str) -> Option<String> {
    link_targets(line).into_iter().next()
}

/// Every markdown link target on the line, left to right. Compacted
/// indexes group several pointers on one line — a line is not one
/// reference, and taking only the first `](` would key the whole bundle
/// on a single pointer (mismerging compact lines) and declare the rest
/// of the line's files orphaned.
fn link_targets(line: &str) -> Vec<String> {
    let mut targets = Vec::new();
    let mut rest = line;
    while let Some(start) = rest.find("](") {
        let after = &rest[start + 2..];
        match after.find(')') {
            Some(end) => {
                targets.push(after[..end].to_string());
                rest = &after[end + 1..];
            }
            None => break,
        }
    }
    targets
}

fn parse(text: &str) -> Vec<IndexLine> {
    text.lines()
        .map(|line| {
            let targets = link_targets(line);
            if line.starts_with("- ") && !targets.is_empty() {
                IndexLine::Entry {
                    targets,
                    text: line.to_string(),
                }
            } else {
                IndexLine::Other(line.to_string())
            }
        })
        .collect()
}

/// Merge `incoming` into `dest`, returning the merged text and how many entries
/// `incoming` contributed that `dest` did not have.
/// Whether union-merging `incoming` over `dest` keeps every line of
/// `dest`. The merge is grow-only across entries but incoming-wins per
/// target key, so two sides that only ADD entries converge losslessly,
/// while a shared key with different text would clobber the dest side's
/// edited bullet. The pull consults this before routing a diverged
/// index: converge when nothing is lost, hold when something is.
pub fn union_preserves_dest_lines(dest: &[u8], incoming: &[u8]) -> bool {
    let (merged, _) = merge_memory_index(dest, incoming);
    let merged_text = String::from_utf8_lossy(&merged);
    String::from_utf8_lossy(dest)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .all(|line| merged_text.lines().any(|m| m == line))
}

pub fn merge_memory_index(dest: &[u8], incoming: &[u8]) -> (Vec<u8>, usize) {
    let dest_lines = parse(&String::from_utf8_lossy(dest));
    let incoming_lines = parse(&String::from_utf8_lossy(incoming));

    let incoming_entry_count = incoming_lines
        .iter()
        .filter(|line| matches!(line, IndexLine::Entry { .. }))
        .count();

    let dest_targets: BTreeSet<&str> = dest_lines
        .iter()
        .flat_map(|line| match line {
            IndexLine::Entry { targets, .. } => {
                targets.iter().map(|t| t.as_str()).collect::<Vec<_>>()
            }
            IndexLine::Other(_) => Vec::new(),
        })
        .collect();

    if dest_targets.is_empty() && dest.is_empty() {
        return (incoming.to_vec(), incoming_entry_count);
    }

    // Pointer-set semantics: a compact line bundles related pointers, so
    // a line is superseded only by an incoming line covering ALL its
    // pointers (lossless, no duplication). Anything else keeps the dest
    // line — the old first-link-keyed replacement silently dropped the
    // other pointers of a grouped line.
    let mut used_for_replacement = vec![false; incoming_lines.len()];
    let mut merged = String::new();
    for line in &dest_lines {
        let text = match line {
            IndexLine::Entry { targets, text } => {
                let covering = incoming_lines.iter().enumerate().find(|(i, inc)| {
                    !used_for_replacement[*i]
                        && match inc {
                            IndexLine::Entry {
                                targets: inc_targets,
                                ..
                            } => {
                                // incoming COVERS dest: every dest pointer
                                // is present in the incoming line
                                targets.iter().all(|t| inc_targets.contains(t))
                            }
                            IndexLine::Other(_) => false,
                        }
                });
                match covering {
                    Some((i, inc)) => {
                        used_for_replacement[i] = true;
                        match inc {
                            IndexLine::Entry { text, .. } => text.as_str(),
                            IndexLine::Other(text) => text.as_str(),
                        }
                    }
                    None => text.as_str(),
                }
            }
            IndexLine::Other(text) => text.as_str(),
        };
        merged.push_str(text);
        merged.push('\n');
    }

    // Incoming entries not used as replacements are appended when they
    // bring at least one pointer the merged text does not have yet, and
    // are not byte-identical to a line already there. A single-link line
    // whose pointer is covered but whose text differs still arrives —
    // the edit must be visible — which is the appended-line form of the
    // incoming-wins contract.
    let mut added = 0;
    for (i, line) in incoming_lines.iter().enumerate() {
        if used_for_replacement[i] {
            continue;
        }
        let IndexLine::Entry { text, .. } = line else {
            continue;
        };
        // Arrive unless the exact line is already there. A line whose
        // pointers are all covered but whose text differs is an edit —
        // it must be visible (as its own line; the bundled line keeps
        // the other pointers).
        let merged_has_text = merged.lines().any(|l| l == text);
        if merged_has_text {
            continue;
        }
        merged.push_str(text);
        merged.push('\n');
        added += 1;
    }

    (merged.into_bytes(), added)
}

/// A link target eligible for dangling-entry detection, in normalized
/// form. The raw `link_target` string is whatever sits between the first
/// `](` and the next `)` — URLs, `#anchor` suffixes, `./` prefixes,
/// percent-encoded or parenthesized names, subdirectory paths. Only a
/// target that normalizes to a bare file name (no separators, no scheme,
/// no encoding artifacts) can be matched against a directory listing
/// safely; every other shape is exempt from stripping, because a wrong
/// drop is permanent (the repo is the only source pulls union from).
fn strippable_target(target: &str) -> Option<&str> {
    let stripped_prefix = target.strip_prefix("./").unwrap_or(target);
    let base = match stripped_prefix.split_once('#') {
        Some((base, _anchor)) => base,
        None => stripped_prefix,
    };
    if base.is_empty() || base == "." || base == ".." {
        return None;
    }
    if base.chars().any(|c| {
        matches!(
            c,
            '/' | '\\' | ':' | '%' | '(' | ')' | '<' | '>' | '?' | '&' | '='
        )
    }) {
        return None;
    }
    // Only a memory-document file name is this sanitizer's business. A
    // bare non-`.md` target is a section pointer (`(global)` pointing at
    // a CLAUDE.md heading), not a lost sibling file — dropping it would
    // delete real content on every machine.
    if !base.to_ascii_lowercase().ends_with(".md") {
        return None;
    }
    Some(base)
}

/// Remove entry lines whose link target exists in no `known_targets`
/// set (the raw file listings of both sides). Headings, blank lines,
/// prose and bullets without a strippable target pass through. Returns
/// the stripped bytes and how many entries were dropped.
pub fn strip_dangling_entries(bytes: &[u8], known_targets: &BTreeSet<String>) -> (Vec<u8>, usize) {
    let (stripped, dropped) = strip_dangling_with_targets(bytes, known_targets);
    (stripped, dropped.len())
}

fn strip_dangling_with_targets(
    bytes: &[u8],
    known_targets: &BTreeSet<String>,
) -> (Vec<u8>, Vec<String>) {
    let mut kept = String::new();
    let mut dropped = Vec::new();
    for line in String::from_utf8_lossy(bytes).lines() {
        // A grouped line carries several pointers: it is dropped only
        // when EVERY link on it is a strippable, dangling target — one
        // alive pointer pins the whole line (conservative: a wrong drop
        // is permanent, a kept line costs one stale pointer).
        let dangling: Vec<String> = link_targets(line)
            .iter()
            .filter_map(|target| {
                strippable_target(target)
                    .map(|base| base.to_string())
                    .filter(|base| !known_targets.contains(base))
            })
            .collect();
        // An exempt link (URL, anchor-only, subdirectory, section
        // pointer) is a pointer we do not judge: its presence keeps the
        // line, whatever the other links look like.
        if line.starts_with("- ")
            && !link_targets(line).is_empty()
            && dangling.len() == link_targets(line).len()
        {
            dropped.extend(dangling);
        } else {
            kept.push_str(line);
            kept.push('\n');
        }
    }
    (kept.into_bytes(), dropped)
}

/// Union-merge, then drop entries whose target exists on no side. The
/// single entry point the engine uses for memory indexes, so no call
/// site can merge without stripping. The `added` count is corrected for
/// entries the strip removed (the merged-entries counter must describe
/// the bytes that actually landed).
pub fn merge_memory_index_checked(
    dest: &[u8],
    incoming: &[u8],
    known_targets: &BTreeSet<String>,
) -> (Vec<u8>, usize, usize) {
    let dest_targets: BTreeSet<String> = parse(&String::from_utf8_lossy(dest))
        .iter()
        .flat_map(|line| match line {
            IndexLine::Entry { targets, .. } => targets.clone(),
            IndexLine::Other(_) => Vec::new(),
        })
        .collect();

    let (merged, added) = merge_memory_index(dest, incoming);
    let (stripped_bytes, dropped) = strip_dangling_with_targets(&merged, known_targets);
    let newly_added_dropped = dropped
        .iter()
        .filter(|target| !dest_targets.contains(*target))
        .count();

    (
        stripped_bytes,
        added.saturating_sub(newly_added_dropped),
        dropped.len(),
    )
}

/// Every link target any index line references — bare file names,
/// anchor suffixes normalized to the base name, subdirectory paths kept
/// whole. Both `MEMORY.md` and aggregate indexes (`all-memories-index.md`)
/// feed the same set: any `.md` in a memory directory can hold references.
pub fn referenced_targets(content: &str) -> BTreeSet<String> {
    content
        .lines()
        .flat_map(|line| link_targets(line))
        .map(|target| {
            let unwrapped = target.trim_start_matches('<').trim_end_matches('>');
            let stripped_prefix = unwrapped.strip_prefix("./").unwrap_or(unwrapped);
            let base = stripped_prefix
                .split_once('#')
                .map(|(base, _)| base)
                .unwrap_or(stripped_prefix);
            base.to_string()
        })
        .collect()
}

/// Whether a path's file name matches any referenced target. A target
/// with a directory part (`notes/sub.md`) also matches on its base name,
/// mirroring how the sync engine lists memory directories.
fn is_referenced(file_name: &str, targets: &BTreeSet<String>) -> bool {
    targets.iter().any(|t| {
        t.eq_ignore_ascii_case(file_name)
            || t.split('/')
                .next_back()
                .is_some_and(|base| base.eq_ignore_ascii_case(file_name))
    })
}

/// `*.md` files under any `projects/*/memory/` directory that no `.md`
/// in the same directory links to. An orphaned memory file is invisible
/// by construction — indexes are all that sync summaries and other
/// machines ever read — so status surfaces them before they rot.
pub fn orphaned_memory_files(claude_dir: &Path) -> Vec<PathBuf> {
    use std::fs;

    let projects = claude_dir.join("projects");
    let Ok(entries) = fs::read_dir(&projects) else {
        return Vec::new();
    };
    let mut orphans = Vec::new();
    // Worktree homes often symlink their whole memory dir to a real
    // home's; walking the link repeats the same physical files under
    // another path and multiplies the report. One scan per real dir.
    let mut scanned: std::collections::BTreeSet<PathBuf> = std::collections::BTreeSet::new();
    for project in entries.flatten() {
        let memory = project.path().join("memory");
        let Ok(real) = fs::canonicalize(&memory) else {
            continue;
        };
        if !scanned.insert(real.clone()) {
            continue;
        }
        if !memory.is_dir() {
            continue;
        }
        let Ok(files) = fs::read_dir(&memory) else {
            continue;
        };
        let md_files: Vec<_> = files
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("md"))
            .collect();
        let mut targets = BTreeSet::new();
        // Holders: MEMORY.md always, and any aggregate index — a file
        // that itself references others is a holder, never an orphan.
        let mut holders = BTreeSet::new();
        for file in &md_files {
            if is_memory_index(file) {
                holders.insert(file.clone());
            }
            if let Ok(text) = fs::read_to_string(file) {
                let refs = referenced_targets(&text);
                if !refs.is_empty() {
                    holders.insert(file.clone());
                }
                targets.extend(refs);
            }
        }
        for file in md_files {
            if holders.contains(&file) {
                continue;
            }
            let Some(name) = file.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if !is_referenced(name, &targets) {
                orphans.push(file);
            }
        }
    }
    orphans.sort();
    orphans
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn recognizes_the_index_by_file_name() {
        assert!(is_memory_index(Path::new("projects/p/memory/MEMORY.md")));
        assert!(!is_memory_index(Path::new("projects/p/memory/fact.md")));
    }

    #[test]
    fn merging_the_same_entries_again_changes_nothing() {
        let one = b"# Memory index\n\n- [a](a.md) hook A\n- [b](b.md) hook B\n";
        let two = b"# Memory index\n\n- [b](b.md) hook B\n- [a](a.md) hook A\n";

        let (merged, added) = merge_memory_index(one, two);

        assert_eq!(merged, one.to_vec(), "same entries, so nothing to add");
        assert_eq!(added, 0);
        assert_eq!(merge_memory_index(&merged, one).0, merged);
        assert_eq!(merge_memory_index(&merged, two).0, merged);
    }

    #[test]
    fn both_machines_end_up_with_every_entry() {
        let mut machine_a = b"# Index\n\n- [a](a.md) from A\n".to_vec();
        let mut repo = machine_a.clone();
        let machine_b = b"# Index\n\n- [b](b.md) from B\n".to_vec();

        // B pushes, then pulls; A pulls afterwards.
        repo = merge_memory_index(&repo, &machine_b).0;
        let machine_b = merge_memory_index(&machine_b, &repo).0;
        machine_a = merge_memory_index(&machine_a, &repo).0;

        for index in [&machine_a, &machine_b, &repo] {
            let text = String::from_utf8(index.clone()).unwrap();
            assert!(text.contains("[a](a.md)"), "{text}");
            assert!(text.contains("[b](b.md)"), "{text}");
        }
        // And a second round settles: nothing further is written anywhere.
        assert_eq!(merge_memory_index(&repo, &machine_a).0, repo);
        assert_eq!(merge_memory_index(&machine_a, &repo).0, machine_a);
    }

    #[test]
    fn headings_blank_lines_and_prose_are_kept_where_they_are() {
        let dest = b"# Memory index\n\n## Work\n- [a](a.md) hook A\n\n## Personal\n- [b](b.md) hook B\n\nSee also the README.\n";
        let incoming = b"- [a](a.md) hook A\n- [c](c.md) hook C\n";

        let (merged, added) = merge_memory_index(dest, incoming);

        assert_eq!(
            String::from_utf8(merged).unwrap(),
            "# Memory index\n\n## Work\n- [a](a.md) hook A\n\n## Personal\n- [b](b.md) hook B\n\nSee also the README.\n- [c](c.md) hook C\n"
        );
        assert_eq!(added, 1);
    }

    #[test]
    fn an_empty_index_takes_the_other_side_verbatim() {
        let incoming = b"# Index\n\n- [a](a.md) hook A\n";
        let (merged, added) = merge_memory_index(b"", incoming);
        assert_eq!(merged, incoming.to_vec());
        assert_eq!(added, 1);
    }

    #[test]
    fn a_sparser_index_cannot_drop_entries() {
        let rich = b"# Memory index\n\n- [a](a.md) hook A\n- [z](z.md) hook Z\n";
        let sparse = b"- [a](a.md) hook A\n";
        let (merged, added) = merge_memory_index(rich, sparse);
        let text = String::from_utf8(merged).unwrap();
        assert!(text.contains("[z](z.md)"));
        assert!(text.contains("# Memory index"));
        assert_eq!(added, 0);
    }

    #[test]
    fn incoming_wins_per_entry_and_new_entries_are_counted() {
        let dest = b"- [a](a.md) OLD\n";
        let incoming = b"- [a](a.md) NEW\n- [b](b.md) added\n";
        let (merged, added) = merge_memory_index(dest, incoming);
        let text = String::from_utf8(merged).unwrap();
        assert!(text.contains("NEW") && !text.contains("OLD"));
        assert_eq!(added, 1);
    }

    fn targets(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    #[test]
    fn a_dangling_entry_is_dropped_when_no_side_has_the_file() {
        let index = b"# Index\n\n- [a](a.md) hook A\n- [gone](gone.md) hook gone\n";
        let (stripped, dropped) = strip_dangling_entries(index, &targets(&["a.md"]));
        let text = String::from_utf8(stripped).unwrap();
        assert!(text.contains("[a](a.md)"), "live entry survives: {text}");
        assert!(!text.contains("gone.md"), "dangling entry dropped: {text}");
        assert!(text.contains("# Index"), "headings survive");
        assert_eq!(dropped, 1);
    }

    #[test]
    fn an_entry_present_on_either_side_is_kept() {
        let index = b"- [a](a.md) A\n- [b](b.md) B\n";
        // a.md exists on the "local" side only, b.md on the "repo" side
        // only — the engine unions both listings before calling.
        let (stripped, dropped) = strip_dangling_entries(index, &targets(&["a.md", "b.md"]));
        let text = String::from_utf8(stripped).unwrap();
        assert!(text.contains("[a](a.md)") && text.contains("[b](b.md)"));
        assert_eq!(dropped, 0);
    }

    #[test]
    fn stripping_is_idempotent() {
        let index = b"# Index\n\n- [a](a.md) A\n- [gone](gone.md) G\n";
        let (once, _) = strip_dangling_entries(index, &targets(&["a.md"]));
        let (twice, dropped) = strip_dangling_entries(&once, &targets(&["a.md"]));
        assert_eq!(once, twice);
        assert_eq!(dropped, 0);
    }

    #[test]
    fn non_filename_targets_are_exempt_from_stripping() {
        // A wrong drop is permanent (the repo is the only source pulls
        // union from), so anything that is not a bare file name is kept.
        let index = concat!(
            "- [spec](https://example.com/s) spec\n",
            "- [wiki](<my file.md>) spaced\n",
            "- [enc](my%20file.md) encoded\n",
            "- [sub](notes/sub.md) nested\n",
            "- [up](../shared.md) outside\n",
            "- [parens](my(file).md) parens\n",
            "- [first](https://x) then [a](a.md) dual-link\n",
            "- [empty]() empty\n",
            "- [pointer](global) section pointer, not a file\n",
            "- [txt](notes.txt) not a memory document\n",
            "- no link bullet\n",
        )
        .as_bytes();
        let (stripped, dropped) = strip_dangling_entries(index, &targets(&[]));
        let text = String::from_utf8(stripped).unwrap();
        for needle in [
            "https://example.com/s",
            "<my file.md>",
            "my%20file.md",
            "notes/sub.md",
            "../shared.md",
            "my(file).md",
            "empty]()",
            "no link bullet",
        ] {
            assert!(text.contains(needle), "exempt shape kept: {needle}");
        }
        assert_eq!(dropped, 0, "nothing strippable in this index");
    }

    #[test]
    fn an_anchor_suffix_normalizes_to_the_base_file_name() {
        let index = b"- [a](a.md#section) anchored\n- [dead](dead.md#x) anchored dead\n";
        let (stripped, dropped) = strip_dangling_entries(index, &targets(&["a.md"]));
        let text = String::from_utf8(stripped).unwrap();
        assert!(text.contains("a.md#section"), "live anchored entry kept");
        assert!(!text.contains("dead.md"), "dead anchored entry dropped");
        assert_eq!(dropped, 1);
    }

    #[test]
    fn checked_reports_added_entries_the_strip_actually_kept() {
        let dest = b"- [a](a.md) A\n";
        let incoming = b"- [a](a.md) A\n- [ghost](ghost.md) G\n- [live](live.md) L\n";
        let (merged, added, stripped) =
            merge_memory_index_checked(dest, incoming, &targets(&["a.md", "live.md"]));
        let text = String::from_utf8(merged).unwrap();
        assert!(text.contains("[live](live.md)"), "live new entry lands");
        assert!(!text.contains("ghost.md"), "dangling new entry dropped");
        assert_eq!(added, 1, "the dropped ghost must not count as merged");
        assert_eq!(stripped, 1);
    }

    #[test]
    fn checked_strips_a_dangling_entry_the_dest_already_had() {
        let dest = b"# I\n\n- [gone](gone.md) G\n";
        let incoming = b"";
        let (merged, added, stripped) = merge_memory_index_checked(dest, incoming, &targets(&[]));
        let text = String::from_utf8(merged).unwrap();
        assert!(!text.contains("gone.md"));
        assert_eq!(added, 0);
        assert_eq!(stripped, 1);
    }
}

#[cfg(test)]
mod convergence_tests {
    use super::*;

    #[test]
    fn a_union_that_only_adds_preserves_the_dest_lines() {
        let dest = b"# Index\n\n- [a](a.md) A\n";
        let incoming = b"# Index\n\n- [a](a.md) A\n- [b](b.md) B\n";
        assert!(union_preserves_dest_lines(dest, incoming));
    }

    #[test]
    fn a_union_that_clobbers_an_edited_entry_reports_it() {
        // Same target key, different text: the merge is incoming-wins per
        // key, so the dest side's edited bullet would be lost.
        let dest = b"# Index\n\n- [a](a.md) edited here\n";
        let incoming = b"# Index\n\n- [a](a.md) edited there\n";
        assert!(!union_preserves_dest_lines(dest, incoming));
    }

    #[test]
    fn identical_sides_preserve_each_other() {
        let dest = b"# Index\n\n- [a](a.md) A\n";
        assert!(union_preserves_dest_lines(dest, dest));
    }

    #[test]
    fn an_empty_dest_has_nothing_to_lose() {
        let incoming = b"# Index\n\n- [a](a.md) A\n";
        assert!(union_preserves_dest_lines(b"", incoming));
    }
}

#[cfg(test)]
mod orphan_tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write(path: &std::path::Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    #[test]
    fn referenced_targets_extracts_bare_and_anchored_names() {
        let text = "# Index\n\n- [a](a.md) A\n- [b](b.md#sec) B\n- [p](notes/sub.md) P\n";
        let targets = referenced_targets(text);
        assert_eq!(
            targets,
            BTreeSet::from([
                "a.md".to_string(),
                "b.md".to_string(),
                "notes/sub.md".to_string(),
            ]),
            "anchors normalize to the base name; directory targets stay whole"
        );
    }

    #[test]
    fn a_memory_file_no_index_references_is_orphaned() {
        let dir = TempDir::new().unwrap();
        let memory = dir.path().join("projects/-home-x/memory");
        write(&memory.join("MEMORY.md"), "# Index\n\n- [a](a.md) A\n");
        write(&memory.join("a.md"), "a\n");
        write(&memory.join("ghost.md"), "nobody points here\n");
        let orphans = orphaned_memory_files(dir.path());
        assert_eq!(orphans.len(), 1);
        assert!(orphans[0].ends_with("ghost.md"));
    }

    #[test]
    fn an_aggregate_index_counts_as_a_reference_holder() {
        let dir = TempDir::new().unwrap();
        let memory = dir.path().join("projects/-home-x/memory");
        write(&memory.join("MEMORY.md"), "# Index\n\n- [a](a.md) A\n");
        write(&memory.join("a.md"), "a\n");
        write(
            &memory.join("all-memories-index.md"),
            "# All\n\n- [g](ghost.md) G\n",
        );
        write(&memory.join("ghost.md"), "g\n");
        assert!(
            orphaned_memory_files(dir.path()).is_empty(),
            "any .md in the dir can hold references, not just MEMORY.md"
        );
    }

    #[test]
    fn a_dir_without_memory_dir_reports_nothing() {
        let dir = TempDir::new().unwrap();
        assert!(orphaned_memory_files(dir.path()).is_empty());
    }
}

#[cfg(test)]
mod orphan_review_tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write(path: &std::path::Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    #[test]
    fn a_prose_link_protects_its_target_from_the_orphan_scan() {
        // The requirement is "never misreport a referenced file": a link
        // in prose or a numbered list is still a reference, even though
        // the union merge's bullet-only parser ignores it there.
        let dir = TempDir::new().unwrap();
        let memory = dir.path().join("projects/-home-x/memory");
        write(&memory.join("MEMORY.md"), "# Index\n\n- [a](a.md) A\n");
        write(&memory.join("a.md"), "a\n");
        write(
            &memory.join("note.md"),
            "See [detail](detail.md) for the audit trail.\n",
        );
        write(&memory.join("detail.md"), "detail\n");
        let orphans = orphaned_memory_files(dir.path());
        assert!(
            orphans.is_empty(),
            "a prose link counts as a reference: {orphans:?}"
        );
    }

    #[test]
    fn a_numbered_list_link_counts_as_a_reference() {
        let dir = TempDir::new().unwrap();
        let memory = dir.path().join("projects/-home-x/memory");
        write(&memory.join("MEMORY.md"), "# Index\n\n- [a](a.md) A\n");
        write(&memory.join("a.md"), "a\n");
        write(&memory.join("list.md"), "1. [detail](detail.md) first\n");
        write(&memory.join("detail.md"), "detail\n");
        assert!(orphaned_memory_files(dir.path()).is_empty());
    }

    #[test]
    fn reference_matching_is_case_insensitive() {
        let dir = TempDir::new().unwrap();
        let memory = dir.path().join("projects/-home-x/memory");
        write(&memory.join("MEMORY.md"), "# Index\n\n- [Big](Big.md) B\n");
        write(&memory.join("big.md"), "b\n");
        assert!(orphaned_memory_files(dir.path()).is_empty());
    }
}

#[cfg(test)]
mod orphan_compact_format_tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write(path: &std::path::Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    #[test]
    fn every_link_on_a_compact_line_counts_as_a_reference() {
        // Compacted indexes group 3-10 pointers on one line. A line is
        // not one reference: every link on it protects its target.
        let dir = TempDir::new().unwrap();
        let memory = dir.path().join("projects/-home-x/memory");
        write(
            &memory.join("MEMORY.md"),
            "# Index\n\n- [bundle](a.md) — also [b.md](b.md); plus [c](c.md)\n",
        );
        write(&memory.join("a.md"), "a\n");
        write(&memory.join("b.md"), "b\n");
        write(&memory.join("c.md"), "c\n");
        assert!(
            orphaned_memory_files(dir.path()).is_empty(),
            "all links on a grouped line protect their files"
        );
    }

    #[test]
    fn a_symlinked_memory_dir_is_scanned_once() {
        // Worktree homes symlink their memory dir to a real home's —
        // scanning the link repeats the same physical files under
        // another path and multiplies the orphan report.
        let dir = TempDir::new().unwrap();
        let real = dir.path().join("projects/-home-real/memory");
        write(&real.join("MEMORY.md"), "# Index\n\n- [a](a.md) A\n");
        write(&real.join("a.md"), "a\n");
        write(&real.join("ghost.md"), "orphan\n");
        let link_home = dir.path().join("projects/-home-link");
        fs::create_dir_all(&link_home).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, link_home.join("memory")).unwrap();

        let orphans = orphaned_memory_files(dir.path());
        let canonical: std::collections::BTreeSet<_> = orphans
            .iter()
            .map(|p| fs::canonicalize(p).unwrap())
            .collect();
        assert_eq!(
            orphans.len(),
            canonical.len(),
            "one physical file, one report line: {orphans:?}"
        );
    }

    #[test]
    fn every_target_of_a_compact_line_is_collected() {
        let text = "- [x](a.md) a; [y](b.md) b; [z](c.md#sec) c\n";
        let targets = referenced_targets(text);
        assert_eq!(
            targets,
            BTreeSet::from(["a.md".to_string(), "b.md".to_string(), "c.md".to_string(),])
        );
    }
}

#[cfg(test)]
mod grouped_lines_tests {
    use super::*;

    #[test]
    fn a_grouped_line_survives_an_incoming_edit_at_one_of_its_pointers() {
        // The compact format bundles related pointers on one line. An
        // incoming single-link line editing ONE of those pointers must
        // not erase the other pointers of the bundle: the grouped line
        // survives, the incoming line is appended, every pointer from
        // both sides is present.
        let dest = b"- [bundle](a.md) - also [b](b.md); plus [c](c.md)\n";
        let incoming = b"- [a](a.md) edited on the other machine\n";
        let (merged, added) = merge_memory_index(dest, incoming);
        let text = String::from_utf8(merged).unwrap();
        for needle in ["a.md", "b.md", "c.md"] {
            assert!(
                text.contains(needle),
                "pointer survives: {needle} in {text}"
            );
        }
        assert!(
            text.contains("edited on the other machine"),
            "the edit arrives: {text}"
        );
        assert_eq!(added, 1, "the incoming line is appended, not dropped");
    }

    #[test]
    fn a_covering_line_supersedes_a_grouped_line_without_duplicates() {
        // The incoming line carries every pointer of the dest line plus
        // new ones: replacing the dest line is lossless (all its pointers
        // are covered) and avoids duplicating them.
        let dest = b"- [bundle](a.md) - also [b](b.md)\n";
        let incoming = b"- [bundle+](a.md) - also [b](b.md); plus [c](c.md)\n";
        let (merged, _) = merge_memory_index(dest, incoming);
        let text = String::from_utf8(merged).unwrap();
        for needle in ["a.md", "b.md", "c.md"] {
            assert!(
                text.contains(needle),
                "pointer survives: {needle} in {text}"
            );
        }
        assert!(
            text.matches("a.md").count() == 1,
            "no duplicated pointer: {text}"
        );
    }

    #[test]
    fn grouped_vs_grouped_partial_overlap_loses_no_pointer() {
        // Two machines grouped DIFFERENT subsets under the same first
        // pointer: neither line supersedes the other, both are kept, and
        // the union of pointers is complete — the old first-link-keyed
        // merge silently dropped the losing line's pointers here.
        let dest = b"- [shared](a.md) - mine [b](b.md)\n";
        let incoming = b"- [shared](a.md) - theirs [d](d.md)\n";
        let (merged, _) = merge_memory_index(dest, incoming);
        let text = String::from_utf8(merged).unwrap();
        for needle in ["a.md", "b.md", "d.md"] {
            assert!(
                text.contains(needle),
                "pointer survives: {needle} in {text}"
            );
        }
    }

    #[test]
    fn single_link_lines_keep_the_incoming_wins_contract() {
        // The pre-existing contract is untouched: one link per line, the
        // incoming side's text wins at a shared key.
        let dest = b"- [a](a.md) dest text\n";
        let incoming = b"- [a](a.md) incoming text\n- [n](n.md) new\n";
        let (merged, added) = merge_memory_index(dest, incoming);
        let text = String::from_utf8(merged).unwrap();
        assert!(text.contains("incoming text"), "incoming wins: {text}");
        assert!(
            !text.contains("dest text"),
            "replaced, not duplicated: {text}"
        );
        assert!(text.contains("n.md"));
        assert_eq!(added, 1);
    }

    #[test]
    fn prose_only_lines_are_never_entries_and_never_merge() {
        // A prose line carrying links is not an index entry: it passes
        // through verbatim and never keys a merge decision.
        let dest = b"See [detail](detail.md) for context.\n";
        let incoming = b"- [detail](detail.md) replaced text\n";
        let (merged, _) = merge_memory_index(dest, incoming);
        let text = String::from_utf8(merged).unwrap();
        assert!(text.contains("See [detail](detail.md) for context."));
    }
}

#[cfg(test)]
mod strip_grouped_tests {
    use super::*;
    use std::collections::BTreeSet;

    fn targets(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_grouped_line_is_stripped_only_when_all_its_pointers_are_dangling() {
        // One dangling pointer among four alive ones: dropping the line
        // would delete four live pointers. The strip is conservative —
        // a grouped line survives unless every link on it is dangling.
        let index = b"- [bundle](alive.md) - [dead](dead.md); [alive2](alive2.md)\n";
        let (stripped, dropped) =
            strip_dangling_entries(index, &targets(&["alive.md", "alive2.md"]));
        assert_eq!(dropped, 0, "the bundle keeps its live pointers");
        assert!(String::from_utf8(stripped).unwrap().contains("dead.md"));
    }

    #[test]
    fn a_grouped_line_with_every_link_dangling_is_dropped() {
        let index = b"- [bundle](dead1.md) - [dead2](dead2.md)\n- [alive](alive.md) A\n";
        let (stripped, dropped) = strip_dangling_entries(index, &targets(&["alive.md"]));
        let text = String::from_utf8(stripped).unwrap();
        assert!(
            !text.contains("dead1.md"),
            "all-dangling bundle dropped: {text}"
        );
        assert!(text.contains("alive.md"));
        assert_eq!(dropped, 2);
    }
}
