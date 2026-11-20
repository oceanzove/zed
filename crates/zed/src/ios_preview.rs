use anyhow::{Context as _, Result, bail};
use futures::{AsyncReadExt as _, AsyncWriteExt as _, FutureExt as _, pin_mut, select};
use gpui::{
    Action, Bounds, EventEmitter, FocusHandle, Focusable, Image, ImageFormat, Keystroke,
    MouseButton, ObjectFit, Point, Render, RenderImage, Subscription, Task, WeakEntity, actions,
    canvas, img,
};
use project::{Project, trusted_worktrees::TrustedWorktrees};
use serde_json::{Value, json};
use smol::{channel, process::ChildStdout};
use std::{
    cell::Cell,
    io::ErrorKind,
    path::Path,
    process::{Command, Stdio},
    rc::Rc,
    sync::Arc,
    time::Duration,
};
use ui::prelude::*;
use util::{ResultExt as _, process::Child};
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

const BRIDGE_SCRIPT: &str = ".zed/ios_simulator.py";
const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
const MAX_STATUS_BYTES: usize = 16 * 1024;

actions!(
    ios_preview,
    [
        /// Opens the iOS simulator preview panel.
        Toggle,
        /// Opens the iOS preview panel and starts the simulator.
        Start
    ]
);

pub fn register(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let project = workspace.project().downgrade();
    let window_handle = window.window_handle();
    let panel = cx.new(|cx: &mut Context<PreviewPanel>| {
        let focus = cx.focus_handle();
        let panel_focus = focus.clone();
        let panel = cx.weak_entity();
        let keystrokes = cx.intercept_keystrokes(move |event, window, cx| {
            if panel_focus.is_focused(window) {
                panel
                    .update(cx, |panel, cx| panel.forward_key(&event.keystroke, cx))
                    .ok();
            }
        });
        cx.on_release(move |panel, cx| {
            if let Some(image) = panel.rendered_frame.take() {
                window_handle
                    .update(cx, |_, window, _| window.drop_image(image).log_err())
                    .ok();
            }
            if let Some(image) = panel.retired_frame.take() {
                window_handle
                    .update(cx, |_, window, _| window.drop_image(image).log_err())
                    .ok();
            }
        })
        .detach();
        PreviewPanel {
            project,
            focus,
            image_bounds: Rc::default(),
            latest_frame: None,
            rendered_frame: None,
            retired_frame: None,
            status: "Start the iOS simulator to preview this project.".into(),
            has_error: false,
            running: false,
            input: None,
            session: None,
            _keystrokes: keystrokes,
        }
    });
    workspace.add_panel(panel, window, cx);
    workspace.register_action(|workspace, _: &Toggle, window, cx| {
        if !workspace.toggle_panel_focus::<PreviewPanel>(window, cx) {
            workspace.close_panel::<PreviewPanel>(window, cx);
        }
    });
    workspace.register_action(|workspace, _: &Start, window, cx| {
        workspace.reveal_panel::<PreviewPanel>(window, cx);
        if let Some(panel) = workspace.panel::<PreviewPanel>(cx) {
            panel.update(cx, |panel, cx| panel.start(window, cx));
        }
    });
}

struct PreviewPanel {
    project: WeakEntity<Project>,
    focus: FocusHandle,
    image_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    latest_frame: Option<Arc<RenderImage>>,
    rendered_frame: Option<Arc<RenderImage>>,
    retired_frame: Option<Arc<RenderImage>>,
    status: SharedString,
    has_error: bool,
    running: bool,
    input: Option<channel::Sender<Value>>,
    session: Option<Task<()>>,
    _keystrokes: Subscription,
}

