/// FoldMap: Folding projection layer (Wrap rows → Display rows).
///
/// This module manages code folding by:
/// - Filtering out wrap rows that belong to folded regions
/// - Maintaining bidirectional mapping: wrap_row ↔ display_row
/// - Handling fold state changes and rebuilding the projection
use super::folding::FoldRange;
use super::wrap_map::WrapMap;

/// FoldMap projects wrap rows to display rows by hiding folded regions.
pub struct FoldMap {
    /// Mapping: display_row → wrap_row
    /// index = display_row, value = actual wrap_row
    visible_wrap_rows: Vec<usize>,

    /// Reverse mapping: wrap_row → display_row
    /// index = wrap_row, value = Some(display_row) if visible, None if folded
    wrap_row_to_display_row: Vec<Option<usize>>,

    /// Candidate fold ranges (from tree-sitter)
    /// Sorted by start_line, unique start_line
    candidates: Vec<FoldRange>,

    /// Currently folded ranges
    /// Subset of candidates, sorted by start_line
    folded: Vec<FoldRange>,

    /// Flag indicating if the fold projection needs rebuilding
    /// Used for lazy evaluation to avoid expensive rebuilds on every text change
    needs_rebuild: bool,

    /// Cached wrap_row_count from last rebuild
    /// Used to detect if WrapMap changed and rebuild is needed
    cached_wrap_row_count: usize,
}

impl FoldMap {
    pub fn new() -> Self {
        Self {
            visible_wrap_rows: Vec::new(),
            wrap_row_to_display_row: Vec::new(),
            candidates: Vec::new(),
            folded: Vec::new(),
            needs_rebuild: true,
            cached_wrap_row_count: 0,
        }
    }

    /// Update cached wrap_row_count without full rebuild.
    /// Used when no folds are active (identity mapping assumed).
    pub(super) fn mark_dirty_with_wrap_count(&mut self, wrap_row_count: usize) {
        self.needs_rebuild = true;
        self.cached_wrap_row_count = wrap_row_count;
    }

    /// Get total number of visible display rows
    pub fn display_row_count(&self) -> usize {
        if self.folded.is_empty() {
            return self.cached_wrap_row_count;
        }
        self.visible_wrap_rows.len()
    }

    /// Convert wrap_row to display_row
    /// Returns None if the wrap_row is hidden by folding
    pub fn wrap_row_to_display_row(&self, wrap_row: usize) -> Option<usize> {
        if self.folded.is_empty() {
            return if wrap_row < self.cached_wrap_row_count {
                Some(wrap_row)
            } else {
                None
            };
        }
        self.wrap_row_to_display_row
            .get(wrap_row)
            .copied()
            .flatten()
    }

    /// Convert display_row to wrap_row
    pub fn display_row_to_wrap_row(&self, display_row: usize) -> Option<usize> {
        if self.folded.is_empty() {
            return if display_row < self.cached_wrap_row_count {
                Some(display_row)
            } else {
                None
            };
        }
        self.visible_wrap_rows.get(display_row).copied()
    }

    /// Find the nearest visible display_row for a given wrap_row
    pub fn nearest_visible_display_row(&self, wrap_row: usize) -> usize {
        if self.folded.is_empty() {
            return wrap_row.min(self.cached_wrap_row_count.saturating_sub(1));
        }

        if let Some(dr) = self.wrap_row_to_display_row(wrap_row) {
            return dr;
        }

        match self.visible_wrap_rows.binary_search(&wrap_row) {
            Ok(idx) => idx,
            Err(insert_pos) => insert_pos.saturating_sub(1),
        }
    }

    /// Set fold candidates (from tree-sitter), full replacement.
    pub fn set_candidates(&mut self, mut candidates: Vec<FoldRange>) {
        // Sort and deduplicate by start_line
        candidates.sort_by_key(|r| (r.start_line, std::cmp::Reverse(r.end_line)));
        candidates.dedup_by(|a, b| a.start_line == b.start_line);
        self.candidates = candidates;

        // Remove any folded ranges that are no longer in candidates
        self.folded.retain(|fold| {
            self.candidates
                .binary_search_by_key(&fold.start_line, |c| c.start_line)
                .is_ok()
        });
    }

    /// Merge new candidates extracted from an edited region into existing candidates.
    ///
    /// Replaces candidates within [edit_start_line, edit_end_line] with `new_candidates`,
    /// keeping candidates outside the edit range intact.
    pub fn merge_candidates_for_edit(
        &mut self,
        edit_start_line: usize,
        edit_end_line: usize,
        new_candidates: Vec<FoldRange>,
    ) {
        // Remove old candidates within the edit range (already done by adjust_folds_for_edit)
        // But do it again in case adjust wasn't called or range differs
        self.candidates
            .retain(|c| c.start_line < edit_start_line || c.start_line > edit_end_line);

        // Add new candidates
        self.candidates.extend(new_candidates);
        self.candidates
            .sort_by_key(|r| (r.start_line, std::cmp::Reverse(r.end_line)));
        self.candidates.dedup_by(|a, b| a.start_line == b.start_line);
    }

