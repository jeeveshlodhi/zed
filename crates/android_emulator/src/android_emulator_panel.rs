mod discovery;
mod emulator_client;
pub mod display;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use collections::HashMap;
use fs::Fs;
use gpui::{
    App, AsyncWindowContext, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement,
    ParentElement, Pixels, Render, Styled, Subscription, Task, WeakEntity, Window, actions, div,
    px,
};
use ui::{
    Color, FluentBuilder, IconButton, IconName, IconSize, Label, LabelCommon, LabelSize,
    ListItem, ListItemSpacing, Tooltip, prelude::*,
};
use util::ResultExt;
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

use discovery::{AvdInfo, EmulatorStatus, grpc_port_from_serial, list_avds, list_running_emulators};
use display::EmulatorDisplay;

const ANDROID_EMULATOR_PANEL_KEY: &str = "AndroidEmulatorPanel";

actions!(
    android_emulator,
    [
        /// Toggles the Android Emulator panel.
        Toggle,
        /// Moves focus to the Android Emulator panel.
        ToggleFocus,
    ]
);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _window, _cx: &mut Context<Workspace>| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<AndroidEmulatorPanel>(window, cx);
        });
        workspace.register_action(|workspace, _: &Toggle, window, cx| {
            if !workspace.toggle_panel_focus::<AndroidEmulatorPanel>(window, cx) {
                workspace.close_panel::<AndroidEmulatorPanel>(window, cx);
            }
        });
    })
    .detach();
}

/// The panel can be in one of two modes: browsing the AVD list, or actively
/// displaying a connected emulator's screen.
enum PanelMode {
    /// Showing the list of available AVDs with start/stop controls.
    AvdList,
    /// Showing the live emulator display for the given AVD.
    Display {
        avd_name: String,
        display: Entity<EmulatorDisplay>,
    },
}

pub struct AndroidEmulatorPanel {
    focus_handle: FocusHandle,
    #[allow(dead_code)]
    fs: Arc<dyn Fs>,
    #[allow(dead_code)]
    workspace: WeakEntity<Workspace>,
    sdk_root: Option<PathBuf>,
    avds: Vec<AvdInfo>,
    emulator_status: HashMap<String, EmulatorStatus>,
    selected_avd: Option<String>,
    is_refreshing: bool,
    launched_emulator_process_ids: Vec<u32>,
    mode: PanelMode,
    _subscriptions: Vec<Subscription>,
    _refresh_task: Task<()>,
}

impl AndroidEmulatorPanel {
    pub fn new(workspace: &Workspace, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        let fs = workspace.app_state().fs.clone();
        let sdk_root = discover_sdk_root();

        let subscriptions = vec![cx.on_app_quit(Self::app_will_quit)];

        let mut panel = Self {
            focus_handle,
            fs,
            workspace: workspace.weak_handle(),
            sdk_root,
            avds: Vec::new(),
            emulator_status: HashMap::default(),
            selected_avd: None,
            is_refreshing: false,
            launched_emulator_process_ids: Vec::new(),
            mode: PanelMode::AvdList,
            _subscriptions: subscriptions,
            _refresh_task: Task::ready(()),
        };

        panel.refresh(window, cx);
        panel
    }

    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> Result<Entity<Self>> {
        workspace.update_in(&mut cx, |workspace, window, cx| {
            cx.new(|cx| Self::new(workspace, window, cx))
        })
    }

    fn avdmanager_path(&self) -> Option<PathBuf> {
        let sdk = self.sdk_root.as_ref()?;
        for relative in &[
            "cmdline-tools/latest/bin/avdmanager",
            "tools/bin/avdmanager",
        ] {
            let path = sdk.join(relative);
            if path.exists() {
                return Some(path);
            }
        }
        None
    }

    fn adb_path(&self) -> Option<PathBuf> {
        let path = self.sdk_root.as_ref()?.join("platform-tools/adb");
        path.exists().then_some(path)
    }

    fn emulator_path(&self) -> Option<PathBuf> {
        let path = self.sdk_root.as_ref()?.join("emulator/emulator");
        path.exists().then_some(path)
    }

    pub fn refresh(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let avdmanager_path = self.avdmanager_path();
        let adb_path = self.adb_path();

        self.is_refreshing = true;
        cx.notify();

        self._refresh_task = cx.spawn(async move |this, cx| {
            let avds = match avdmanager_path {
                Some(path) => list_avds(&path).await.log_err().unwrap_or_default(),
                None => Vec::new(),
            };

            let running = match adb_path {
                Some(path) => list_running_emulators(&path)
                    .await
                    .log_err()
                    .unwrap_or_default(),
                None => HashMap::default(),
            };

            this.update(cx, |panel, cx| {
                panel.avds = avds;
                panel.emulator_status = running;
                panel.is_refreshing = false;
                cx.notify();
            })
            .log_err();
        });
    }

