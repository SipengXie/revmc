//! Branch profile data for PGO-guided compilation.

use std::collections::HashMap;

/// Branch profile for a single bytecode (keyed by bytecode PC).
/// Each entry records how many times a JUMPI was taken vs not-taken.
#[derive(Clone, Debug, Default)]
pub struct BranchProfile {
    /// Map from bytecode PC → (taken_count, not_taken_count).
    pub branches: HashMap<u32, (u64, u64)>,
}

impl BranchProfile {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a branch outcome at the given PC.
    pub fn record(&mut self, pc: u32, taken: bool) {
        let entry = self.branches.entry(pc).or_insert((0, 0));
        if taken {
            entry.0 += 1;
        } else {
            entry.1 += 1;
        }
    }

    /// Returns true if the taken direction is cold (< 20% of total).
    /// Returns None if no profile data for this PC.
    pub fn is_taken_cold(&self, pc: u32) -> Option<bool> {
        self.branches.get(&pc).map(|&(taken, not_taken)| {
            let total = taken + not_taken;
            if total == 0 {
                return false;
            }
            // "cold" = less than 20% of total executions
            taken * 5 < total
        })
    }

    /// Returns true if the not-taken direction is cold (< 20% of total).
    pub fn is_not_taken_cold(&self, pc: u32) -> Option<bool> {
        self.branches.get(&pc).map(|&(taken, not_taken)| {
            let total = taken + not_taken;
            if total == 0 {
                return false;
            }
            not_taken * 5 < total
        })
    }
}
