pub mod detail;
pub mod forge;
pub mod list;

use gpui::App;
use workspace::Workspace;

pub use list::Open;

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _cx| {
        list::register(workspace);
    })
    .detach();
}
