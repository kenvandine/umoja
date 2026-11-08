//! Read-only troubleshooting tools for the assistant.
//!
//! The collectors live in [`umoja_common::diagnostics`]; this module
//! registers them as assistant tools and, for questions that look like
//! troubleshooting ("why is my fan loud?", "Wi-Fi won't connect"), gathers
//! a report up front and adds it to the question's context. Small local
//! models often answer from general knowledge instead of calling a tool,
//! so grounding the first turn makes the answer cite this computer's
//! actual readings. The model can still call the tools for more detail
//! (a unit's status, a journal search).

use super::agent::{Question, StepState, Ui};
use super::tools::ToolSpec;
use crate::lemonade::RUNTIME;
use umoja_common::{diagnostics, i18n};

/// Every diagnostics collector as a read-only (never confirmed) tool.
pub fn tools() -> Vec<ToolSpec> {
    diagnostics::tools()
        .into_iter()
        .map(|t| {
            ToolSpec::read_only(t.name, t.description, (t.parameters)(), t.run)
                .with_summary(t.summary)
        })
        .collect()
}

/// How the report is introduced to the model.
const PREAMBLE: &str = "The user is troubleshooting this computer. Below is a read-only \
report gathered just now from it (sensors, /proc, systemd, the journal, NetworkManager). \
Answer from it: name the most likely cause and quote the evidence (a process and its CPU %, a \
temperature, a log line). CPU % is processor load; memory use does not make a fan spin. If \
nothing in the report looks wrong, say so plainly and suggest what to check next. Never invent \
readings. Only suggest commands installed on Ubuntu by default (nmcli, journalctl, \
systemctl, powerprofilesctl, wpctl, bluetoothctl, df, top); diagnosis never needs sudo. Call \
the unit_status or journal tools if you need more detail. Reply in at most three short \
sentences, and do not describe what you checked or did.";

/// Progress-line id for the up-front check; the agent numbers tool calls
/// from 1.
const STEP_ID: usize = 0;

/// Add a system report to `question` when it is about a problem.
pub async fn ground(mut question: Question, ui: &dyn Ui) -> Question {
    if diagnostics::topics_for(&question.text).is_empty() {
        return question;
    }
    let label = i18n::umoja("Check this computer");
    ui.step(STEP_ID, &label, StepState::Running);
    let text = question.text.clone();
    let report = RUNTIME
        .spawn_blocking(move || diagnostics::report_for(&text))
        .await
        .ok()
        .flatten();
    let Some(report) = report else {
        ui.step(STEP_ID, &label, StepState::Failed);
        return question;
    };
    ui.step(STEP_ID, &label, StepState::Done);
    let grounded = format!("{PREAMBLE}\n\n<system-report>\n{report}</system-report>");
    question.context = Some(match question.context.take() {
        Some(ctx) if !ctx.trim().is_empty() => format!("{ctx}\n\n{grounded}"),
        _ => grounded,
    });
    question
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tools_register_without_confirmation() {
        let tools = tools();
        assert!(tools.iter().any(|t| t.name == "journal"));
        for t in &tools {
            assert_eq!(
                (t.risk)(&serde_json::json!({})),
                super::super::tools::Risk::Low
            );
            assert!(!t.describe(&serde_json::json!({})).is_empty());
        }
    }
}
