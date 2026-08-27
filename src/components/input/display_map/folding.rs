use std::ops::Range;

use ropey::Rope;

#[cfg(not(target_family = "wasm"))]
use tree_sitter::Node;
#[cfg(not(target_family = "wasm"))]
pub use tree_sitter::Tree;

use crate::components::input::{RopeExt as _, TabSize};

#[cfg(target_family = "wasm")]
/// Stub type for tree-sitter Tree on WASM (tree-sitter not available).
pub struct Tree;

/// A fold range representing a foldable code region.
///
/// The fold range spans from start_line to end_line (inclusive).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FoldRange {
    /// Start line (inclusive)
    pub start_line: usize,
    /// End line (inclusive)
    pub end_line: usize,
}

impl FoldRange {
    pub fn new(start_line: usize, end_line: usize) -> Self {
        assert!(
            start_line <= end_line,
            "fold start_line must be <= end_line"
        );
        Self {
            start_line,
            end_line,
        }
    }
}

// ==================== Native Implementation (with tree-sitter) ====================

#[cfg(not(target_family = "wasm"))]
/// Check if a named node qualifies as a fold candidate.
///
/// Uses a structural heuristic: any **named** node spanning ≥ MIN_FOLD_LINES
/// is foldable. tree-sitter already parses code into semantic units (functions,
/// classes, blocks, etc.), so named nodes naturally correspond to meaningful
/// foldable regions across all languages without a per-language node-type list.
fn is_foldable_node(node: &Node) -> bool {
    // Skip root node (e.g. `source_file`) and unnamed tokens
    if !node.is_named() || node.parent().is_none() {
        return false;
    }

    let start = node.start_position().row;
    let end = node.end_position().row;
    end > start
}

#[cfg(not(target_family = "wasm"))]
/// Extract fold ranges from a tree-sitter syntax tree (full traversal).
pub fn extract_fold_ranges(tree: &Tree) -> Vec<FoldRange> {
    let mut ranges = Vec::new();
    collect_foldable_nodes(tree.root_node(), &mut ranges);

    ranges.sort_by_key(|r| r.start_line);
    ranges.dedup_by_key(|r| r.start_line);
    ranges
}

#[cfg(not(target_family = "wasm"))]
/// Extract fold ranges only within a byte range (for incremental updates after edits).
///
/// Skips subtrees entirely outside the range, making it O(nodes in range)
/// instead of O(all nodes in tree).
pub fn extract_fold_ranges_in_range(tree: &Tree, byte_range: Range<usize>) -> Vec<FoldRange> {
    let mut ranges = Vec::new();
    collect_foldable_nodes_in_range(tree.root_node(), &byte_range, &mut ranges);

    ranges.sort_by_key(|r| r.start_line);
    ranges.dedup_by_key(|r| r.start_line);
    ranges
}

#[cfg(not(target_family = "wasm"))]
/// Recursively collect foldable nodes, skipping subtrees outside byte_range.
fn collect_foldable_nodes_in_range(
    node: Node,
    byte_range: &Range<usize>,
    ranges: &mut Vec<FoldRange>,
) {
    if node.end_byte() <= byte_range.start || node.start_byte() >= byte_range.end {
        return;
    }

    if is_foldable_node(&node) {
        ranges.push(FoldRange {
            start_line: node.start_position().row,
            end_line: node.end_position().row,
        });
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_foldable_nodes_in_range(child, byte_range, ranges);
    }
}

#[cfg(not(target_family = "wasm"))]
/// Recursively collect foldable nodes from the syntax tree (full traversal).
fn collect_foldable_nodes(node: Node, ranges: &mut Vec<FoldRange>) {
    if is_foldable_node(&node) {
        ranges.push(FoldRange {
            start_line: node.start_position().row,
            end_line: node.end_position().row,
        });
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_foldable_nodes(child, ranges);
    }
}

// ==================== WASM Stub Implementation ====================

#[cfg(target_family = "wasm")]
/// Extract fold ranges - WASM stub (returns empty, no tree-sitter).
pub fn extract_fold_ranges(_tree: &Tree) -> Vec<FoldRange> {
    Vec::new()
}

#[cfg(target_family = "wasm")]
/// Extract fold ranges in range - WASM stub (returns empty, no tree-sitter).
pub fn extract_fold_ranges_in_range(_tree: &Tree, _byte_range: Range<usize>) -> Vec<FoldRange> {
    Vec::new()
}

/// Merge syntax and indent fold ranges.
///
/// Ranges are unique by `start_line`; when both sources share a start line,
/// the range with the larger `end_line` is kept.
pub fn merge_fold_ranges(syntax: Vec<FoldRange>, indent: Vec<FoldRange>) -> Vec<FoldRange> {
    let mut ranges = syntax;
    ranges.extend(indent);
    ranges.sort_by_key(|r| (r.start_line, std::cmp::Reverse(r.end_line)));
    ranges.dedup_by(|a, b| a.start_line == b.start_line);
    ranges
}