impl PreviewPanel {
    fn start(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.running {
            return;
        }

        let root = self.project.upgrade().and_then(|project| {
            let project = project.read(cx);
            if TrustedWorktrees::has_restricted_worktrees(&project.worktree_store(), cx) {
                return None;
            }
            project.visible_worktrees(cx).find_map(|worktree| {
                let worktree = worktree.read(cx);
                worktree.as_local()?;
                let root = worktree.abs_path();
                root.join(BRIDGE_SCRIPT).is_file().then_some(root)
            })
        });
        let Some(root) = root else {
            self.set_status(
                "Open a trusted local project with .zed/ios_simulator.py to start the preview.",
                true,
                cx,
            );
            return;
        };

        let (input, receiver) = channel::bounded(64);
        let executor = cx.background_executor().clone();
        let svg_renderer = cx.svg_renderer();
        self.input = Some(input);
        self.running = true;
        self.set_status("Starting the simulator…", false, cx);
        self.session = Some(cx.spawn_in(window, async move |panel, cx| {
            let result = run_bridge(&root, receiver, async |message| {
                let message = match message {
                    BridgeMessage::Frame { format, bytes } => {
                        let svg_renderer = svg_renderer.clone();
                        let frame = executor
                            .spawn(async move {
                                Image::from_bytes(format, bytes).to_image_data(svg_renderer)
                            })
                            .await?;
                        PanelMessage::Frame(frame)
                    }
                    BridgeMessage::Status { message, error } => {
                        PanelMessage::Status { message, error }
                    }
                };
                panel.update(cx, |panel, cx| {
                    if panel.input.is_none() {
                        return;
                    }
                    match message {
                        PanelMessage::Frame(frame) => {
                            panel.latest_frame = Some(frame);
                            cx.notify();
                        }
                        PanelMessage::Status { message, error } => {
                            panel.set_status(message, error, cx);
                        }
                    }
                })
            })
            .await;
            panel
                .update(cx, |panel, cx| {
                    let stopping = panel.input.take().is_none();
                    panel.running = false;
                    if stopping {
                        panel.set_status("Preview stopped.", false, cx);
                    } else if let Err(error) = result {
                        panel.set_status(format!("{error:#}"), true, cx);
                    } else if panel.has_error {
                        cx.notify();
                    } else {
                        panel.set_status("Simulator preview ended.", false, cx);
                    }
                })
                .ok();
        }));
    }

    fn stop(&mut self, cx: &mut Context<Self>) {
        if self.input.take().is_some() {
            self.set_status("Stopping the simulator preview…", false, cx);
        }
    }

    fn set_status(&mut self, status: impl Into<SharedString>, error: bool, cx: &mut Context<Self>) {
        self.status = status.into();
        self.has_error = error;
        cx.notify();
    }

    fn send_input(&mut self, value: Value, cx: &mut Context<Self>) {
        if let Some(input) = &self.input
            && let Err(error) = input.try_send(value)
        {
            let status = if error.is_full() {
                "Simulator input is busy. Try again."
            } else {
                "Simulator input is disconnected."
            };
            self.set_status(status, true, cx);
        }
    }

    fn forward_key(&mut self, keystroke: &Keystroke, cx: &mut Context<Self>) {
        if self.input.is_none() {
            return;
        }

        let key = keystroke.key.as_str();
        let modifiers = keystroke.modifiers;
        let message = if modifiers.platform && key == "v" {
            cx.read_from_clipboard()
                .and_then(|item| item.text())
                .map(|text| json!({"type": "text", "text": text}))
        } else if modifiers.platform || modifiers.control {
            None
        } else if matches!(
            key,
            "backspace" | "enter" | "tab" | "escape" | "left" | "right" | "up" | "down"
        ) {
            Some(json!({"type": "key", "key": key}))
        } else {
            keystroke
                .key_char
                .as_ref()
                .filter(|text| !text.chars().any(char::is_control))
                .map(|text| json!({"type": "text", "text": text}))
        };
        if let Some(message) = message {
            self.send_input(message, cx);
            cx.stop_propagation();
        }
    }