    /// Set a fold at the given start_line (must be in candidates)
    pub fn set_folded(&mut self, start_line: usize, folded: bool) {
        if folded {
            // Find the candidate range for this start_line
            if let Ok(idx) = self
                .candidates
                .binary_search_by_key(&start_line, |c| c.start_line)
            {
                // Add to folded if not already present, maintaining sorted order
                if let Err(insert_pos) = self
                    .folded
                    .binary_search_by_key(&start_line, |f| f.start_line)
                {
                    self.folded.insert(insert_pos, self.candidates[idx]);
                    self.needs_rebuild = true;
                }
            }
        } else {
            // Remove from folded
            if let Ok(idx) = self
                .folded
                .binary_search_by_key(&start_line, |f| f.start_line)
            {
                self.folded.remove(idx);
                self.needs_rebuild = true;
            }
        }
    }

    /// Toggle fold at the given start_line
    pub fn toggle_fold(&mut self, start_line: usize) {
        let is_folded = self.is_folded_at(start_line);
        self.set_folded(start_line, !is_folded);
    }

    /// Check if a line is currently folded
    pub fn is_folded_at(&self, start_line: usize) -> bool {
        self.folded
            .binary_search_by_key(&start_line, |f| f.start_line)
            .is_ok()
    }

    /// Check if a line is a fold candidate
    pub fn is_fold_candidate(&self, start_line: usize) -> bool {
        self.candidates
            .binary_search_by_key(&start_line, |c| c.start_line)
            .is_ok()
    }

    /// Get all fold candidates
    #[inline]
    pub fn fold_candidates(&self) -> &[FoldRange] {
        &self.candidates
    }

    /// Get all currently folded ranges
    #[inline]
    pub fn folded_ranges(&self) -> &[FoldRange] {
        &self.folded
    }

    /// Clear all folds
    pub fn clear_folds(&mut self) {
        if !self.folded.is_empty() {
            self.folded.clear();
            self.needs_rebuild = true;
        }
    }

    /// Fold every candidate range.
    pub fn fold_all(&mut self) {
        if self.folded.len() != self.candidates.len() {
            self.folded = self.candidates.clone();
            self.needs_rebuild = true;
        }
    }

    /// Fold all candidates whose nesting depth is >= `level` (1 = outermost).
    /// Unfolds shallower regions first so only the requested level stays collapsed.
    pub fn fold_level(&mut self, level: usize) {
        if level == 0 {
            self.clear_folds();
            return;
        }
        let candidates = self.candidates.clone();
        self.folded.clear();
        for candidate in &candidates {
            if Self::nesting_depth(&candidates, candidate) >= level {
                self.folded.push(*candidate);
            }
        }
        self.needs_rebuild = true;
    }

    /// Nesting depth of a range: 1 if not contained in any other candidate.
    pub fn nesting_depth(candidates: &[FoldRange], range: &FoldRange) -> usize {
        let mut depth = 1;
        for other in candidates {
            if other.start_line == range.start_line && other.end_line == range.end_line {
                continue;
            }
            let contains = other.start_line <= range.start_line
                && other.end_line >= range.end_line
                && (other.start_line < range.start_line || other.end_line > range.end_line);
            if contains {
                depth += 1;
            }
        }
        depth
    }

    fn containing_sorted(ranges: &[FoldRange], line: usize) -> Vec<FoldRange> {
        let mut containing: Vec<FoldRange> = ranges
            .iter()
            .copied()
            .filter(|c| c.start_line <= line && line <= c.end_line)
            .collect();
        containing.sort_by_key(|c| {
            (
                c.end_line.saturating_sub(c.start_line),
                std::cmp::Reverse(c.start_line),
            )
        });
        containing
    }

    /// Fold the innermost unfolded candidate that contains `line`.
    pub fn fold_at_line(&mut self, line: usize) -> bool {
        for range in Self::containing_sorted(&self.candidates, line) {
            if !self.is_folded_at(range.start_line) {
                self.set_folded(range.start_line, true);
                return true;
            }
        }
        false
    }

    /// Unfold the innermost folded range that contains `line`.
    pub fn unfold_at_line(&mut self, line: usize) -> bool {
        if let Some(range) = Self::containing_sorted(&self.folded, line).first().copied() {
            self.set_folded(range.start_line, false);
            return true;
        }
        false
    }