    fn launch_avd(&mut self, avd_name: String, _window: &mut Window, cx: &mut Context<Self>) {
        let emulator_path = match self.emulator_path() {
            Some(path) => path,
            None => return,
        };
        let adb_path = self.adb_path();

        cx.spawn({
            let avd_name = avd_name.clone();
            async move |this, cx| {
                let grpc_port = pick_free_port().unwrap_or(8554);
                let child = match smol::process::Command::new(&emulator_path)
                    .arg("-avd")
                    .arg(&avd_name)
                    .arg("-no-window")
                    .arg("-grpc")
                    .arg(grpc_port.to_string())
                    .stdin(smol::process::Stdio::null())
                    .stdout(smol::process::Stdio::null())
                    .stderr(smol::process::Stdio::null())
                    .spawn()
                {
                    Ok(child) => child,
                    Err(error) => {
                        log::error!(
                            "android_emulator: failed to launch AVD {}: {}",
                            avd_name,
                            error
                        );
                        return;
                    }
                };
                let emulator_process_id = child.id();
                drop(child);

                this.update(cx, |panel, _cx| {
                    panel
                        .launched_emulator_process_ids
                        .push(emulator_process_id);
                })
                .log_err();

                let mut launched = adb_path.is_none();
                if let Some(adb_path) = adb_path {
                    for _ in 0..45 {
                        cx.background_executor()
                            .timer(std::time::Duration::from_secs(1))
                            .await;

                        let running = list_running_emulators(&adb_path)
                            .await
                            .log_err()
                            .unwrap_or_default();

                        if matches!(running.get(&avd_name), Some(EmulatorStatus::Running { .. })) {
                            launched = true;
                            break;
                        }
                    }
                }

                if !launched {
                    log::error!(
                        "android_emulator: AVD {} did not appear in adb after launch",
                        avd_name
                    );
                    return;
                }

                this.update(cx, |panel, cx| {
                    panel.open_display(avd_name.clone(), grpc_port, cx);
                    panel.refresh_without_window(cx);
                })
                .log_err();
            }
        })
        .detach();
    }

    fn app_will_quit(&mut self, _cx: &mut Context<Self>) -> Task<()> {
        for emulator_process_id in std::mem::take(&mut self.launched_emulator_process_ids) {
            let terminate_result = std::process::Command::new("kill")
                .arg("-TERM")
                .arg(emulator_process_id.to_string())
                .status();
            if let Err(error) = terminate_result {
                log::error!(
                    "android_emulator: failed to terminate emulator process {} on app quit: {}",
                    emulator_process_id,
                    error
                );
            }
        }

        Task::ready(())
    }

    /// Switches the panel into display mode for the given AVD / gRPC port.
    pub fn open_display(
        &mut self,
        avd_name: String,
        grpc_port: u16,
        cx: &mut Context<Self>,
    ) {
        let display = cx.new(|cx| EmulatorDisplay::new(grpc_port, cx));
        self.mode = PanelMode::Display { avd_name, display };
        cx.notify();
    }

    /// Switches back to the AVD list and refreshes device status.
    fn close_display(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.mode = PanelMode::AvdList;
        self.refresh(window, cx);
    }

    fn stop_avd(&mut self, avd_name: String, _window: &mut Window, cx: &mut Context<Self>) {
        let adb_path = match self.adb_path() {
            Some(path) => path,
            None => return,
        };

        let serial = match self.emulator_status.get(&avd_name) {
            Some(EmulatorStatus::Running { serial }) => serial.clone(),
            _ => return,
        };

        cx.spawn(async move |this, cx| {
            smol::process::Command::new(&adb_path)
                .args(["-s", &serial, "emu", "kill"])
                .output()
                .await
                .log_err();

            cx.background_executor()
                .timer(std::time::Duration::from_secs(2))
                .await;

            this.update(cx, |panel, cx| {
                // If this was the active display, go back to the list.
                if let PanelMode::Display { avd_name: ref name, .. } = panel.mode {
                    if *name == avd_name {
                        panel.mode = PanelMode::AvdList;
                    }
                }
                panel.refresh_without_window(cx);
            })
            .log_err();
        })
        .detach();
    }

