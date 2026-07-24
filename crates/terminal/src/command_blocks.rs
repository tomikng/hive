//! Command-block model built from OSC 133 `SemanticPrompt` events.
//!
//! A shell emitting the OSC 133 "semantic prompt" protocol marks four points
//! in its output stream: `A` (prompt start), `B` (command start / prompt
//! end), `C` (command output start / command executed), and `D` (command
//! finished, carrying an optional exit code). [`CommandBlockTracker`] turns
//! that stream of marks, paired with the terminal's line position at the
//! moment each mark arrives, into a list of [`CommandBlock`]s.
//!
//! This module is intentionally terminal-agnostic: it only deals in `char`
//! kinds, `Option<i32>` exit codes, and caller-supplied line numbers, so the
//! state machine can be unit-tested without a live terminal/grid. The thin
//! wiring layer in `terminal.rs` is responsible for sourcing the current
//! line from the terminal and calling [`CommandBlockTracker::on_semantic_prompt`].
//!
//! ## Scrollback-anchoring limitation
//!
//! Line numbers recorded here are the terminal's grid line coordinate *at
//! the moment the event arrived* - they are not stable identifiers. As the
//! terminal scrolls (new output pushes old rows into scrollback, or the grid
//! wraps/resizes), a block's recorded `prompt_line`/`command_line`/etc. do
//! not move with the content: a block recorded at line 40 stays "40" even
//! after everything has scrolled up by 500 lines, so old blocks can end up
//! pointing at stale or already-recycled rows. Giving every block a stable
//! identity that survives scrolling/history eviction is deferred to a later
//! phase; for now this tracker is honest about only being accurate for
//! blocks still within/near the live viewport.

/// A single shell command's lifecycle, as reconstructed from OSC 133 marks.
///
/// Line numbers are in the terminal's grid-line coordinate space as of the
/// moment each mark was observed - see the module docs for why these are not
/// stable across scrolling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommandBlock {
    /// Line at which the shell printed its prompt (OSC 133;A).
    pub prompt_line: usize,
    /// Line at which the user's command starts / the prompt ends (OSC 133;B).
    /// Defaults to `prompt_line` until a `B` mark is observed.
    pub command_line: usize,
    /// Line at which the command's output starts (OSC 133;C). Defaults to
    /// `prompt_line` until a `C` mark is observed.
    pub output_start_line: usize,
    /// Line at which the command finished (OSC 133;D). `None` while the
    /// block is still open.
    pub end_line: Option<usize>,
    /// Exit code reported by `D`, if any. `None` while open, and also `None`
    /// if the shell sent a bare `D` with no exit code.
    pub exit_code: Option<i32>,
}

impl CommandBlock {
    fn opened_at(line: usize) -> Self {
        Self {
            prompt_line: line,
            command_line: line,
            output_start_line: line,
            end_line: None,
            exit_code: None,
        }
    }

    fn is_open(&self) -> bool {
        self.end_line.is_none()
    }
}

/// Consumes OSC 133 `SemanticPrompt` marks and produces an append-only list
/// of [`CommandBlock`]s.
#[derive(Debug, Default)]
pub struct CommandBlockTracker {
    blocks: Vec<CommandBlock>,
}

