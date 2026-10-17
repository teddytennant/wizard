// OWNER: dialogs
//! The palette's `Help`.

use super::panel::Alert;
use super::Dialog;

pub fn open(palette_key: &str) -> Box<dyn Dialog> {
    let mut a = Alert::new(
        "Help",
        format!("Press {palette_key} to see all available actions and commands in any context."),
    );
    a.hint = "esc/enter";
    Box::new(a)
}