    /// Toggle the innermost region at `line`: unfold if folded, otherwise fold.
    pub fn toggle_fold_at_line(&mut self, line: usize) {
        if !self.unfold_at_line(line) {
            self.fold_at_line(line);
        }
    }

    /// Adjust folds and candidates after a text edit.
    ///
    /// - Folds/candidates overlapping the edited line range are removed
    /// - Folds/candidates after the edit are shifted by line_delta
    ///
    /// This avoids expensive full tree traversal on every keystroke.
    pub fn adjust_folds_for_edit(
        &mut self,
        edit_start_line: usize,
        edit_end_line: usize,
        line_delta: isize,
    ) {
        // Adjust folded ranges
        if !self.folded.is_empty() {
            self.folded.retain(|fold| {
                !(fold.start_line <= edit_end_line && fold.end_line >= edit_start_line)
            });

            if line_delta != 0 {
                for fold in &mut self.folded {
                    if fold.start_line > edit_end_line {
                        fold.start_line = (fold.start_line as isize + line_delta).max(0) as usize;
                        fold.end_line = (fold.end_line as isize + line_delta).max(0) as usize;
                    }
                }
            }
        }

        // Adjust candidates the same way
        if !self.candidates.is_empty() {
            self.candidates
                .retain(|c| !(c.start_line <= edit_end_line && c.end_line >= edit_start_line));

            if line_delta != 0 {
                for c in &mut self.candidates {
                    if c.start_line > edit_end_line {
                        c.start_line = (c.start_line as isize + line_delta).max(0) as usize;
                        c.end_line = (c.end_line as isize + line_delta).max(0) as usize;
                    }
                }
            }
        }

        self.needs_rebuild = true;
    }

    /// Rebuild the fold mapping after wrap_map or fold state changes
    ///
    /// This is the core algorithm that projects wrap rows to display rows.
    pub fn rebuild(&mut self, wrap_map: &WrapMap) {
        let wrap_row_count = wrap_map.wrap_row_count();

        // Performance optimization: skip rebuild if nothing changed
        if !self.needs_rebuild && wrap_row_count == self.cached_wrap_row_count {
            return;
        }

        self.cached_wrap_row_count = wrap_row_count;

        self.visible_wrap_rows.clear();
        self.wrap_row_to_display_row.clear();
        self.wrap_row_to_display_row.resize(wrap_row_count, None);

        if self.folded.is_empty() {
            // Fast path: no folds, all wrap rows are visible
            self.visible_wrap_rows.extend(0..wrap_row_count);
            for (display_row, &wrap_row) in self.visible_wrap_rows.iter().enumerate() {
                self.wrap_row_to_display_row[wrap_row] = Some(display_row);
            }
            self.needs_rebuild = false;
            return;
        }

        // Build set of hidden wrap_row ranges from folded buffer lines
        let mut hidden_ranges = Vec::new();
        for fold in &self.folded {
            // VSCode-style: keep the start line visible, hide start+1..=end
            // (including the closing brace / last line of the region).
            let hide_start_line = fold.start_line + 1;
            let hide_end_line = fold.end_line;

            if hide_start_line > hide_end_line {
                continue;
            }

            // Get wrap_row ranges for the hidden buffer lines
            let start_wrap_row = wrap_map.buffer_line_to_first_wrap_row(hide_start_line);
            let end_wrap_row = if hide_end_line + 1 < wrap_map.buffer_line_count() {
                wrap_map.buffer_line_to_first_wrap_row(hide_end_line + 1)
            } else {
                wrap_row_count
            };

            if start_wrap_row < end_wrap_row {
                hidden_ranges.push(start_wrap_row..end_wrap_row);
            }
        }

        // Merge overlapping hidden ranges
        hidden_ranges.sort_by_key(|r| r.start);
        let mut merged_hidden: Vec<usize> = Vec::new();
        for range in hidden_ranges {
            if let Some(last) = merged_hidden.last_mut() {
                if range.start <= *last {
                    // Overlapping or adjacent, merge
                    *last = (*last).max(range.end);
                } else {
                    merged_hidden.push(range.start);
                    merged_hidden.push(range.end);
                }
            } else {
                merged_hidden.push(range.start);
                merged_hidden.push(range.end);
            }
        }

        // Scan all wrap rows and filter out hidden ones
        let mut display_row = 0;
        let mut hidden_iter = merged_hidden.chunks_exact(2);
        let mut current_hidden = hidden_iter.next();

        for wrap_row in 0..wrap_row_count {
            // Check if wrap_row is in current hidden range
            let is_hidden = if let Some(&[start, end]) = current_hidden {
                if wrap_row >= end {
                    current_hidden = hidden_iter.next();
                    if let Some(&[new_start, new_end]) = current_hidden {
                        wrap_row >= new_start && wrap_row < new_end
                    } else {
                        false
                    }
                } else {
                    wrap_row >= start && wrap_row < end
                }
            } else {
                false
            };

            if !is_hidden {
                self.visible_wrap_rows.push(wrap_row);
                self.wrap_row_to_display_row[wrap_row] = Some(display_row);
                display_row += 1;
            }
        }

        self.needs_rebuild = false;
    }