impl CommandBlockTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feeds one OSC 133 mark into the state machine.
    ///
    /// `kind` is the OSC 133 mark character (`'A'`, `'B'`, `'C'`, `'D'`);
    /// unrecognized kinds are ignored. `current_line` is the terminal's line
    /// position at the moment the mark was observed. `exit_code` is only
    /// meaningful for `'D'`.
    ///
    /// Out-of-order or missing marks degrade gracefully: `'B'`/`'C'`/`'D'`
    /// with no open block are no-ops, and a second `'A'` before a `'D'`
    /// finalizes (closes, with no exit code) the previously dangling block
    /// before opening the next one.
    pub fn on_semantic_prompt(&mut self, kind: char, exit_code: Option<i32>, current_line: usize) {
        match kind {
            'A' => {
                if let Some(dangling_block) = self.open_block_mut() {
                    dangling_block.end_line = Some(current_line);
                }
                self.blocks.push(CommandBlock::opened_at(current_line));
            }
            'B' => {
                if let Some(open_block) = self.open_block_mut() {
                    open_block.command_line = current_line;
                }
            }
            'C' => {
                if let Some(open_block) = self.open_block_mut() {
                    open_block.output_start_line = current_line;
                }
            }
            'D' => {
                if let Some(open_block) = self.open_block_mut() {
                    open_block.end_line = Some(current_line);
                    open_block.exit_code = exit_code;
                }
            }
            _ => {}
        }
    }

    /// All blocks observed so far, oldest first. The last entry may still be
    /// open (`end_line: None`) if its `D` mark hasn't arrived yet.
    pub fn blocks(&self) -> &[CommandBlock] {
        &self.blocks
    }

    fn open_block_mut(&mut self) -> Option<&mut CommandBlock> {
        self.blocks.last_mut().filter(|block| block.is_open())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_opens_a_new_block_at_the_current_line() {
        let mut tracker = CommandBlockTracker::new();

        tracker.on_semantic_prompt('A', None, 10);

        assert_eq!(
            tracker.blocks(),
            &[CommandBlock {
                prompt_line: 10,
                command_line: 10,
                output_start_line: 10,
                end_line: None,
                exit_code: None,
            }]
        );
    }

    #[test]
    fn b_sets_command_line_on_the_open_block() {
        let mut tracker = CommandBlockTracker::new();

        tracker.on_semantic_prompt('A', None, 10);
        tracker.on_semantic_prompt('B', None, 11);

        assert_eq!(tracker.blocks().len(), 1);
        assert_eq!(tracker.blocks()[0].command_line, 11);
    }

    #[test]
    fn c_sets_output_start_line_on_the_open_block() {
        let mut tracker = CommandBlockTracker::new();

        tracker.on_semantic_prompt('A', None, 10);
        tracker.on_semantic_prompt('B', None, 11);
        tracker.on_semantic_prompt('C', None, 12);

        assert_eq!(tracker.blocks().len(), 1);
        assert_eq!(tracker.blocks()[0].output_start_line, 12);
    }

    #[test]
    fn d_closes_the_block_with_its_exit_code() {
        let mut tracker = CommandBlockTracker::new();

        tracker.on_semantic_prompt('A', None, 10);
        tracker.on_semantic_prompt('B', None, 11);
        tracker.on_semantic_prompt('C', None, 12);
        tracker.on_semantic_prompt('D', Some(0), 15);

        assert_eq!(
            tracker.blocks(),
            &[CommandBlock {
                prompt_line: 10,
                command_line: 11,
                output_start_line: 12,
                end_line: Some(15),
                exit_code: Some(0),
            }]
        );
    }

    #[test]
    fn full_sequence_then_a_new_a_opens_a_fresh_block() {
        let mut tracker = CommandBlockTracker::new();

        tracker.on_semantic_prompt('A', None, 0);
        tracker.on_semantic_prompt('B', None, 1);
        tracker.on_semantic_prompt('C', None, 2);
        tracker.on_semantic_prompt('D', Some(0), 5);
        tracker.on_semantic_prompt('A', None, 6);

        assert_eq!(tracker.blocks().len(), 2);
        assert_eq!(tracker.blocks()[0].end_line, Some(5));
        assert_eq!(tracker.blocks()[1].prompt_line, 6);
        assert!(tracker.blocks()[1].is_open());
    }

    #[test]
    fn second_a_before_d_finalizes_the_dangling_block() {
        let mut tracker = CommandBlockTracker::new();

        tracker.on_semantic_prompt('A', None, 0);
        tracker.on_semantic_prompt('B', None, 1);
        // No 'C' or 'D' - the shell got interrupted, or the parser missed a mark.
        tracker.on_semantic_prompt('A', None, 20);

        assert_eq!(tracker.blocks().len(), 2);
        let first = tracker.blocks()[0];
        assert_eq!(first.end_line, Some(20));
        assert_eq!(first.exit_code, None);
        assert_eq!(tracker.blocks()[1].prompt_line, 20);
    }

    #[test]
    fn d_with_no_open_block_is_ignored_not_a_panic() {
        let mut tracker = CommandBlockTracker::new();

        tracker.on_semantic_prompt('D', Some(1), 3);

        assert!(tracker.blocks().is_empty());
    }

    #[test]
    fn b_with_no_open_block_is_ignored() {
        let mut tracker = CommandBlockTracker::new();

        tracker.on_semantic_prompt('B', None, 3);

        assert!(tracker.blocks().is_empty());
    }

    #[test]
    fn c_with_no_open_block_is_ignored() {
        let mut tracker = CommandBlockTracker::new();

        tracker.on_semantic_prompt('C', None, 3);

        assert!(tracker.blocks().is_empty());
    }

    #[test]
    fn d_after_a_block_is_already_closed_is_ignored() {
        let mut tracker = CommandBlockTracker::new();

        tracker.on_semantic_prompt('A', None, 0);
        tracker.on_semantic_prompt('D', Some(0), 5);
        // A stray second D for the same (already closed) block.
        tracker.on_semantic_prompt('D', Some(99), 6);

        assert_eq!(tracker.blocks().len(), 1);
        assert_eq!(tracker.blocks()[0].exit_code, Some(0));
        assert_eq!(tracker.blocks()[0].end_line, Some(5));
    }

    #[test]
    fn exit_code_passes_through_success_failure_and_bare_d() {
        let mut tracker = CommandBlockTracker::new();

        tracker.on_semantic_prompt('A', None, 0);
        tracker.on_semantic_prompt('D', Some(0), 1);
        assert_eq!(tracker.blocks()[0].exit_code, Some(0));

        tracker.on_semantic_prompt('A', None, 2);
        tracker.on_semantic_prompt('D', Some(130), 3);
        assert_eq!(tracker.blocks()[1].exit_code, Some(130));

        // A bare D (no exit code payload) should not be conflated with 0.
        tracker.on_semantic_prompt('A', None, 4);
        tracker.on_semantic_prompt('D', None, 5);
        assert_eq!(tracker.blocks()[2].exit_code, None);
        assert_eq!(tracker.blocks()[2].end_line, Some(5));
    }

    #[test]
    fn unknown_kind_is_ignored() {
        let mut tracker = CommandBlockTracker::new();

        tracker.on_semantic_prompt('A', None, 0);
        tracker.on_semantic_prompt('Z', None, 1);

        assert_eq!(tracker.blocks().len(), 1);
        assert_eq!(tracker.blocks()[0].command_line, 0);
    }
}
