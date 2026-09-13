//! Source snippet mirrored by the rustdoc `compile_fail` on `TaskComposer::bind`.
//! The executable gate is `cargo test --doc TaskComposer`.

use devmanager::ui::task_cockpit::composer::TaskComposer;

fn main() {
    let _ = TaskComposer::bind_with_catalog;
}
