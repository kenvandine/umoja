//! Print the assistant's diagnostics report for a question, or run one tool.
//! `cargo run -p umoja-common --example diagnose -- "why is my fan loud?"`
//! `cargo run -p umoja-common --example diagnose -- --tool unit_status '{"unit":"bluetooth.service"}'`
use umoja_common::diagnostics;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--tool") {
        let name = args.get(1).map(String::as_str).unwrap_or_default();
        let json = args.get(2).map(String::as_str).unwrap_or("{}");
        let v = serde_json::from_str(json).unwrap_or_default();
        println!(
            "{}",
            diagnostics::run_tool(name, &v).unwrap_or_else(|| "unknown tool".into())
        );
        return;
    }
    let q = args.join(" ");
    println!("topics: {:?}", diagnostics::topics_for(&q));
    println!(
        "{}",
        diagnostics::report_for(&q).unwrap_or_else(|| "(no report)".into())
    );
}
