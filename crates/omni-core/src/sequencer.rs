//! The sequencer rule: last-write-wins, but never silently.
//!
//! The server is the only authority on ordering. A client submits a version
//! together with the active version it had seen (`base_version_id`). Comparing
//! the two is the whole correctness story.

/// What the server should do with a submitted version.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// No version existed at this path: create it.
    Create,
    /// The sender saw the active version: a plain linear advance.
    Linear,
    /// The sender's base is not the active version, so another device changed
    /// this path since the sender last looked. Last write wins, but the
    /// superseded version is recorded so the dashboard can offer a rollback.
    Overwrite { losing_version_id: Option<i64> },
}

/// The stateless sequencer. Kept as a unit type with associated functions
/// because the rule depends on nothing but its arguments.
pub struct Sequencer;

impl Sequencer {
    /// Decide what a submitted version means given the currently active one.
    ///
    /// `active_version_id` is `None` when the path does not exist (or is a
    /// tombstone). `base_version_id` is what the sender believed was active;
    /// `None` means the sender thought the path was new.
    pub fn decide(active_version_id: Option<i64>, base_version_id: Option<i64>) -> Decision {
        match (active_version_id, base_version_id) {
            // Nothing active: the sender is creating the path, whatever it
            // thought. (A `base` for a path that does not exist is just stale.)
            (None, _) => Decision::Create,
            // The sender saw exactly what is active.
            (Some(active), Some(base)) if base == active => Decision::Linear,
            // Anything else is a divergence: the sender's view is stale.
            (Some(active), _) => Decision::Overwrite {
                losing_version_id: Some(active),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_path_is_a_create() {
        assert_eq!(Sequencer::decide(None, None), Decision::Create);
        // A stale base for a path that does not exist is still a create.
        assert_eq!(Sequencer::decide(None, Some(7)), Decision::Create);
    }

    #[test]
    fn matching_base_is_linear() {
        assert_eq!(Sequencer::decide(Some(4), Some(4)), Decision::Linear);
    }

    #[test]
    fn a_stale_base_overwrites_and_names_the_loser() {
        assert_eq!(
            Sequencer::decide(Some(4), Some(3)),
            Decision::Overwrite {
                losing_version_id: Some(4)
            }
        );
    }

    #[test]
    fn an_unseen_existing_path_overwrites() {
        // The sender thought the file was new, but it exists.
        assert_eq!(
            Sequencer::decide(Some(9), None),
            Decision::Overwrite {
                losing_version_id: Some(9)
            }
        );
    }
}
