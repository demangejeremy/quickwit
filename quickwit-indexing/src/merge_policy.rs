// Copyright (C) 2022 Quickwit, Inc.
//
// Quickwit is offered under the AGPL v3.0 and as commercial software.
// For commercial licensing, contact us at hello@quickwit.io.
//
// AGPL:
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU Affero General Public License as
// published by the Free Software Foundation, either version 3 of the
// License, or (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU Affero General Public License for more details.
//
// You should have received a copy of the GNU Affero General Public License
// along with this program. If not, see <http://www.gnu.org/licenses/>.

use std::cmp::Reverse;
use std::fmt;
use std::ops::Range;

use quickwit_metastore::SplitMetadata;
use tracing::debug;

use crate::new_split_id;

pub struct MergeOperation {
    pub merge_split_id: String,
    pub splits: Vec<SplitMetadata>,
}

impl MergeOperation {
    pub fn new_merge_operation(splits: Vec<SplitMetadata>) -> MergeOperation {
        Self {
            merge_split_id: new_split_id(),
            splits,
        }
    }

    pub fn splits_as_slice(&self) -> &[SplitMetadata] {
        self.splits.as_slice()
    }
}

impl fmt::Debug for MergeOperation {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Merge(merged_split_id={},splits=[", self.merge_split_id)?;
        for split in &self.splits {
            write!(f, "{},", split.split_id())?;
        }
        write!(f, "])")?;
        Ok(())
    }
}

/// A merge policy wraps the logic that decide what should be merged.
/// The SplitMetadata must be extracted from the splits `Vec`.
///
/// It is called by the merge planner whenever a new split is added.
pub trait MergePolicy: Send + Sync + fmt::Debug {
    /// Returns the list of merge operations that should be performed.
    fn operations(&self, splits: &mut Vec<SplitMetadata>) -> Vec<MergeOperation>;
    /// A mature split is a split that won't undergo a merge operation in the future.
    fn is_mature(&self, split: &SplitMetadata) -> bool;
}

/// StableMultitenantMergePolicy is a rather naive implementation optimized
/// for splits produced by a rather stable stream of splits,
/// with incoming documents ordered more or less as expected time, so that splits are
/// time pruning is efficient out of the box.
///
/// The logic goes as follows.
/// Each splits has
/// - a number of documents
/// - an end time
///
/// The policy first builds the merge operations
///
/// 1. Build merge operations
/// We start by sorting the splits by reverse date so that the most recent splits are
/// coming first.
/// We iterate through the splits and assign them to increasing levels.
/// Level 0 will receive `{split_i}` for i within `[0..l_0)`
/// ...
/// Level k will receive `{split_i}` for i within `[l_{k-1}..l_k)`
///
/// The limit at which we change level is simply defined as
/// `l_0 = 3 x self.min_level_num_docs`.
///
/// Assuming level N-1 has been built, level N is given by
/// `l_N = min(num_docs(split_l_{N_1})` * 3, self.max_merge_docs)`.
/// We stop once l_N = self.max_merge_docs is reached.
///
/// As a result, each level interval is at least 3 times larger than the previous one,
/// forming a logscale over the number of documents.
///
/// Because we stop merging splits reaching a size larger than if it would result in a size larger
/// than `target_num_docs`.
#[derive(Clone, Debug)]
pub struct StableMultitenantWithTimestampMergePolicy {
    pub min_level_num_docs: usize,
    pub merge_enabled: bool,
    pub merge_factor: usize,
    pub max_merge_factor: usize,
    /// The merge policy aims to eventually produce mature splits that have a larger size but
    /// are within close range of `split_num_docs_target`.
    ///
    /// In other words, splits that contain a number of documents greater than or equal to
    /// `split_num_docs_target` are considered mature and never merged.
    pub split_num_docs_target: usize,
}

impl Default for StableMultitenantWithTimestampMergePolicy {
    fn default() -> Self {
        StableMultitenantWithTimestampMergePolicy {
            min_level_num_docs: 100_000,
            merge_enabled: true,
            merge_factor: 10,
            max_merge_factor: 12,
            split_num_docs_target: 10_000_000,
        }
    }
}

fn remove_matching_items<T, Pred: Fn(&T) -> bool>(items: &mut Vec<T>, predicate: Pred) -> Vec<T> {
    let mut matching_items = Vec::new();
    let mut i = 0;
    while i < items.len() {
        if predicate(&items[i]) {
            let matching_item = items.remove(i);
            matching_items.push(matching_item);
        } else {
            i += 1;
        }
    }
    matching_items
}

struct SplitShortDebug<'a>(&'a SplitMetadata);

