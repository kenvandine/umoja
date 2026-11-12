//! `umoja-mcp`: the Umoja MCP server on stdio.
//!
//!     umoja-mcp                 serve MCP on stdin/stdout
//!     umoja-mcp --dry-run       confirm as usual, but change nothing
//!     umoja-mcp --list          print the tools
//!     umoja-mcp --automations   run the user's automations (a session service)
//!     umoja-mcp call NAME [JSON]  run one tool (risky ones ask with a dialog)

use serde_json::{Value, json};

const USAGE: &str = "usage: umoja-mcp [--dry-run] | --list | --automations | call NAME [JSON-ARGUMENTS]";

fn main() {
    // stdout carries the protocol: logs go to stderr.
    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into());
    tracing_subscriber_init(&filter);

    let args: Vec<String> = std::env::args().skip(1).collect();
    let ctx = glib::MainContext::default();
    match args.first().map(String::as_str) {
        None | Some("--dry-run") => {
            let dry = args.first().is_some_and(|a| a == "--dry-run");
            ctx.block_on(umoja_mcp::server::serve_stdio(umoja_mcp::registry(), dry));
        }
        Some("--automations") => ctx.block_on(umoja_mcp::automations::serve()),
        Some("--list") => {
            for t in umoja_mcp::registry() {
                let risk = umoja_mcp::server::risk_name((t.risk)(&json!({})));
                println!("{} [{}{}]\n    {}\n", t.name, risk, if t.read_only { ", read-only" } else { "" }, t.description);
            }
        }
        Some("call") => {
            let Some(name) = args.get(1) else {
                eprintln!("{USAGE}");
                std::process::exit(2);
            };
            let arguments: Value = match args.get(2) {
                Some(a) => serde_json::from_str(a).unwrap_or_else(|e| {
                    eprintln!("bad JSON arguments: {e}");
                    std::process::exit(2);
                }),
                None => json!({}),
            };
            let tools = umoja_mcp::registry();
            let Some(tool) = tools.iter().find(|t| t.name == name).cloned() else {
                eprintln!("no tool named {name}; see umoja-mcp --list");
                std::process::exit(2);
            };
            let server = umoja_mcp::server::Server::new(tools, false, std::rc::Rc::new(|_| {}));
            let r = ctx.block_on(server.call(&tool, arguments));
            println!("{}", r.outcome.text);
            if let Some(d) = r.outcome.data {
                eprintln!("{}", serde_json::to_string_pretty(&d).unwrap_or_default());
            }
            std::process::exit(if r.outcome.is_error { 1 } else { 0 });
        }
        _ => {
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    }
}

fn tracing_subscriber_init(filter: &str) {
    use tracing_subscriber::{EnvFilter, fmt};
    let _ = fmt()
        .with_env_filter(EnvFilter::new(filter))
        .with_target(false)
        .with_writer(std::io::stderr)
        .try_init();
}
