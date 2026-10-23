//! Interactive or batch: whether a command may ask, and who asks.
//!
//! Every question goes through [`Prompter`], so commands never touch the
//! terminal themselves: the binary passes [`TerminalPrompter`], tests a
//! scripted one.

/// How a run interacts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// A terminal and no `--no-input`: missing arguments are asked for.
    Interactive,
    /// No terminal, or `--no-input`: a missing required argument ends the
    /// run with exit code 64 and the list of what is missing.
    Batch,
}

impl Mode {
    /// The mode of a run whose stdin is (`tty`) or is not a terminal.
    #[must_use]
    pub const fn detect(tty: bool, no_input: bool) -> Self {
        if tty && !no_input {
            Self::Interactive
        } else {
            Self::Batch
        }
    }
}

/// Why a question got no answer.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PromptError {
    /// The user gave up (Esc, Ctrl-C, Ctrl-D).
    #[error("cancelled")]
    Cancelled,
    /// The terminal could not be used.
    #[error("{0}")]
    Terminal(String),
}

/// Asks the questions of [`Mode::Interactive`].
///
/// # Errors
///
/// Every method fails with [`PromptError::Cancelled`] when the user gives
/// up and [`PromptError::Terminal`] when the terminal cannot be used.
#[expect(
    clippy::missing_errors_doc,
    reason = "the trait documents the errors of every method once"
)]
pub trait Prompter: Send {
    /// A line of text.
    fn text(&mut self, question: &str) -> Result<String, PromptError>;
    /// A line of text that is not echoed (tokens, secret values).
    fn password(&mut self, question: &str) -> Result<String, PromptError>;
    /// Yes or no.
    fn confirm(&mut self, question: &str) -> Result<bool, PromptError>;
    /// One of `options`, by index.
    fn select(&mut self, question: &str, options: &[String]) -> Result<usize, PromptError>;
}

/// The real terminal (`inquire`). Questions are drawn on stderr, so
/// stdout keeps only what a command prints.
#[derive(Debug, Clone, Copy, Default)]
pub struct TerminalPrompter;

/// Esc and Ctrl-C at a question are the user giving up.
fn prompt_error(error: inquire::InquireError) -> PromptError {
    match error {
        inquire::InquireError::OperationCanceled | inquire::InquireError::OperationInterrupted => {
            PromptError::Cancelled
        }
        other => PromptError::Terminal(other.to_string()),
    }
}

impl Prompter for TerminalPrompter {
    fn text(&mut self, question: &str) -> Result<String, PromptError> {
        inquire::Text::new(question).prompt().map_err(prompt_error)
    }

    fn password(&mut self, question: &str) -> Result<String, PromptError> {
        inquire::Password::new(question)
            .without_confirmation()
            .with_display_mode(inquire::PasswordDisplayMode::Masked)
            .prompt()
            .map_err(prompt_error)
    }

    fn confirm(&mut self, question: &str) -> Result<bool, PromptError> {
        inquire::Confirm::new(question)
            .with_default(false)
            .prompt()
            .map_err(prompt_error)
    }

    fn select(&mut self, question: &str, options: &[String]) -> Result<usize, PromptError> {
        inquire::Select::new(question, options.to_vec())
            .raw_prompt()
            .map(|choice| choice.index)
            .map_err(prompt_error)
    }
}
