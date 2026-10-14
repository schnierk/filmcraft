//! `regie.plan`: understand a request (see [the module overview](super)).

use serde_json::{Value, json};

use crate::commands::{CommandSpec, always, bad, str_p};
use crate::{Result, Session};

use super::types::Plan;

pub(crate) fn commands() -> Vec<CommandSpec> {
    vec![CommandSpec {
        id: "regie.plan",
        label: "AI Director: Understand Request",
        menu: &[],
        shortcut: None,
        params: r#"{"request":str,"language":"de"|"en"?}"#,
        enabled: always,
        run: run_plan,
        journal: false,
    }]
}

fn run_plan(_s: &mut Session, p: &Value) -> Result<Value> {
    let request = str_p(p, "request").ok_or_else(|| bad("regie.plan", "`request` (text) is required"))?;
    let plan = Plan { request: request.to_string(), ..Plan::default() };
    Ok(json!(plan))
}
