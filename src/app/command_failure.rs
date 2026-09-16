use super::*;

#[derive(Clone)]
pub struct ShellTarget {
    pub ns: String,
    pub pod: String,
    pub container: Option<String>,
}

#[derive(Clone)]
pub struct CommandFailure {
    pub message: String,
    original_message: String,
    pub target: Option<ShellTarget>,
}

pub(crate) fn missing_shell(message: &str) -> bool {
    message.lines().any(|line| {
        let line = line.to_ascii_lowercase();
        (line.contains("exec: \"sh\"") || line.contains("exec: \"/bin/sh\""))
            && (line.contains("executable file not found")
                || line.contains("no such file or directory"))
    })
}

impl App {
    pub fn handle_command_result(
        &mut self,
        target: Option<ShellTarget>,
        result: std::io::Result<()>,
        recovery: Option<Box<CommandFailure>>,
    ) {
        match result {
            Ok(()) => {
                self.command_failure = None;
                self.flash = "Command completed.".into();
                self.flash_err = false;
            }
            Err(error) => {
                let message = crate::ui::strip_ansi_if_present(&error.to_string()).into_owned();
                let target = target.filter(|_| missing_shell(&message));
                let (message, original_message, target) = match recovery {
                    Some(previous) => (
                        format!(
                            "{}\n\nRecovery failed:\n{message}",
                            previous.original_message
                        ),
                        previous.original_message,
                        previous.target,
                    ),
                    None => (message.clone(), message, target),
                };
                self.command_failure = Some(CommandFailure {
                    message,
                    original_message,
                    target,
                });
                self.popup_scroll = 0;
                self.flash = "Command failed.".into();
                self.flash_err = true;
            }
        }
    }

    pub fn command_failure_visible(&self) -> bool {
        self.command_failure.is_some() && !matches!(self.mode, Mode::Prompt | Mode::Confirm)
    }

    pub(super) fn retain_recovery_error(&mut self) {
        if let Some(failure) = &mut self.command_failure {
            failure.message.push_str(&format!("\n\n{}", self.flash));
        }
    }

    pub(super) fn key_command_failure(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Enter => self.command_failure = None,
            KeyCode::PageUp | KeyCode::Up => {
                self.popup_scroll = self.popup_scroll.saturating_sub(self.popup_viewport.max(1));
            }
            KeyCode::PageDown | KeyCode::Down => {
                self.popup_scroll = self
                    .popup_scroll
                    .saturating_add(self.popup_viewport.max(1))
                    .min(self.popup_max_scroll);
            }
            KeyCode::Char('d') => {
                let target = self.command_failure.as_ref().and_then(|f| f.target.clone());
                if let Some(target) = target {
                    let recovery = self.command_failure.clone().map(Box::new);
                    self.request_debug_target(target.ns, target.pod, target.container, recovery);
                    if self.mode != Mode::Prompt {
                        self.retain_recovery_error();
                    }
                }
            }
            _ => {}
        }
    }
}
