//! Answering a command that asks you something.
//!
//! `docs/interactive-commands.md` describes the mechanism; this is the window's
//! half of it, and it is the one gate this surface claims.
//!
//! # Why this exists at all
//!
//! Phase 1 declared no console access, "exactly as the browser GUI does not
//! claim one". That was honest about the browser and wrong as a destination.
//! The browser GUI *could not* claim a console: its user was behind a socket
//! that could drop mid-command, and a page holding the stdin of a live child
//! process in another process is a hung `apt install` waiting on a tab somebody
//! closed. That limitation was a property of the boundary, not of graphical
//! surfaces — and it was the single most-cited reason that surface degraded
//! against the TUI, because "wizard can't run anything that prompts" is a whole
//! class of task.
//!
//! This window has no boundary. It is the same process as the agent, it dies
//! when the agent dies, and it has a person in front of it. That is exactly the
//! condition `ConsoleAccess::Interactive` describes, and it is the same
//! condition the TUI meets.
//!
//! # The gate rule, restated because this is the exception
//!
//! [`crate::plugins::native`]'s header says the window never calls `claim()`. Plan and
//! interview gates are claimed by [`TaskShared::handle_event`], which parks the
//! reply channel so that a disconnect can resolve it; claiming one here would
//! take the channel out of that bookkeeping.
//!
//! The console gate is the opposite case, and the asymmetry is in
//! `TaskShared::handle_event` itself: it matches `ConsoleOpened` and does
//! **nothing** with it. Nobody else claims a console for a GUI task, so if this
//! window does not, no one does, and `ConsoleHost::attended` stays false — the
//! command keeps its timeout clock running and dies at it, which is the
//! unattended behaviour. Claiming is therefore not stealing the ticket from the
//! bookkeeping; it is the only thing that stops a prompting command from timing
//! out.
//!
//! # One console at a time
//!
//! Tool dispatch is sequential within a turn, so at most one foreground command
//! is ever prompting. The state is a single `Option` rather than a map, and a
//! second `ConsoleOpened` while one is open replaces it — which cannot happen
//! today and, if it ever did, leaving the *older* console bound would be the
//! wrong of the two answers.

use crate::agent::{ConsoleGate, ConsoleWriter};

/// A running command this window is driving.
pub struct Console {
    /// The ticket, so [`AgentEvent::ConsoleClosed`](crate::agent::AgentEvent)
    /// for some *other* console cannot close this one.
    gate: ConsoleGate,
    /// The command line, for the prompt label above the composer.
    pub command: String,
    writer: ConsoleWriter,
}

impl Console {
    /// Claim `gate` and bind the composer to `command`'s stdin.
    ///
    /// `None` when the ticket has already been claimed, when it came off a wire
    /// (a peer's session: `crate::plugins::mesh::turn` voids it to ticket 0 precisely so
    /// that watching a peer never becomes typing into a peer's shell), or when
    /// the command ended between the announcement and this call.
    pub fn claim(gate: ConsoleGate, command: String) -> Option<Self> {
        let writer = gate.claim()?;
        Some(Self {
            gate,
            command,
            writer,
        })
    }

    /// Whether `gate` is the console this one is driving.
    pub fn is(&self, gate: ConsoleGate) -> bool {
        self.gate == gate
    }

    /// Send one line to the child, terminator included.
    ///
    /// `false` when the command has ended or its queue is full. Both are things
    /// to say out loud rather than to block a draw loop over, which is why
    /// [`ConsoleWriter::line`] is not `async` and neither is this.
    pub fn line(&self, text: &str) -> bool {
        self.writer.line(text)
    }

    /// Close the child's stdin: a terminal's Ctrl-D, for the program reading a
    /// list of lines that has no natural end.
    pub fn eof(&self) -> bool {
        self.writer.eof()
    }
}

impl std::fmt::Debug for Console {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Console")
            .field("gate", &self.gate)
            .field("command", &self.command)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::ConsoleInput;

    /// The whole point: a claim yields a writer, and what is written arrives at
    /// the command's end of the channel. Without this the composer could look
    /// bound and type into nothing.
    #[test]
    fn a_claimed_console_carries_a_typed_line_to_the_command() {
        let (gate, mut host) = ConsoleGate::open();
        let console = Console::claim(gate, "npm init".to_string()).expect("the first claim wins");
        assert!(host.attended(), "the command knows somebody is there");
        assert!(console.line("wizard"));
        assert_eq!(
            host.receive.try_recv(),
            Ok(ConsoleInput::Line("wizard".to_string()))
        );
        assert!(console.eof());
        assert_eq!(host.receive.try_recv(), Ok(ConsoleInput::Eof));
    }

    /// Exactly once, like every other gate. A second window (or a replayed
    /// stream) must not get a second author of what the child reads.
    #[test]
    fn a_console_can_only_be_claimed_once() {
        let (gate, _host) = ConsoleGate::open();
        let _first = Console::claim(gate, "apt install".to_string()).expect("the first claim wins");
        assert!(Console::claim(gate, "apt install".to_string()).is_none());
    }

    /// A close for another command must not unbind this one. Consoles do not
    /// overlap today, but the identity check is what makes that a fact about
    /// the code rather than about the current dispatch order.
    #[test]
    fn a_console_only_answers_to_its_own_ticket() {
        let (mine, _host) = ConsoleGate::open();
        let (other, _other_host) = ConsoleGate::open();
        let console = Console::claim(mine, "npm init".to_string()).expect("claim");
        assert!(console.is(mine));
        assert!(!console.is(other));
    }
}
