//! Commands typed into the composer instead of sent to the agent.
//!
//! A leading `/` turns composer text into an app command. Everything the app
//! needs to know about a command lives in [`COMMANDS`]: the registry drives
//! autocomplete, validation, and dispatch alike, so a new command is one entry
//! here plus one arm where the app runs the resulting [`Command`].

use std::ops::Range;

/// Where a command can run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// Available wherever the composer is shown.
    #[allow(
        dead_code,
        reason = "the registry describes any command; every command so far acts on a session"
    )]
    Everywhere,
    /// Available only while a session is selected, because the command acts on
    /// that session.
    Session,
}

/// What a command expects after its name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Argument {
    /// The command takes nothing after its name.
    #[allow(
        dead_code,
        reason = "the registry describes any command; every command so far takes an argument"
    )]
    None,
    /// The rest of the line is the argument, described by the held hint.
    Required(&'static str),
}

/// What the app does when a command runs.
///
/// Kept separate from the command's name so dispatch stays exhaustive rather
/// than matching on strings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Line the argument up behind the session's current turn.
    QueueFollowUp,
}

/// One command the composer can run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SlashCommand {
    pub name: &'static str,
    pub summary: &'static str,
    pub argument: Argument,
    pub scope: Scope,
    pub kind: Kind,
}

impl SlashCommand {
    /// How the command reads in help text, such as `/next <instructions>`.
    #[must_use]
    pub fn usage(&self) -> String {
        match self.argument {
            Argument::None => format!("/{}", self.name),
            Argument::Required(hint) => format!("/{} <{hint}>", self.name),
        }
    }

    /// Text that replaces the typed name when the command is completed.
    ///
    /// A command that takes an argument leaves the caret past a space so the
    /// user can keep typing the argument straight away.
    #[must_use]
    pub fn completion(&self) -> String {
        match self.argument {
            Argument::None => format!("/{}", self.name),
            Argument::Required(_) => format!("/{} ", self.name),
        }
    }

    /// Whether the command can run right now.
    #[must_use]
    pub fn is_available(&self, session_selected: bool) -> bool {
        self.scope != Scope::Session || session_selected
    }

    /// Why the command cannot run right now, if it cannot.
    #[must_use]
    pub fn unavailable_reason(&self, session_selected: bool) -> Option<String> {
        (!self.is_available(session_selected))
            .then(|| format!("{} needs an open session.", self.usage()))
    }
}

/// Every command the composer understands.
pub const COMMANDS: &[SlashCommand] = &[SlashCommand {
    name: "next",
    summary: "Queue what the agent should do after the current turn",
    argument: Argument::Required("instructions"),
    scope: Scope::Session,
    kind: Kind::QueueFollowUp,
}];

/// A command that is ready to run, with its argument already taken apart.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    /// Queue `prompt` behind the selected session's current turn.
    QueueFollowUp(String),
}

/// What the composer should do with the text it was given.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolution {
    /// Not a command; send it to the agent as an ordinary prompt.
    Prompt,
    /// Run this command instead of sending a prompt.
    Run(Command),
    /// The text named a command that cannot run, with the reason to show.
    Rejected(String),
}

/// Look a command up by name, ignoring case.
#[must_use]
pub fn find(name: &str) -> Option<&'static SlashCommand> {
    COMMANDS
        .iter()
        .find(|command| command.name.eq_ignore_ascii_case(name))
}

/// Decide what `content` means.
#[must_use]
pub fn resolve(content: &str, session_selected: bool) -> Resolution {
    let Some((name, argument)) = split(content) else {
        return Resolution::Prompt;
    };
    if name.is_empty() {
        return Resolution::Rejected("Type a command name after /.".to_owned());
    }
    let Some(command) = find(name) else {
        return Resolution::Rejected(format!(
            "Unknown command /{name}. Type / to see the available commands."
        ));
    };
    if let Some(reason) = command.unavailable_reason(session_selected) {
        return Resolution::Rejected(reason);
    }
    if let Argument::Required(hint) = command.argument
        && argument.is_empty()
    {
        return Resolution::Rejected(format!(
            "/{} needs {hint}, as in {}.",
            command.name,
            command.usage()
        ));
    }
    Resolution::Run(match command.kind {
        Kind::QueueFollowUp => Command::QueueFollowUp(argument.to_owned()),
    })
}

