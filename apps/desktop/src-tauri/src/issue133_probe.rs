//! THROWAWAY -- #133's failing-direction probe. Do not merge.
//!
//! The whole point of #133 is that the PR gate had only ever been observed
//! PASSING. This module exists on a throwaway branch to make it fail on a PR
//! that touches both a Rust path and a docs path, so the required context
//! `Rust build + lib tests` can be observed resolving to FAILED and blocking
//! the merge.

#[cfg(test)]
mod tests {
    #[test]
    fn issue133_failing_direction_probe() {
        assert_eq!(
            1, 2,
            "#133 deliberate failure: proving the Rust PR gate blocks a mixed PR"
        );
    }
}