    /// Test helper: rebuild assuming one wrap row per buffer line.
    #[cfg(test)]
    fn rebuild_identity(&mut self, line_count: usize) {
        self.needs_rebuild = true;
        self.cached_wrap_row_count = line_count;
        self.visible_wrap_rows.clear();
        self.wrap_row_to_display_row.clear();
        self.wrap_row_to_display_row.resize(line_count, None);

        let mut hidden = vec![false; line_count];
        for fold in &self.folded {
            let start = fold.start_line.saturating_add(1);
            if start >= line_count || start > fold.end_line {
                continue;
            }
            let end = fold.end_line.min(line_count.saturating_sub(1));
            for line in start..=end {
                hidden[line] = true;
            }
        }

        let mut display_row = 0;
        for wrap_row in 0..line_count {
            if !hidden[wrap_row] {
                self.visible_wrap_rows.push(wrap_row);
                self.wrap_row_to_display_row[wrap_row] = Some(display_row);
                display_row += 1;
            }
        }
        self.needs_rebuild = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hides_start_plus_one_through_end() {
        let mut map = FoldMap::new();
        map.set_candidates(vec![FoldRange::new(0, 3)]);
        map.set_folded(0, true);
        map.rebuild_identity(5);

        assert_eq!(map.wrap_row_to_display_row(0), Some(0));
        assert_eq!(map.wrap_row_to_display_row(1), None);
        assert_eq!(map.wrap_row_to_display_row(2), None);
        assert_eq!(map.wrap_row_to_display_row(3), None);
        assert_eq!(map.wrap_row_to_display_row(4), Some(1));
        assert_eq!(map.display_row_count(), 2);
    }

    #[test]
    fn nested_folds_hide_inner_and_outer() {
        let mut map = FoldMap::new();
        map.set_candidates(vec![FoldRange::new(0, 8), FoldRange::new(2, 5)]);
        map.set_folded(2, true);
        map.rebuild_identity(10);

        assert!(map.wrap_row_to_display_row(2).is_some());
        assert_eq!(map.wrap_row_to_display_row(3), None);
        assert_eq!(map.wrap_row_to_display_row(5), None);
        assert!(map.wrap_row_to_display_row(6).is_some());

        map.set_folded(0, true);
        map.rebuild_identity(10);
        assert!(map.wrap_row_to_display_row(0).is_some());
        for line in 1..=8 {
            assert_eq!(map.wrap_row_to_display_row(line), None, "line {line}");
        }
        assert!(map.wrap_row_to_display_row(9).is_some());
    }

    #[test]
    fn fold_level_collapses_nested_depths() {
        let mut map = FoldMap::new();
        let outer = FoldRange::new(0, 10);
        let inner = FoldRange::new(2, 6);
        let inner2 = FoldRange::new(3, 5);
        map.set_candidates(vec![outer, inner, inner2]);

        assert_eq!(FoldMap::nesting_depth(map.fold_candidates(), &outer), 1);
        assert_eq!(FoldMap::nesting_depth(map.fold_candidates(), &inner), 2);
        assert_eq!(FoldMap::nesting_depth(map.fold_candidates(), &inner2), 3);

        map.fold_level(2);
        assert!(!map.is_folded_at(0));
        assert!(map.is_folded_at(2));
        assert!(map.is_folded_at(3));

        map.fold_all();
        assert!(map.is_folded_at(0));
        map.clear_folds();
        assert!(!map.is_folded_at(0));
        assert!(map.folded_ranges().is_empty());
    }

    #[test]
    fn fold_at_line_picks_innermost_unfolded() {
        let mut map = FoldMap::new();
        map.set_candidates(vec![FoldRange::new(0, 10), FoldRange::new(2, 6)]);
        assert!(map.fold_at_line(4));
        assert!(map.is_folded_at(2));
        assert!(!map.is_folded_at(0));

        assert!(map.fold_at_line(4));
        assert!(map.is_folded_at(0));

        map.toggle_fold_at_line(4);
        assert!(!map.is_folded_at(2));
        assert!(map.is_folded_at(0));
    }
}
