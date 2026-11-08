//! Read-only troubleshooting tools; the collectors live in
//! [`umoja_common::diagnostics`].

use crate::tool::{Outcome, Tool, blocking};
use umoja_common::diagnostics;

pub fn tools() -> Vec<Tool> {
    diagnostics::tools()
        .into_iter()
        .map(|t| {
            let f = t.run;
            Tool::read_only(t.name, t.description, (t.parameters)(), move |a| {
                Box::pin(async move {
                    match blocking(move || f(&a)).await {
                        Ok(s) => Outcome::text(s),
                        Err(e) => Outcome::err(e),
                    }
                })
            })
            .with_summary(t.summary)
        })
        .collect()
}

/// A read-only report for a troubleshooting question, or `None` when the
/// question isn't about a problem.
pub fn report_for(question: &str) -> Option<String> {
    if diagnostics::topics_for(question).is_empty() {
        return None;
    }
    diagnostics::report_for(question)
}
