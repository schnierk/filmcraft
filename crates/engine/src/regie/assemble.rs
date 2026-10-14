//! `regie.assemble`, `regie.run`, `regie.status`: build the film (see [the module overview](super)).

use serde_json::{Value, json};

use crate::commands::{CommandSpec, always};
use crate::{Result, Session};

pub(crate) fn commands() -> Vec<CommandSpec> {
    vec![CommandSpec {
        id: "regie.status",
        label: "AI Director: Status",
        menu: &[],
        shortcut: None,
        params: "{}",
        enabled: always,
        run: run_status,
        journal: false,
    }]
}

/// Whether a film is being made, its progress and the last result.
fn run_status(_s: &mut Session, _p: &Value) -> Result<Value> {
    Ok(json!({"running": false, "job": null, "progress": 0.0, "status": "", "result": null}))
}
