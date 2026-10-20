use super::*;

impl HeadlessServer {
    pub(super) fn cancel_shell_command_for_pane(&self, pane_id: crate::layout::PaneId) {
        for ws in &self.app.state.workspaces {
            if let Some(pane) = ws.pane_state(pane_id) {
                if let Some(runtime) = self.app.terminal_runtimes.get(&pane.attached_terminal_id) {
                    runtime.cancel_shell_command();
                }
                break;
            }
        }
    }

    pub(super) fn forward_shell_command_completed(
        &mut self,
        pane_id: crate::layout::PaneId,
        command: &crate::pane::shell_command::ShellCommandCompletion,
    ) {
        let identity = self
            .app
            .state
            .workspaces
            .iter()
            .enumerate()
            .find_map(|(ws_idx, ws)| {
                let pane = ws.pane_state(pane_id)?;
                let runtime = self.app.terminal_runtimes.get(&pane.attached_terminal_id)?;
                let terminal = self.app.state.terminals.get(&pane.attached_terminal_id)?;
                if !shell_command_identity_matches(
                    runtime.shell_command_generation(),
                    command.generation,
                ) || terminal.effective_agent_label().is_some()
                {
                    return None;
                }
                Some((
                    self.app.public_pane_id(ws_idx, pane_id)?,
                    pane.attached_terminal_id.to_string(),
                ))
            });
        let Some((pane_id, terminal_id)) = identity else {
            return;
        };
        let Some(sequence) = self.shell_command_sequence.checked_add(1) else {
            return;
        };
        self.shell_command_sequence = sequence;
        let event = protocol::endpoint::EndpointShellCommandCompleted {
            boot_id: self.client_shell_boot_id.clone(),
            terminal_id,
            pane_id,
            sequence,
            exit_code: command.exit_code,
            duration_ms: command.duration_ms,
            output: command.output.clone(),
        };
        if let Ok(message) = protocol::endpoint::shell_command_completed_message(&event) {
            self.send_to_client_shells(message);
        }
    }
}

fn shell_command_identity_matches(current_generation: Option<u64>, event_generation: u64) -> bool {
    current_generation == Some(event_generation)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shell_command_rebound_or_closed_runtime_rejects_old_event() {
        assert!(shell_command_identity_matches(Some(12), 12));
        assert!(!shell_command_identity_matches(Some(13), 12));
        assert!(!shell_command_identity_matches(None, 12));
    }
}