    /// Refreshes AVD list without needing a `&mut Window` — safe to call from
    /// async contexts that only have `AsyncApp`.
    fn refresh_without_window(&mut self, cx: &mut Context<Self>) {
        let avdmanager_path = self.avdmanager_path();
        let adb_path = self.adb_path();

        self.is_refreshing = true;
        cx.notify();

        self._refresh_task = cx.spawn(async move |this, cx| {
            let avds = match avdmanager_path {
                Some(path) => list_avds(&path).await.log_err().unwrap_or_default(),
                None => Vec::new(),
            };
            let running = match adb_path {
                Some(path) => list_running_emulators(&path)
                    .await
                    .log_err()
                    .unwrap_or_default(),
                None => HashMap::default(),
            };
            this.update(cx, |panel, cx| {
                panel.avds = avds;
                panel.emulator_status = running;
                panel.is_refreshing = false;
                cx.notify();
            })
            .log_err();
        });
    }

    fn render_avd_row(
        &self,
        avd: AvdInfo,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let avd_name = avd.name.clone();
        let is_selected = self.selected_avd.as_deref() == Some(&avd.name);
        let is_running = matches!(
            self.emulator_status.get(&avd.name),
            Some(EmulatorStatus::Running { .. })
        );

        let launch_name = avd_name.clone();
        let stop_name = avd_name.clone();
        let open_name = avd_name.clone();
        let select_name = avd_name.clone();

        ListItem::new(ElementId::Name(avd_name.into()))
            .toggle_state(is_selected)
            .spacing(ListItemSpacing::Sparse)
            .on_click(cx.listener(move |this, _, _window, cx| {
                this.selected_avd = Some(select_name.clone());
                cx.notify();
            }))
            .child(
                h_flex()
                    .w_full()
                    .justify_between()
                    .gap_2()
                    .child(
                        v_flex()
                            .min_w_0()
                            .flex_1()
                            .child(
                                Label::new(avd.name.clone())
                                    .size(LabelSize::Small)
                                    .truncate(),
                            )
                            .child(
                                Label::new(format!("{} · {}", avd.target, avd.abi))
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted)
                                    .truncate(),
                            ),
                    )
                    .child(
                        h_flex()
                            .flex_none()
                            .gap_1()
                            .items_center()
                            .when(is_running, |row| {
                                row.child(
                                    Label::new("Running")
                                        .size(LabelSize::XSmall)
                                        .color(Color::Success),
                                )
                                .child(
                                    IconButton::new(
                                        ElementId::Name(format!("open-{}", open_name).into()),
                                        IconName::Screen,
                                    )
                                    .icon_size(IconSize::Small)
                                    .tooltip(Tooltip::text("Open display"))
                                    .on_click(cx.listener(move |this, _, _window, cx| {
                                        let grpc_port = this
                                            .emulator_status
                                            .get(&open_name)
                                            .and_then(|s| {
                                                if let EmulatorStatus::Running { serial } = s {
                                                    grpc_port_from_serial(serial)
                                                } else {
                                                    None
                                                }
                                            })
                                            .unwrap_or(8554);
                                        this.open_display(
                                            open_name.clone(),
                                            grpc_port,
                                            cx,
                                        );
                                    })),
                                )
                                .child(
                                    IconButton::new(
                                        ElementId::Name(format!("stop-{}", stop_name).into()),
                                        IconName::Power,
                                    )
                                    .icon_size(IconSize::Small)
                                    .tooltip(Tooltip::text("Stop emulator"))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.stop_avd(stop_name.clone(), window, cx);
                                    })),
                                )
                            })
                            .when(!is_running, |row| {
                                row.child(
                                    IconButton::new(
                                        ElementId::Name(format!("launch-{}", launch_name).into()),
                                        IconName::PlayFilled,
                                    )
                                    .icon_size(IconSize::Small)
                                    .tooltip(Tooltip::text("Launch emulator"))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.launch_avd(launch_name.clone(), window, cx);
                                    })),
                                )
                            }),
                    ),
            )
    }
}

impl EventEmitter<PanelEvent> for AndroidEmulatorPanel {}