    fn render_screen(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(frame) = self.latest_frame.clone() else {
            return div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .child(Icon::new(IconName::Screen).color(Color::Muted))
                .into_any_element();
        };

        if self.rendered_frame.as_ref() != Some(&frame) {
            if let Some(retired) = self.retired_frame.take() {
                window.drop_image(retired).log_err();
            }
            self.retired_frame = self.rendered_frame.replace(frame.clone());
        }

        let image_bounds = self.image_bounds.clone();
        let image_size = frame.size(0);
        div()
            .relative()
            .flex_1()
            .min_h_0()
            .overflow_hidden()
            .child(img(frame).size_full().object_fit(ObjectFit::Contain))
            .child(
                canvas(
                    move |bounds, _, _| {
                        image_bounds.set(Some(ObjectFit::Contain.get_bounds(bounds, image_size)));
                    },
                    |_, (), _, _| {},
                )
                .absolute()
                .inset_0(),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|panel, event: &gpui::MouseDownEvent, window, cx| {
                    if let Some(position) = panel
                        .image_bounds
                        .get()
                        .and_then(|bounds| normalize_position(bounds, event.position))
                    {
                        panel.focus.focus(window, cx);
                        panel.send_input(
                            json!({"type": "tap", "x": position.0, "y": position.1}),
                            cx,
                        );
                        cx.stop_propagation();
                    }
                }),
            )
            .into_any_element()
    }
}

impl EventEmitter<PanelEvent> for PreviewPanel {}

impl Focusable for PreviewPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Panel for PreviewPanel {
    fn persistent_name() -> &'static str {
        "iOS Preview"
    }

    fn panel_key() -> &'static str {
        "ios_preview"
    }

    fn position(&self, _: &Window, _: &App) -> DockPosition {
        DockPosition::Right
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        position == DockPosition::Right
    }

    fn set_position(&mut self, _: DockPosition, _: &mut Window, _: &mut Context<Self>) {}

    fn default_size(&self, _: &Window, _: &App) -> Pixels {
        px(340.)
    }

    fn min_size(&self, _: &Window, _: &App) -> Option<Pixels> {
        Some(px(190.))
    }

    fn icon(&self, _: &Window, _: &App) -> Option<IconName> {
        Some(IconName::Screen)
    }

    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some("iOS Preview")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(Toggle)
    }

    fn set_active(&mut self, active: bool, _: &mut Window, cx: &mut Context<Self>) {
        if !active {
            self.stop(cx);
        }
    }

    fn activation_priority(&self) -> u32 {
        120
    }
}

impl Render for PreviewPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let button = if self.input.is_some() {
            Button::new("stop-ios-preview", "Stop")
                .on_click(cx.listener(|panel, _, _, cx| panel.stop(cx)))
        } else {
            Button::new("start-ios-preview", "Start")
                .disabled(self.running)
                .on_click(cx.listener(|panel, _, window, cx| panel.start(window, cx)))
        };
        let status_color = if self.has_error {
            cx.theme().status().error
        } else {
            cx.theme().colors().text_muted
        };

        v_flex()
            .id("ios-preview")
            .track_focus(&self.focus)
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .child(
                h_flex()
                    .justify_between()
                    .px_3()
                    .py_1()
                    .child(Label::new("iOS Preview"))
                    .child(button),
            )
            .child(self.render_screen(window, cx))
            .child(
                div()
                    .px_3()
                    .py_2()
                    .text_xs()
                    .text_color(status_color)
                    .whitespace_normal()
                    .child(self.status.clone()),
            )
    }
}

enum BridgeMessage {
    Frame { format: ImageFormat, bytes: Vec<u8> },
    Status { message: String, error: bool },
}

enum PanelMessage {
    Frame(Arc<RenderImage>),
    Status { message: String, error: bool },
}

struct BridgeProcess(Child);

impl Drop for BridgeProcess {
    fn drop(&mut self) {
        self.0.kill().log_err();
    }
}