/// Indent-based fold ranges (VSCode-style).
///
/// A line is foldable when a later non-empty line has strictly greater indent.
/// The fold ends at the last consecutive deeper line (empty lines in between
/// are included in the hidden span). Trailing empty lines after the last
/// deeper line are left visible so neighbouring blocks stay separated.
pub fn extract_indent_fold_ranges(text: &Rope, tab_size: TabSize) -> Vec<FoldRange> {
    let line_count = text.lines_len();
    if line_count < 2 {
        return Vec::new();
    }

    let mut indents: Vec<Option<usize>> = Vec::with_capacity(line_count);
    for row in 0..line_count {
        let slice = text.slice_line(row);
        if slice.chars().all(|c| c.is_whitespace()) {
            indents.push(None);
        } else {
            indents.push(Some(tab_size.indent_count(&slice)));
        }
    }

    let mut ranges = Vec::new();
    for i in 0..line_count {
        let Some(indent) = indents[i] else {
            continue;
        };
        let mut end = None;
        let mut j = i + 1;
        while j < line_count {
            match indents[j] {
                None => j += 1,
                Some(next) if next > indent => {
                    end = Some(j);
                    j += 1;
                }
                _ => break,
            }
        }
        if let Some(end_line) = end {
            if end_line > i {
                ranges.push(FoldRange::new(i, end_line));
            }
        }
    }

    ranges.sort_by_key(|r| (r.start_line, std::cmp::Reverse(r.end_line)));
    ranges.dedup_by(|a, b| a.start_line == b.start_line);
    ranges
}

/// Outermost fold header for a hidden line, if `line` is inside any folded range.
pub fn hidden_line_fold_header(line: usize, folded: &[FoldRange]) -> Option<usize> {
    folded
        .iter()
        .filter(|f| line > f.start_line && line <= f.end_line)
        .map(|f| f.start_line)
        .min()
}

/// First line after the outermost fold that hides `line`.
pub fn hidden_line_after_fold(line: usize, folded: &[FoldRange]) -> Option<usize> {
    folded
        .iter()
        .filter(|f| line > f.start_line && line <= f.end_line)
        .map(|f| f.end_line)
        .max()
        .map(|end| end + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fold_range_ordering() {
        let mut ranges = vec![
            FoldRange {
                start_line: 10,
                end_line: 20,
            },
            FoldRange {
                start_line: 5,
                end_line: 15,
            },
            FoldRange {
                start_line: 5,
                end_line: 15,
            },
            FoldRange {
                start_line: 1,
                end_line: 30,
            },
        ];

        ranges.sort_by_key(|r| r.start_line);
        ranges.dedup_by_key(|r| r.start_line);

        assert_eq!(ranges.len(), 3);
        assert_eq!(ranges[0].start_line, 1);
        assert_eq!(ranges[1].start_line, 5);
        assert_eq!(ranges[2].start_line, 10);
    }

    #[test]
    fn indent_folds_function_body_and_skips_blank_separator() {
        let text = Rope::from("def foo():\n    a = 1\n    b = 2\n\ndef bar():\n    c = 3\n");
        let ranges = extract_indent_fold_ranges(&text, TabSize::default());
        assert!(
            ranges.contains(&FoldRange::new(0, 2)),
            "foo body should fold, got {ranges:?}"
        );
        assert!(
            ranges.contains(&FoldRange::new(4, 5)),
            "bar body should fold, got {ranges:?}"
        );
    }

    #[test]
    fn indent_folds_include_blank_lines_inside_block() {
        let text = Rope::from("def foo():\n    a = 1\n\n    b = 2\n");
        let ranges = extract_indent_fold_ranges(&text, TabSize::default());
        assert_eq!(ranges, vec![FoldRange::new(0, 3)]);
    }

    #[test]
    fn merge_prefers_larger_end_on_same_start() {
        let syntax = vec![FoldRange::new(0, 20), FoldRange::new(2, 5)];
        let indent = vec![FoldRange::new(0, 18), FoldRange::new(8, 12)];
        let merged = merge_fold_ranges(syntax, indent);
        assert_eq!(
            merged,
            vec![
                FoldRange::new(0, 20),
                FoldRange::new(2, 5),
                FoldRange::new(8, 12),
            ]
        );
    }

    #[test]
    fn hidden_line_clamp_uses_outermost_fold() {
        let folded = vec![FoldRange::new(0, 10), FoldRange::new(2, 6)];
        assert_eq!(hidden_line_fold_header(4, &folded), Some(0));
        assert_eq!(hidden_line_after_fold(4, &folded), Some(11));
        assert_eq!(hidden_line_fold_header(0, &folded), None);
        assert_eq!(hidden_line_fold_header(11, &folded), None);
    }

    #[test]
    fn clamp_forward_skips_hidden_closing_line() {
        let text = Rope::from("fn foo() {\n    a();\n}\nnext\n");
        let folded = vec![FoldRange::new(0, 2)];
        let after = hidden_line_after_fold(2, &folded).expect("line 2 is hidden");
        assert_eq!(after, 3);
        assert_eq!(text.slice_line(after).to_string(), "next");
        assert_eq!(hidden_line_fold_header(2, &folded), Some(0));
        assert_eq!(text.slice_line(0).to_string(), "fn foo() {");
    }
}
