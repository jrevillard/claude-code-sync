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

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// The file name whose contents are union-merged rather than overwritten.
pub const MEMORY_INDEX_FILE: &str = "MEMORY.md";

/// One line of an index: an entry carries the link target it is keyed by.
enum IndexLine {
    Entry { target: String, text: String },
    Other(String),
}

/// True when this path's file name is a memory index.
pub fn is_memory_index(path: &std::path::Path) -> bool {
    path.file_name().map(|name| name == MEMORY_INDEX_FILE) == Some(true)
}

fn link_target(line: &str) -> Option<String> {
    let start = line.find("](")? + 2;
    let rest = &line[start..];
    let end = rest.find(')')?;
    Some(rest[..end].to_string())
}

fn parse(text: &str) -> Vec<IndexLine> {
    text.lines()
        .map(|line| match link_target(line) {
            Some(target) if line.starts_with("- ") => IndexLine::Entry {
                target,
                text: line.to_string(),
            },
            _ => IndexLine::Other(line.to_string()),
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

    let incoming_entries: BTreeMap<&str, &str> = incoming_lines
        .iter()
        .filter_map(|line| match line {
            IndexLine::Entry { target, text } => Some((target.as_str(), text.as_str())),
            IndexLine::Other(_) => None,
        })
        .collect();

    let dest_targets: BTreeSet<&str> = dest_lines
        .iter()
        .filter_map(|line| match line {
            IndexLine::Entry { target, .. } => Some(target.as_str()),
            IndexLine::Other(_) => None,
        })
        .collect();

    if dest_targets.is_empty() && dest.is_empty() {
        return (incoming.to_vec(), incoming_entries.len());
    }

    let mut merged = String::new();
    for line in &dest_lines {
        let text = match line {
            IndexLine::Entry { target, text } => incoming_entries
                .get(target.as_str())
                .copied()
                .unwrap_or(text.as_str()),
            IndexLine::Other(text) => text.as_str(),
        };
        merged.push_str(text);
        merged.push('\n');
    }

    let mut added = 0;
    for (target, text) in &incoming_entries {
        if dest_targets.contains(target) {
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
        let strippable = link_target(line).and_then(|target| {
            strippable_target(&target).map(|base| (target.clone(), base.to_string()))
        });
        match strippable {
            Some((_raw, base)) if !known_targets.contains(&base) => {
                dropped.push(base);
            }
            _ => {
                kept.push_str(line);
                kept.push('\n');
            }
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
        .filter_map(|line| match line {
            IndexLine::Entry { target, .. } => Some(target.clone()),
            IndexLine::Other(_) => None,
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
        .filter_map(|line| link_target(line))
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
    for project in entries.flatten() {
        let memory = project.path().join("memory");
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