async fn run_bridge(
    root: &Path,
    input: channel::Receiver<Value>,
    mut on_message: impl AsyncFnMut(BridgeMessage) -> Result<()>,
) -> Result<()> {
    let python = std::env::var_os("ZED_IOS_PREVIEW_PYTHON").unwrap_or_else(|| "python3".into());
    let mut command = Command::new(python);
    command.args([BRIDGE_SCRIPT, "panel"]).current_dir(root);
    let mut child = BridgeProcess(Child::spawn(
        command,
        Stdio::piped(),
        Stdio::piped(),
        Stdio::inherit(),
    )?);
    let mut stdin = child
        .0
        .stdin
        .take()
        .context("Opening simulator input pipe")?;
    let mut stdout = child
        .0
        .stdout
        .take()
        .context("Opening simulator output pipe")?;

    let writer = async move {
        while let Ok(event) = input.recv().await {
            let mut data = serde_json::to_vec(&event)?;
            data.push(b'\n');
            stdin
                .write_all(&data)
                .await
                .context("Writing simulator input")?;
        }
        stdin.close().await.context("Closing simulator input")
    }
    .fuse();
    let reader = async {
        while let Some(message) = read_message(&mut stdout).await? {
            on_message(message).await?;
        }
        anyhow::Ok(())
    }
    .fuse();
    pin_mut!(writer, reader);

    select! {
        result = reader => result?,
        result = writer => {
            result?;
            let shutdown_timeout = smol::Timer::after(Duration::from_secs(2)).fuse();
            pin_mut!(shutdown_timeout);
            select! {
                result = reader => result?,
                _ = shutdown_timeout => return Ok(()),
            }
        },
    }

    if let Some(status) = child.0.try_status()?
        && !status.success()
    {
        bail!("Simulator preview exited with {status}");
    }
    Ok(())
}

async fn read_message(stdout: &mut ChildStdout) -> Result<Option<BridgeMessage>> {
    let mut kind = [0];
    match stdout.read_exact(&mut kind).await {
        Ok(()) => {}
        Err(error) if error.kind() == ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error).context("Reading simulator output"),
    }

    let mut length = [0; 4];
    stdout
        .read_exact(&mut length)
        .await
        .context("Reading simulator message length")?;
    let length = u32::from_be_bytes(length) as usize;
    let limit = if kind[0] == b'I' {
        MAX_FRAME_BYTES
    } else {
        MAX_STATUS_BYTES
    };
    if length > limit {
        bail!("Simulator preview sent an oversized message");
    }

    let mut bytes = vec![0; length];
    stdout
        .read_exact(&mut bytes)
        .await
        .context("Reading simulator message")?;
    let message = match kind[0] {
        b'I' if bytes.starts_with(b"\x89PNG\r\n\x1a\n") => BridgeMessage::Frame {
            format: ImageFormat::Png,
            bytes,
        },
        b'I' if bytes.starts_with(&[0xff, 0xd8, 0xff]) => BridgeMessage::Frame {
            format: ImageFormat::Jpeg,
            bytes,
        },
        b'S' | b'E' => BridgeMessage::Status {
            message: String::from_utf8(bytes).context("Reading simulator status text")?,
            error: kind[0] == b'E',
        },
        _ => bail!("Simulator preview sent an unrecognized message"),
    };
    Ok(Some(message))
}

fn normalize_position(bounds: Bounds<Pixels>, position: Point<Pixels>) -> Option<(f32, f32)> {
    if bounds.size.width <= px(0.) || bounds.size.height <= px(0.) || !bounds.contains(&position) {
        return None;
    }
    let offset = position - bounds.origin;
    Some((offset.x / bounds.size.width, offset.y / bounds.size.height))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignores_clicks_outside_the_simulator_screen() {
        let bounds = Bounds {
            origin: gpui::point(px(20.), px(10.)),
            size: gpui::size(px(100.), px(200.)),
        };
        assert_eq!(
            normalize_position(bounds, gpui::point(px(19.), px(110.))),
            None
        );
        assert_eq!(
            normalize_position(bounds, gpui::point(px(70.), px(110.))),
            Some((0.5, 0.5)),
        );
    }
}
