use gpui::{
    App, Application, Bounds, Context, SharedString, Window, WindowBounds, WindowOptions, div,
    prelude::*, px, rgb, size,
};

struct NativeShell {
    projects: Vec<SharedString>,
    agents: Vec<SharedString>,
}

impl NativeShell {
    fn new() -> Self {
        Self {
            projects: vec![
                SharedString::from("UserFirst"),
                SharedString::from("GPUI Prototype"),
                SharedString::from("Terminal Rewrite"),
            ],
            agents: vec![
                SharedString::from("Claude 1"),
                SharedString::from("Codex 1"),
            ],
        }
    }
}

impl Render for NativeShell {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let project_rows = self.projects.iter().map(|project| {
            div()
                .flex()
                .items_center()
                .justify_between()
                .px_3()
                .py_2()
                .rounded_md()
                .bg(rgb(0x151922))
                .border_1()
                .border_color(rgb(0x232833))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(div().size_2().rounded_full().bg(rgb(0x4f46e5)))
                        .child(div().text_sm().text_color(rgb(0xe5e7eb)).child(project.clone())),
                )
                .child(div().text_xs().text_color(rgb(0x6b7280)).child("3"))
        });

        let agent_rows = self.agents.iter().map(|agent| {
            div()
                .flex()
                .items_center()
                .gap_2()
                .px_3()
                .py_2()
                .rounded_md()
                .bg(rgb(0x12161f))
                .border_1()
                .border_color(rgb(0x222733))
                .child(div().size_2().rounded_full().bg(rgb(0xf59e0b)))
                .child(div().text_sm().text_color(rgb(0xcbd5e1)).child(agent.clone()))
                .child(div().text_xs().text_color(rgb(0x64748b)).child("thinking"))
        });

        div()
            .size_full()
            .flex()
            .bg(rgb(0x090b10))
            .text_color(rgb(0xe2e8f0))
            .child(
                div()
                    .w(px(300.0))
                    .h_full()
                    .flex_none()
                    .flex()
                    .flex_col()
                    .gap_4()
                    .p_4()
                    .bg(rgb(0x0f131b))
                    .border_r_1()
                    .border_color(rgb(0x222733))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(div().text_lg().font_weight(gpui::FontWeight::BOLD).child("DevManager"))
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(rgb(0x7c8799))
                                    .child("Native rewrite in progress"),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .child(div().text_xs().text_color(rgb(0x94a3b8)).child("PROJECTS"))
                                    .child(div().text_xs().text_color(rgb(0x64748b)).child("+ Native")),
                            )
                            .children(project_rows),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(div().text_xs().text_color(rgb(0x94a3b8)).child("AI SESSIONS"))
                            .children(agent_rows),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .p_3()
                            .rounded_lg()
                            .bg(rgb(0x12161f))
                            .border_1()
                            .border_color(rgb(0x222733))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(0x94a3b8))
                                    .child("Archived source: zz-archive/tauri-react-v0.1.11"),
                            ),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .h_full()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .h(px(52.0))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_between()
                            .px_4()
                            .bg(rgb(0x0c1016))
                            .border_b_1()
                            .border_color(rgb(0x222733))
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap_1()
                                    .child(div().text_sm().font_weight(gpui::FontWeight::SEMIBOLD).child("Native Terminal Pane"))
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(rgb(0x64748b))
                                            .child("GPUI shell online, alacritty_terminal integration next"),
                                    ),
                            )
                            .child(
                                div()
                                    .px_3()
                                    .py_1()
                                    .rounded_full()
                                    .bg(rgb(0x132016))
                                    .text_xs()
                                    .text_color(rgb(0x86efac))
                                    .child("prototype"),
                            ),
                    )
                    .child(
                        div()
                            .flex_1()
                            .p_4()
                            .child(
                                div()
                                    .size_full()
                                    .flex()
                                    .flex_col()
                                    .rounded_xl()
                                    .bg(rgb(0x05070b))
                                    .border_1()
                                    .border_color(rgb(0x1b2230))
                                    .overflow_hidden()
                                    .child(
                                        div()
                                            .h(px(40.0))
                                            .flex_none()
                                            .flex()
                                            .items_center()
                                            .justify_between()
                                            .px_4()
                                            .bg(rgb(0x0b0e14))
                                            .border_b_1()
                                            .border_color(rgb(0x1b2230))
                                            .child(div().text_sm().text_color(rgb(0xa5b4c3)).child("devmanager-native"))
                                            .child(div().text_xs().text_color(rgb(0x64748b)).child("terminal surface placeholder")),
                                    )
                                    .child(
                                        div()
                                            .flex_1()
                                            .flex()
                                            .justify_center()
                                            .items_center()
                                            .child(
                                                div()
                                                    .w(px(560.0))
                                                    .flex()
                                                    .flex_col()
                                                    .gap_3()
                                                    .p_6()
                                                    .rounded_xl()
                                                    .bg(rgb(0x0a1117))
                                                    .border_1()
                                                    .border_color(rgb(0x20303d))
                                                    .child(
                                                        div()
                                                            .text_lg()
                                                            .font_weight(gpui::FontWeight::BOLD)
                                                            .child("Phase 1"),
                                                    )
                                                    .child(
                                                        div()
                                                            .text_sm()
                                                            .text_color(rgb(0x94a3b8))
                                                            .child("The old Tauri app is archived. This native GPUI shell is the new root app."),
                                                    )
                                                    .child(
                                                        div()
                                                            .text_sm()
                                                            .text_color(rgb(0x94a3b8))
                                                            .child("Next: hook alacritty_terminal to a real terminal surface and start porting the session model."),
                                                    ),
                                            ),
                                    ),
                            ),
                    ),
            )
    }
}

fn main() {
    Application::new().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1440.0), px(920.0)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some("DevManager".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
            |_, cx| cx.new(|_| NativeShell::new()),
        )
        .unwrap();
        cx.activate(true);
    });
}