impl Focusable for AndroidEmulatorPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for AndroidEmulatorPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let has_sdk = self.sdk_root.is_some();

        // ── Display mode ──────────────────────────────────────────────────
        if let PanelMode::Display { avd_name, display } = &self.mode {
            let avd_name = avd_name.clone();
            let display = display.clone();

            return v_flex()
                .key_context("AndroidEmulatorPanel")
                .track_focus(&self.focus_handle)
                .size_full()
                .bg(cx.theme().colors().panel_background)
                .child(
                    h_flex()
                        .h_8()
                        .px_2()
                        .flex_none()
                        .items_center()
                        .justify_between()
                        .border_b_1()
                        .border_color(cx.theme().colors().border)
                        .child(
                            h_flex()
                                .gap_1()
                                .items_center()
                                .child(
                                    IconButton::new("back-to-list", IconName::ArrowLeft)
                                        .icon_size(IconSize::Small)
                                        .tooltip(Tooltip::text("Back to device list"))
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            this.close_display(window, cx);
                                        })),
                                )
                                .child(
                                    Label::new(avd_name)
                                        .size(LabelSize::Small)
                                        .color(Color::Default),
                                ),
                        ),
                )
                .child(div().flex_1().child(display))
                .into_any();
        }

        // ── AVD list mode ─────────────────────────────────────────────────
        v_flex()
            .key_context("AndroidEmulatorPanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().colors().panel_background)
            .child(
                h_flex()
                    .h_8()
                    .px_2()
                    .flex_none()
                    .items_center()
                    .justify_between()
                    .border_b_1()
                    .border_color(cx.theme().colors().border)
                    .child(
                        Label::new("Android Emulator")
                            .size(LabelSize::Small)
                            .color(Color::Default),
                    )
                    .child(
                        IconButton::new("android-emulator-refresh", IconName::RotateCw)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("Refresh device list"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.refresh(window, cx);
                            })),
                    ),
            )
            .when(!has_sdk, |container| {
                container.child(
                    v_flex()
                        .p_4()
                        .gap_2()
                        .child(
                            Label::new("Android SDK not found")
                                .size(LabelSize::Small)
                                .color(Color::Warning),
                        )
                        .child(
                            Label::new(
                                "Set ANDROID_SDK_ROOT or ANDROID_HOME, or install \
                                 the Android SDK in ~/Library/Android/sdk (macOS) \
                                 or ~/Android/Sdk (Linux).",
                            )
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                        ),
                )
            })
            .when(has_sdk && self.is_refreshing, |container| {
                container.child(
                    div().p_4().child(
                        Label::new("Refreshing…")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
                )
            })
            .when(
                has_sdk && !self.is_refreshing && self.avds.is_empty(),
                |container| {
                    container.child(
                        div().p_4().child(
                            Label::new(
                                "No AVDs found. Create one using avdmanager \
                                 or Android Studio.",
                            )
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                        ),
                    )
                },
            )
            .when(
                has_sdk && !self.is_refreshing && !self.avds.is_empty(),
                |container| {
                    let rows: Vec<_> = self
                        .avds
                        .clone()
                        .into_iter()
                        .map(|avd| self.render_avd_row(avd, cx).into_any_element())
                        .collect();

                    container.child(
                        div()
                            .id("android-avd-list")
                            .flex_1()
                            .overflow_y_scroll()
                            .children(rows),
                    )
                },
            )
            .into_any()
    }
}

impl Panel for AndroidEmulatorPanel {
    fn persistent_name() -> &'static str {
        "AndroidEmulatorPanel"
    }

    fn panel_key() -> &'static str {
        ANDROID_EMULATOR_PANEL_KEY
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        DockPosition::Right
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(
            position,
            DockPosition::Left | DockPosition::Right | DockPosition::Bottom
        )
    }

    fn set_position(
        &mut self,
        _position: DockPosition,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
    }

    fn default_size(&self, _window: &Window, _cx: &App) -> Pixels {
        px(320.)
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<ui::IconName> {
        Some(IconName::Screen)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Android Emulator")
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        Box::new(Toggle)
    }

    fn activation_priority(&self) -> u32 {
        10
    }
}

/// Searches well-known environment variables and default install locations
/// for the Android SDK root directory.
fn discover_sdk_root() -> Option<PathBuf> {
    for env_var in &["ANDROID_SDK_ROOT", "ANDROID_HOME"] {
        if let Ok(value) = std::env::var(env_var) {
            let path = PathBuf::from(value);
            if path.exists() {
                return Some(path);
            }
        }
    }

    let home = dirs::home_dir()?;
    let candidates = [
        home.join("Library/Android/sdk"),
        home.join("Android/Sdk"),
    ];
    for candidate in &candidates {
        if candidate.exists() {
            return Some(candidate.clone());
        }
    }

    None
}

fn pick_free_port() -> Option<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").ok()?;
    let port = listener.local_addr().ok()?.port();
    Some(port)
}