/// The `/name` being typed at `cursor`, as a replacement range and the partial
/// name within it.
///
/// Returns nothing once the caret leaves the command name, because arguments
/// belong to the command rather than to this menu.
#[must_use]
pub fn completion_query(content: &str, cursor: usize) -> Option<(Range<usize>, &str)> {
    let rest = content.strip_prefix('/')?;
    let end = rest
        .find(char::is_whitespace)
        .map_or(content.len(), |index| index + 1);
    if cursor > end {
        return None;
    }
    let name = &content[1..end];
    (name.is_empty() || is_command_name(name)).then_some((0..end, name))
}

/// Commands worth offering for a partially typed `name`, best match first.
///
/// Commands that cannot run here are still listed, so the vocabulary stays
/// discoverable and the caller can explain what they need.
#[must_use]
pub fn matching(name: &str) -> Vec<&'static SlashCommand> {
    let name = name.to_ascii_lowercase();
    let mut matches: Vec<&'static SlashCommand> = COMMANDS
        .iter()
        .filter(|command| {
            command.name.contains(&name) || command.summary.to_ascii_lowercase().contains(&name)
        })
        .collect();
    matches.sort_by_key(|command| (!command.name.starts_with(&name), command.name));
    matches
}

/// Split `content` into a command name and the argument that follows it.
///
/// Only a first word shaped like a command name counts, so a pasted path such
/// as `/Users/me/notes.md` stays an ordinary prompt.
fn split(content: &str) -> Option<(&str, &str)> {
    let rest = content.trim_start().strip_prefix('/')?;
    let (name, argument) = rest
        .find(char::is_whitespace)
        .map_or((rest, ""), |index| (&rest[..index], rest[index..].trim()));
    (name.is_empty() || is_command_name(name)).then_some((name, argument))
}

fn is_command_name(word: &str) -> bool {
    word.starts_with(|character: char| character.is_ascii_alphabetic())
        && word
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-')
}

#[cfg(test)]
mod tests {
    use super::{Command, Resolution, completion_query, matching, resolve};

    #[test]
    fn plain_text_is_never_treated_as_a_command() {
        assert_eq!(resolve("ship the feature", true), Resolution::Prompt);
    }

    #[test]
    fn a_pasted_path_stays_a_prompt() {
        assert_eq!(
            resolve("/Users/me/notes.md needs a summary", true),
            Resolution::Prompt
        );
        assert_eq!(completion_query("/Users/me", 4), None);
    }

    #[test]
    fn next_queues_the_rest_of_the_line() {
        assert_eq!(
            resolve("/next run the tests", true),
            Resolution::Run(Command::QueueFollowUp("run the tests".to_owned()))
        );
    }

    #[test]
    fn next_is_rejected_without_an_argument_or_a_session() {
        let Resolution::Rejected(missing) = resolve("/next", true) else {
            panic!("an argumentless /next is rejected");
        };
        assert!(missing.contains("instructions"), "{missing}");

        let Resolution::Rejected(homeless) = resolve("/next do it", false) else {
            panic!("/next is rejected without a session");
        };
        assert!(homeless.contains("session"), "{homeless}");
    }

    #[test]
    fn an_unknown_command_names_itself() {
        let Resolution::Rejected(message) = resolve("/nxet do it", true) else {
            panic!("an unknown command is rejected");
        };
        assert!(message.contains("/nxet"), "{message}");
    }

    #[test]
    fn completion_covers_the_name_and_stops_at_the_argument() {
        assert_eq!(completion_query("/ne", 3), Some((0..3, "ne")));
        assert_eq!(completion_query("/", 1), Some((0..1, "")));
        assert_eq!(completion_query("/next later", 8), None);
        assert_eq!(completion_query("just talking", 4), None);
    }

    #[test]
    fn an_empty_name_offers_every_command() {
        assert_eq!(matching("").len(), super::COMMANDS.len());
        assert_eq!(
            matching("ne").first().map(|command| command.name),
            Some("next")
        );
        assert!(matching("zzz").is_empty());
    }
}
