// OWNER: dialogs
//! `/rename`: a one-line prompt dialog; wizard has no rename, so the title is kept in
//! `state.json` and shown wherever openw shows the session.

use super::panel::Prompt;
use super::{Dialog, Effect};

pub fn open(title: &str) -> Box<dyn Dialog> {
    let mut p = Prompt::new("Rename Session", title, "Enter text", Effect::Rename);
    p.editor.move_doc_end();
    Box::new(p)
}