impl<'a> fmt::Debug for SplitShortDebug<'a> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Split")
            .field("split_id", &self.0.split_id())
            .field("num_docs", &self.0.num_docs)
            .finish()
    }
}

fn splits_short_debug(splits: &[SplitMetadata]) -> Vec<SplitShortDebug> {
    splits.iter().map(SplitShortDebug).collect()
}

impl MergePolicy for StableMultitenantWithTimestampMergePolicy {
    fn operations(&self, splits: &mut Vec<SplitMetadata>) -> Vec<MergeOperation> {
        let original_num_splits = splits.len();
        let operations = self.merge_operations(splits);
        debug_assert_eq!(
            original_num_splits,
            operations
                .iter()
                .map(|op| op.splits_as_slice().len())
                .sum::<usize>()
                + splits.len(),
            "The merge policy is supposed to keep the number of splits."
        );
        operations
    }

    fn is_mature(&self, split: &SplitMetadata) -> bool {
        self.is_mature_for_merge(split)
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum MergeCandidateSize {
    /// The split candidate is too small to be considered for execution.
    TooSmall,
    /// The split candidate is good to go.
    ValidSplit,
    /// We should not add an extra split in this candidate.
    /// This can happen for any of the two following reasons:
    /// - the number of splits involved already reached `merge_factor_max`.
    /// - the overall number of docs that will end up in the merged segment already
    /// exceeds `max_merge_docs`.
    OneMoreSplitWouldBeTooBig,
}

impl StableMultitenantWithTimestampMergePolicy {
    /// A mature split for merge is a split that won't undergo merge operation in the future.
    fn is_mature_for_merge(&self, split: &SplitMetadata) -> bool {
        // If merge is disabled, split is considered mature as it will not undergo a merge
        // operation.
        if !self.merge_enabled {
            return true;
        }

        split.num_docs >= self.split_num_docs_target
    }

    fn merge_operations(&self, splits: &mut Vec<SplitMetadata>) -> Vec<MergeOperation> {
        if !self.merge_enabled || splits.len() < 2 {
            return Vec::new();
        }
        // First we isolate splits that are mature.
        let splits_not_for_merge =
            remove_matching_items(splits, |split| self.is_mature_for_merge(split));

        let mut merge_operations: Vec<MergeOperation> = Vec::new();
        // We stable sort the splits, most recent first.
        splits.sort_by_key(|split| {
            let time_end = split
                .time_range
                .as_ref()
                .map(|time_range| Reverse(*time_range.end()));
            (time_end, split.num_docs)
        });
        debug!(splits=?splits_short_debug(&splits[..]), "merge-policy-run");

        // Splits should naturally have an increasing num_merge
        let split_levels = self.build_split_levels(splits);
        for split_range in split_levels.into_iter().rev() {
            debug!(splits=?splits_short_debug(&splits[split_range.clone()]));
            if let Some(merge_range) = self.merge_candidate_from_level(splits, split_range) {
                debug!(merge_range=?merge_range, "merge-candidate");
                let splits_in_merge: Vec<SplitMetadata> = splits.drain(merge_range).collect();
                let merge_operation = MergeOperation::new_merge_operation(splits_in_merge);
                merge_operations.push(merge_operation);
            } else {
                debug!("no-merge");
            }
        }
        splits.extend(splits_not_for_merge);
        merge_operations
    }

    /// This function groups splits in levels.
    ///
    /// It assumes that splits are almost sorted by their increasing size,
    /// but should behave decently (not create too many levels) if they are not.
    ///
    /// All splits are required to have a number of documents lower than
    /// `self.max_merge_docs`
    pub(crate) fn build_split_levels(&self, splits: &[SplitMetadata]) -> Vec<Range<usize>> {
        assert!(
            splits
                .iter()
                .all(|split| split.num_docs < self.split_num_docs_target),
            "All splits are expected to be smaller than `max_merge_docs`."
        );
        if splits.is_empty() {
            return Vec::new();
        }

        let mut split_levels: Vec<Range<usize>> = Vec::new();
        let mut current_level_start_ord = 0;
        let mut current_level_max_docs = (splits[0].num_docs * 3).max(self.min_level_num_docs);

        let mut levels = vec![(0..current_level_max_docs)]; // for logging only
        for (split_ord, split) in splits.iter().enumerate() {
            if split.num_docs >= current_level_max_docs {
                split_levels.push(current_level_start_ord..split_ord);
                current_level_start_ord = split_ord;
                current_level_max_docs = 3 * split.num_docs;
                levels.push(split.num_docs..current_level_max_docs)
            }
        }
        debug!(levels=?levels);
        split_levels.push(current_level_start_ord..splits.len());
        split_levels
    }

    /// Given splits tries to select a subrange of level_range that would be a good merge candidate.
    fn merge_candidate_from_level(
        &self,
        splits: &[SplitMetadata],
        level_range: Range<usize>,
    ) -> Option<Range<usize>> {
        let merge_candidate_end = level_range.end;
        let mut merge_candidate_start = merge_candidate_end;
        for split_ord in level_range.rev() {
            if self.merge_candidate_size(&splits[merge_candidate_start..merge_candidate_end])
                == MergeCandidateSize::OneMoreSplitWouldBeTooBig
            {
                break;
            }
            merge_candidate_start = split_ord;
        }
        if self.merge_candidate_size(&splits[merge_candidate_start..merge_candidate_end])
            == MergeCandidateSize::TooSmall
        {
            return None;
        }
        Some(merge_candidate_start..merge_candidate_end)
    }

    /// Returns `MergeCandidateSize` iff we should stop adding extra split into this
    /// merge candidate.
    fn merge_candidate_size(&self, splits: &[SplitMetadata]) -> MergeCandidateSize {
        // We don't perform merge with a single segment. We
        // may relax this in the future in order to compact deletes.
        if splits.len() <= 1 {
            return MergeCandidateSize::TooSmall;
        }

        // There are already enough splits in this merge.
        if splits.len() >= self.max_merge_factor {
            return MergeCandidateSize::OneMoreSplitWouldBeTooBig;
        }
        let num_docs_in_merge: usize = splits.iter().map(|split| split.num_docs).sum();

        // The resulting split will exceed `max_merge_docs`.
        if num_docs_in_merge >= self.split_num_docs_target {
            return MergeCandidateSize::OneMoreSplitWouldBeTooBig;
        }

        if splits.len() < self.merge_factor {
            return MergeCandidateSize::TooSmall;
        }

        MergeCandidateSize::ValidSplit
    }

    fn case_levels_given_growth_factor(&self, growth_factor: usize) -> Vec<usize> {
        assert!(self.min_level_num_docs > 0);
        assert!(self.merge_factor > 1);
        assert!(self.max_merge_factor >= self.merge_factor);
        assert!(self.split_num_docs_target > self.min_level_num_docs);
        let mut levels_start_num_docs = vec![1];
        let mut level_end_doc = self.min_level_num_docs;
        while level_end_doc < self.split_num_docs_target {
            levels_start_num_docs.push(level_end_doc);
            level_end_doc *= growth_factor;
        }
        levels_start_num_docs.push(self.split_num_docs_target);
        levels_start_num_docs
    }

    pub fn max_num_splits_ideal_case(&self, num_docs: u64) -> usize {
        let levels = self.case_levels_given_growth_factor(self.merge_factor);
        self.max_num_splits_knowning_levels(num_docs, &levels, true)
    }

    pub fn max_num_splits_worst_case(&self, num_docs: u64) -> usize {
        let levels = self.case_levels_given_growth_factor(3);
        self.max_num_splits_knowning_levels(num_docs, &levels, false)
    }

    fn max_num_splits_knowning_levels(
        &self,
        mut num_docs: u64,
        levels: &[usize],
        sorted: bool,
    ) -> usize {
        assert!(is_sorted(levels));
        if num_docs == 0 {
            return 0;
        }
        let (&head, tail) = levels.split_first().unwrap();
        if num_docs < head as u64 {
            return 0;
        }
        let first_level_min_saturation_docs = if sorted {
            head * (self.merge_factor - 1)
        } else {
            head + (self.merge_factor - 2)
        };
        if tail.is_empty() || num_docs <= first_level_min_saturation_docs as u64 {
            return (num_docs as usize + head - 1) / head;
        }
        num_docs -= first_level_min_saturation_docs as u64;
        self.merge_factor - 1 + self.max_num_splits_knowning_levels(num_docs, tail, sorted)
    }
}

fn is_sorted(els: &[usize]) -> bool {
    els.windows(2).all(|w| w[0] <= w[1])
}

#[cfg(test)]
mod tests {

    use std::ops::RangeInclusive;

    use super::*;

    fn create_splits(num_docs_vec: Vec<usize>) -> Vec<SplitMetadata> {
        let num_docs_with_timestamp = num_docs_vec
            .into_iter()
            // we give the same timestamp to all of them and rely on stable sort to keep the split
            // order.
            .map(|num_docs| (num_docs, (1630563067..=1630564067)))
            .collect();
        create_splits_with_timestamps(num_docs_with_timestamp)
    }

    fn create_splits_with_timestamps(
        num_docs_vec: Vec<(usize, RangeInclusive<i64>)>,
    ) -> Vec<SplitMetadata> {
        num_docs_vec
            .into_iter()
            .enumerate()
            .map(|(split_ord, (num_docs, time_range))| SplitMetadata {
                split_id: format!("split_{:02}", split_ord),
                num_docs,
                time_range: Some(time_range),
                ..Default::default()
            })
            .collect()
    }

    #[test]
    fn test_split_is_mature() {
        let merge_policy = StableMultitenantWithTimestampMergePolicy::default();
        // Split under max_merge_docs is not mature.
        let mut split = create_splits(vec![9_000_000]).into_iter().next().unwrap();
        assert!(!merge_policy.is_mature(&split));
        // All splits are mature when merge is disabled.
        let merge_policy_with_disabled_merge = StableMultitenantWithTimestampMergePolicy {
            merge_enabled: false,
            ..Default::default()
        };
        assert!(merge_policy_with_disabled_merge.is_mature(&split));
        // Split under max_merge_docs is not mature.
        assert!(!merge_policy.is_mature(&split));
        // Split with docs > max_merge_docs is mature.
        split.num_docs = merge_policy.split_num_docs_target + 1;
        assert!(merge_policy.is_mature(&split));
    }

    #[test]
    fn test_build_split_levels() {
        let merge_policy = StableMultitenantWithTimestampMergePolicy::default();
        let splits = Vec::new();
        let split_groups = merge_policy.build_split_levels(&splits);
        assert!(split_groups.is_empty());
    }

    #[test]
    fn test_stable_multitenant_merge_policy_build_split_simple() {
        let merge_policy = StableMultitenantWithTimestampMergePolicy::default();
        let splits = create_splits(vec![100_000, 100_000, 100_000, 800_000, 900_000]);
        let split_groups = merge_policy.build_split_levels(&splits);
        assert_eq!(&split_groups, &[0..3, 3..5]);
    }

    #[test]
    fn test_stable_multitenant_merge_policy_build_split_perfect_world() {
        let merge_policy = StableMultitenantWithTimestampMergePolicy::default();
        let splits = create_splits(vec![
            100_000, 100_000, 100_000, 100_000, 100_000, 100_000, 100_000, 100_000, 800_000,
            1_600_000,
        ]);
        let split_groups = merge_policy.build_split_levels(&splits);
        assert_eq!(&split_groups, &[0..8, 8..10]);
    }

    #[test]
    fn test_stable_multitenant_merge_policy_build_split_decreasing() {
        let merge_policy = StableMultitenantWithTimestampMergePolicy::default();
        let splits = create_splits(vec![
            100_000, 100_000, 100_000, 100_000, 100_000, 100_000, 100_000, 100_000, 800_000,
            100_000, 1_600_000,
        ]);
        let split_groups = merge_policy.build_split_levels(&splits);
        assert_eq!(&split_groups, &[0..8, 8..11]);
    }

    #[test]
    #[should_panic(expected = "All splits are expected to be smaller than `max_merge_docs`.")]
    fn test_stable_multitenant_merge_policy_build_split_panics_if_exceeding_max_merge_docs() {
        let merge_policy = StableMultitenantWithTimestampMergePolicy::default();
        let splits = create_splits(vec![11_000_000]);
        merge_policy.build_split_levels(&splits);
    }

    #[test]
    fn test_stable_multitenant_merge_policy_not_enough_splits() {
        let merge_policy = StableMultitenantWithTimestampMergePolicy::default();
        let mut splits = create_splits(vec![100; 7]);
        assert_eq!(splits.len(), 7);
        assert!(merge_policy.operations(&mut splits).is_empty());
    }

    #[test]
    fn test_stable_multitenant_merge_policy_just_enough_splits_for_a_merge() {
        let merge_policy = StableMultitenantWithTimestampMergePolicy::default();
        let mut splits = create_splits(vec![100; 10]);
        let mut merge_ops = merge_policy.operations(&mut splits);
        assert!(splits.is_empty());
        assert_eq!(merge_ops.len(), 1);
        let merge_op = merge_ops.pop().unwrap();
        let mut merge_segment_ids: Vec<String> = merge_op
            .splits_as_slice()
            .iter()
            .map(|split| split.split_id().to_string())
            .collect();
        merge_segment_ids.sort();
        assert_eq!(
            merge_segment_ids,
            &[
                "split_00", "split_01", "split_02", "split_03", "split_04", "split_05", "split_06",
                "split_07", "split_08", "split_09"
            ]
        );
    }

    #[test]
    fn test_stable_multitenant_merge_policy_many_splits_on_same_level() {
        let merge_policy = StableMultitenantWithTimestampMergePolicy::default();
        let mut splits = create_splits(vec![100; 13]);
        let mut merge_ops = merge_policy.operations(&mut splits);
        assert_eq!(splits.len(), 1);
        assert_eq!(splits[0].split_id(), "split_00");
        assert_eq!(merge_ops.len(), 1);
        let merge_op = merge_ops.pop().unwrap();
        let mut merge_split_ids: Vec<String> = merge_op
            .splits_as_slice()
            .iter()
            .map(|split| split.split_id().to_string())
            .collect();
        merge_split_ids.sort();
        assert_eq!(
            merge_split_ids,
            &[
                "split_01", "split_02", "split_03", "split_04", "split_05", "split_06", "split_07",
                "split_08", "split_09", "split_10", "split_11", "split_12"
            ]
        );
    }

    #[test]
    fn test_stable_multitenant_merge_policy_splits_below_min_level() {
        let merge_policy = StableMultitenantWithTimestampMergePolicy::default();
        let mut splits = create_splits(vec![
            100, 1000, 10_000, 10_000, 10_000, 10_000, 10_000, 40_000, 40_000, 40_000,
        ]);
        let mut merge_ops = merge_policy.operations(&mut splits);
        assert_eq!(splits.len(), 0);
        assert_eq!(merge_ops.len(), 1);
        let merge_op = merge_ops.pop().unwrap();
        let mut merge_split_ids: Vec<String> = merge_op
            .splits_as_slice()
            .iter()
            .map(|split| split.split_id().to_string())
            .collect();
        merge_split_ids.sort();
        assert_eq!(
            merge_split_ids,
            &[
                "split_00", "split_01", "split_02", "split_03", "split_04", "split_05", "split_06",
                "split_07", "split_08", "split_09"
            ]
        );
    }

    #[test]
    fn test_stable_multitenant_merge_policy_splits_above_min_level() {
        let merge_policy = StableMultitenantWithTimestampMergePolicy::default();
        let mut splits = create_splits(vec![
            100_000, 1_000_000, 1_000_000, 1_000_000, 1_000_000, 1_000_000, 1_000_000, 1_000_000,
        ]);
        let merge_ops = merge_policy.operations(&mut splits);
        assert_eq!(splits.len(), 8);
        assert_eq!(merge_ops.len(), 0);
    }

    #[test]
    fn test_stable_multitenant_merge_policy_above_max_merge_docs_is_ignored() {
        let merge_policy = StableMultitenantWithTimestampMergePolicy::default();
        let mut splits = create_splits(vec![
            100_000, 100_000, 100_000, 100_000, 100_000,
            10_000_000, // this split should not interfere with the merging of other splits
            100_000, 100_000, 100_000, 100_000, 100_000,
        ]);
        let merge_ops = merge_policy.operations(&mut splits);
        assert_eq!(splits.len(), 1);
        assert_eq!(splits[0].num_docs, 10_000_000);
        assert_eq!(merge_ops.len(), 1);
    }

    #[test]
    fn test_merge_policy_splits_too_large_are_ignored() {
        let merge_policy = StableMultitenantWithTimestampMergePolicy::default();
        let mut splits = create_splits(vec![9_999_999, 10_000_000]);
        let merge_ops = merge_policy.operations(&mut splits);
        assert_eq!(splits.len(), 2);
        assert_eq!(splits[0].num_docs, 9_999_999);
        assert_eq!(splits[1].num_docs, 10_000_000);
        assert!(merge_ops.is_empty());
    }

    #[test]
    fn test_merge_policy_splits_entire_level_reach_merge_max_doc() {
        let merge_policy = StableMultitenantWithTimestampMergePolicy::default();
        let mut splits = create_splits(vec![5_000_000, 5_000_000]);
        let merge_ops = merge_policy.operations(&mut splits);
        assert!(splits.is_empty());
        assert_eq!(merge_ops.len(), 1);
        assert_eq!(merge_ops[0].splits_as_slice().len(), 2);
    }

    #[test]
    fn test_merge_policy_last_merge_can_have_a_lower_merge_factor() {
        let merge_policy = StableMultitenantWithTimestampMergePolicy::default();
        let mut splits = create_splits(vec![9_999_997, 9_999_998, 9_999_999]);
        let merge_ops = merge_policy.operations(&mut splits);
        assert_eq!(splits.len(), 1);
        assert_eq!(splits[0].num_docs, 9_999_997);
        assert_eq!(merge_ops.len(), 1);
        assert_eq!(merge_ops[0].splits_as_slice().len(), 2);
    }

    #[test]
    fn test_merge_policy_no_merge_with_only_one_split() {
        let merge_policy = StableMultitenantWithTimestampMergePolicy::default();
        let mut splits = create_splits(vec![9_999_999]);
        let merge_ops = merge_policy.operations(&mut splits);
        assert_eq!(splits.len(), 1);
        assert_eq!(splits[0].num_docs, 9_999_999);
        assert!(merge_ops.is_empty());
    }

    #[test]
    fn test_stable_multitenant_merge_policy_max_num_splits_worst_case() {
        let merge_policy = StableMultitenantWithTimestampMergePolicy::default();
        assert_eq!(merge_policy.max_num_splits_worst_case(99), 9);
        assert_eq!(merge_policy.max_num_splits_worst_case(1_000_000), 27);
        assert_eq!(merge_policy.max_num_splits_worst_case(2_000_000), 36);
        assert_eq!(merge_policy.max_num_splits_worst_case(3_000_000), 36);
        assert_eq!(merge_policy.max_num_splits_worst_case(4_000_000), 36);
        assert_eq!(merge_policy.max_num_splits_worst_case(5_000_000), 45);
        assert_eq!(merge_policy.max_num_splits_worst_case(7_000_000), 45);
        assert_eq!(merge_policy.max_num_splits_worst_case(10_000_000), 45);
        assert_eq!(merge_policy.max_num_splits_worst_case(20_000_000), 54);
        assert_eq!(merge_policy.max_num_splits_worst_case(100_000_000), 63);
        assert_eq!(merge_policy.max_num_splits_worst_case(1_000_000_000), 153);
    }

    #[test]
    fn test_stable_multitenant_merge_policy_max_num_splits_ideal_case() {
        let merge_policy = StableMultitenantWithTimestampMergePolicy::default();
        assert_eq!(merge_policy.max_num_splits_ideal_case(1_000_000), 18);
        assert_eq!(merge_policy.max_num_splits_ideal_case(99), 9);
        assert_eq!(merge_policy.max_num_splits_ideal_case(2_000_000), 20);
        assert_eq!(merge_policy.max_num_splits_ideal_case(3_000_000), 21);
        assert_eq!(merge_policy.max_num_splits_ideal_case(4_000_000), 22);
        assert_eq!(merge_policy.max_num_splits_ideal_case(5_000_000), 23);
        assert_eq!(merge_policy.max_num_splits_ideal_case(7_000_000), 25);
        assert_eq!(merge_policy.max_num_splits_ideal_case(10_000_000), 27);
        assert_eq!(merge_policy.max_num_splits_ideal_case(100_000_000), 37);
        assert_eq!(merge_policy.max_num_splits_ideal_case(1_000_000_000), 127);
    }

    #[test]
    fn test_stable_multitenant_merge_policy_merge_not_enabled() {
        let merge_policy = StableMultitenantWithTimestampMergePolicy {
            merge_enabled: false,
            ..Default::default()
        };
        let mut splits = create_splits(vec![100; 10]);
        let merge_ops = merge_policy.operations(&mut splits);
        assert_eq!(splits.len(), 10);
        assert_eq!(merge_ops.len(), 0);
    }
}
