//! Troubleshooting questions start from a system report.
//!
//! The collectors live in [`umoja_common::diagnostics`] (umoja-mcp serves
//! them as tools); for questions that look like
//! troubleshooting ("why is my fan loud?", "Wi-Fi won't connect"), gathers
//! a report up front and adds it to the question's context. Small local
//! models often answer from general knowledge instead of calling a tool,
//! so grounding the first turn makes the answer cite this computer's
//! actual readings. The model can still call the tools for more detail
//! (a unit's status, a journal search).

use super::agent::{Question, StepState, Ui};
use crate::lemonade::RUNTIME;
use umoja_common::{diagnostics, i18n};

/// How the report is introduced to the model.
const PREAMBLE: &str = "The user is troubleshooting this computer. Below is a read-only \
report gathered just now from it (sensors, /proc, systemd, the journal, NetworkManager). \
Answer from it: name the most likely cause and quote the evidence (a process and its CPU %, a \
temperature, a log line). CPU % is processor load; memory use does not make a fan spin. \
Wi-Fi signal above 60% is good, so blame signal only when it is low or drops are logged. If \
nothing in the report looks wrong, say so plainly and suggest what to check next. Never invent \
readings. Only suggest commands installed on Ubuntu by default (nmcli, journalctl, \
systemctl, powerprofilesctl, wpctl, bluetoothctl, df, top); diagnosis never needs sudo. Call \
the unit_status or journal tools if you need more detail. Reply in at most three short \
sentences, and do not describe what you checked or did.";

/// Extra rules when the question is about a crash: work from the
/// evidence, the way a careful developer would, rather than a plausible
/// story.
const CRASH_RULES: &str = "For a crash: first rule out the boring causes the report shows (the \
system was out of memory). Several programs crashing in the same minute usually means the \
display or session went away (the desktop restarting, a logout), not a separate bug in each; \
say so. Blame a package update only when the crashing program or the library in its stack \
was updated just before; otherwise don't call it the cause. \
Say whether it is a one-off or keeps happening. Use the signal in plain words (a segmentation \
fault is a bug in the program; an abort usually means it hit an error it checks for). Quote \
the top stack frame only if the report has one, and never guess at a cause the report doesn't \
show: if there is no stack trace, say the exact cause can't be told from here. If it is from \
an Ubuntu package, offer to open Ubuntu's crash reporter (the report_crash tool) so it can be \
sent to the developers; otherwise suggest reporting it to the program's developers. Restarting \
the app is usually all the user needs to do now.";

/// Progress-line id for the up-front check; the agent numbers tool calls
/// from 1.
const STEP_ID: usize = 0;

/// Add a system report to `question` when it is about a problem.
pub async fn ground(mut question: Question, ui: &dyn Ui) -> Question {
    let topics = diagnostics::topics_for(&question.text);
    if topics.is_empty() {
        return question;
    }
    let crash = topics.contains(&diagnostics::Topic::Crashes);
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
    let rules = if crash {
        format!("{PREAMBLE} {CRASH_RULES}")
    } else {
        PREAMBLE.to_string()
    };
    let grounded = format!("{rules}\n\n<system-report>\n{report}</system-report>");
    question.context = Some(match question.context.take() {
        Some(ctx) if !ctx.trim().is_empty() => format!("{ctx}\n\n{grounded}"),
        _ => grounded,
    });
    question
}
