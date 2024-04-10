pub(crate) mod expression_state;
pub(crate) mod repl_state;

use anyhow::bail;

use crate::{
    example_id::ExampleId,
    examples::Example,
    expression::driver::{EvaluateExpression, ExpressionEvent},
    repl::driver::{ReplCommand, ReplEvent, ReplQuery},
};

use self::{
    expression_state::ExpressionExampleState,
    repl_state::{ReplExampleState, ReplSessionExpecting, ReplSessionLive, ReplSessionState},
};

use super::{InputEvent, OutputEvent};

#[derive(Default, Debug)]
pub(super) struct State {
    examples: ExamplesState,
    pending_eprintlns: usize,
    error: Option<anyhow::Error>,
}

impl State {
    pub(super) fn event(&mut self, event: InputEvent) -> Vec<OutputEvent> {
        let output = match event {
            InputEvent::Example(example) => self.example(example),
            InputEvent::ReplEvent(repl_event) => self.repl_event(repl_event),
            InputEvent::ExpressionEvent(expression_event) => {
                self.expression_event(expression_event)
            }
            InputEvent::Eprintlned => self.eprintlned(),
        };

        let output = match output {
            Ok(output) => output,
            Err(error) => {
                self.error = Some(error);
                vec![]
            }
        };

        if let (Some(error), 0) = (&self.error, self.pending_eprintlns) {
            return vec![OutputEvent::Done(Err(anyhow::anyhow!("{error}")))];
        }

        if self.examples.is_empty() && self.pending_eprintlns == 0 {
            return vec![OutputEvent::Done(Ok(()))];
        }

        output
    }

    pub(super) fn example(&mut self, example: Example) -> anyhow::Result<Vec<OutputEvent>> {
        let (id, example_state, event) = match example {
            Example::Repl(example) => {
                let example_id = example.id.clone();
                let example_state = ExampleState::Repl(ReplExampleState::new(example));
                let event = OutputEvent::ReplCommand(ReplCommand::Spawn(example_id.clone()));
                (example_id, example_state, event)
            }
            Example::Expression(example) => {
                let example_id = example.id.clone();
                let example_state = ExampleState::Expression(ExpressionExampleState::Pending);
                let event = OutputEvent::ExpressionCommand(EvaluateExpression(example));
                (example_id, example_state, event)
            }
        };

        self.examples.insert(id.clone(), example_state)?;

        Ok(vec![event])
    }

    pub(super) fn repl_event(&mut self, repl_event: ReplEvent) -> anyhow::Result<Vec<OutputEvent>> {
        match repl_event {
            ReplEvent::Spawn(spawn) => self.repl_event_spawn(spawn),
            ReplEvent::Query(id, query, result) => self.repl_event_query(id, query, result),
            ReplEvent::Kill(id) => self.repl_event_kill(id),
            ReplEvent::Read(id, result) => self.repl_event_read(id, result),
        }
    }

    fn repl_event_spawn(
        &mut self,
        spawn: Result<ExampleId, pty_process::Error>,
    ) -> anyhow::Result<Vec<OutputEvent>> {
        let id = spawn?;

        let session = self.examples.get_mut_repl(&id)?;

        if let ReplSessionState::Live(_) = &session.state {
            return Err(anyhow::anyhow!("spawned session {session:?} already live"));
        }

        let session_live = ReplSessionLive::new(session.example.entries.clone());
        session.state = ReplSessionState::Live(session_live);
        Ok(vec![])
    }

    fn repl_event_query(
        &self,
        _id: ExampleId,
        _query: ReplQuery,
        result: anyhow::Result<()>,
    ) -> anyhow::Result<Vec<OutputEvent>> {
        result?;
        // TODO possibly store this fact
        Ok(vec![])
    }

    fn repl_event_kill(
        &mut self,
        result: anyhow::Result<ExampleId>,
    ) -> anyhow::Result<Vec<OutputEvent>> {
        let id = result?;
        self.examples.remove(&id)?;
        Ok(Vec::new())
    }

    fn repl_event_read(
        &mut self,
        id: ExampleId,
        result: std::io::Result<u8>,
    ) -> anyhow::Result<Vec<OutputEvent>> {
        let session_live = self.examples.get_mut_repl(&id)?;
        let session_live = session_live.state.live_mut()?;
        let ch = result?;

        let output = match &mut session_live.expecting {
            ReplSessionExpecting::Nothing => anyhow::bail!("not expecting, got {:?}", ch as char),
            ReplSessionExpecting::Prompt(acc) => {
                acc.push(ch.into());
                let string = String::from_utf8(strip_ansi_escapes::strip(acc)?)?;

                if string.ends_with("nix-repl> ") {
                    session_live.expecting = ReplSessionExpecting::Nothing;
                    self.next_query(&id)?
                } else {
                    vec![]
                }
            }
            ReplSessionExpecting::Echo {
                acc,
                last_query: expected,
                expected_result,
            } => {
                acc.push(ch.into());
                if !acc.ends_with('\n') {
                    vec![]
                } else if Self::sanitize(acc)? == expected.as_str() {
                    session_live.expecting = if let Some(expected_result) = expected_result {
                        ReplSessionExpecting::Result {
                            acc: String::new(),
                            expected_result: expected_result.clone(),
                        }
                    } else {
                        ReplSessionExpecting::Prompt(String::new())
                    };
                    vec![]
                } else {
                    anyhow::bail!("actual: {acc:?}, expected: {expected:?}");
                }
            }
            ReplSessionExpecting::Result {
                acc,
                expected_result,
            } => 'arm: {
                acc.push(ch.into());

                let Some(stripped_crlf_twice) = acc.strip_suffix("\r\n\r\n") else {
                    break 'arm vec![];
                };

                let sanitized = Self::sanitize(stripped_crlf_twice)?;

                if sanitized != expected_result.as_str() {
                    anyhow::bail!(indoc::formatdoc! {"
                        {id}
                        actual (sanitized): {sanitized}
                        expected          : {expected_result}"
                    })
                }

                session_live.expecting = ReplSessionExpecting::Prompt(String::new());
                vec![]
            }
        };

        Ok(output)
    }

    fn next_query(&mut self, id: &ExampleId) -> anyhow::Result<Vec<OutputEvent>> {
        let session = self.examples.get_mut_repl(id)?;

        let ReplSessionState::Live(session_live) = &mut session.state else {
            anyhow::bail!("expected session {id} to be live");
        };

        let Some(entry) = session_live.next() else {
            return self.session_end(id);
        };

        session_live.expecting = ReplSessionExpecting::Echo {
            acc: String::new(),
            last_query: entry.query.clone(),
            expected_result: entry.expected_result,
        };

        Ok(vec![OutputEvent::ReplCommand(ReplCommand::Query(
            id.clone(),
            entry.query.clone(),
        ))])
    }

    fn session_end(&mut self, id: &ExampleId) -> anyhow::Result<Vec<OutputEvent>> {
        let session = self.examples.get_mut_repl(id)?;
        session.state = ReplSessionState::Killing;
        Ok(vec![
            OutputEvent::ReplCommand(ReplCommand::Kill(id.clone())),
            self.eprintln(Self::fmt_pass(id)),
        ])
    }

    fn fmt_pass(id: &ExampleId) -> String {
        format!("PASS: {id}")
    }

    fn eprintln(&mut self, line: String) -> OutputEvent {
        self.pending_eprintlns += 1;
        OutputEvent::Eprintln(line)
    }

    fn sanitize(s: &str) -> anyhow::Result<String> {
        let ansi_stripped = strip_ansi_escapes::strip(s)?;
        let string = String::from_utf8(ansi_stripped)?
            .chars()
            .filter(|ch| ch != &'\r')
            .collect();
        Ok(string)
    }

    pub(crate) fn expression_event_output(
        &mut self,
        expression_output: std::io::Result<(ExampleId, std::process::Output)>,
    ) -> anyhow::Result<Vec<OutputEvent>> {
        let (example_id, expression_output) = expression_output?;

        if !expression_output.status.success() {
            let stderr = String::from_utf8_lossy(&expression_output.stderr);
            bail!("{example_id}\n{stderr}")
        }

        self.examples.remove(&example_id)?;

        Ok(vec![self.eprintln(Self::fmt_pass(&example_id))])
    }

    pub(crate) fn expression_event(
        &mut self,
        expression_event: ExpressionEvent,
    ) -> Result<Vec<OutputEvent>, anyhow::Error> {
        match expression_event {
            ExpressionEvent::Spawn(result) => self.expression_event_spawn(result),
            ExpressionEvent::Output(result) => self.expression_event_output(result),
        }
    }

    fn expression_event_spawn(
        &mut self,
        result: Result<ExampleId, std::io::Error>,
    ) -> anyhow::Result<Vec<OutputEvent>> {
        let example_id = result?;
        let example_state = self.examples.get_mut_expression(&example_id)?;
        *example_state = ExpressionExampleState::Spawned;
        Ok(vec![])
    }

    pub(crate) fn eprintlned(&mut self) -> Result<Vec<OutputEvent>, anyhow::Error> {
        self.pending_eprintlns -= 1;
        Ok(Vec::new())
    }
}

#[derive(Debug, Default)]
pub(crate) struct ExamplesState(std::collections::BTreeMap<ExampleId, ExampleState>);

impl ExamplesState {
    pub(crate) fn insert(&mut self, id: ExampleId, state: ExampleState) -> anyhow::Result<()> {
        if self.0.insert(id.clone(), state).is_some() {
            anyhow::bail!("duplicate session id {id:?}");
        };
        Ok(())
    }

    pub(crate) fn get_mut(&mut self, id: &ExampleId) -> anyhow::Result<&mut ExampleState> {
        self.0
            .get_mut(id)
            .ok_or_else(|| anyhow::anyhow!("repl session not found {id:?}"))
    }

    pub(crate) fn remove(&mut self, id: &ExampleId) -> anyhow::Result<ExampleState> {
        self.0
            .remove(id)
            .ok_or_else(|| anyhow::anyhow!("repl session not found {id:?}"))
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    fn get_mut_repl(&mut self, id: &ExampleId) -> anyhow::Result<&mut ReplExampleState> {
        let example_state = self.get_mut(id)?;
        let ExampleState::Repl(repl_example_state) = example_state else {
            anyhow::bail!("expected repl example state");
        };
        Ok(repl_example_state)
    }

    fn get_mut_expression(
        &mut self,
        id: &ExampleId,
    ) -> anyhow::Result<&mut ExpressionExampleState> {
        let example_state = self.get_mut(id)?;
        let ExampleState::Expression(expression_example_state) = example_state else {
            anyhow::bail!("expected expression example state");
        };
        Ok(expression_example_state)
    }
}

#[derive(Debug)]
pub(crate) enum ExampleState {
    Repl(ReplExampleState),
    Expression(ExpressionExampleState),
}
