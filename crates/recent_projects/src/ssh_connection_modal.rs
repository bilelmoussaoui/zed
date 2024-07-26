use std::{path::PathBuf, sync::Arc};

use anyhow::Result;
use auto_update::AutoUpdater;
use editor::Editor;
use futures::channel::oneshot;
use gpui::{
    px, size, AnyWindowHandle, AsyncAppContext, Bounds, DismissEvent, EventEmitter, FocusableView,
    ParentElement as _, Render, SemanticVersion, SharedString, Task, View, WindowBounds,
    WindowHandle,
};
use release_channel::{AppVersion, ReleaseChannel};
use remote::{SshConnectionOptions, SshPlatform, SshSession};
use ui::{
    v_flex, FluentBuilder as _, InteractiveElement, IntoElement, Label, LabelCommon, Styled,
    StyledExt as _, ViewContext, VisualContext, WindowContext,
};
use util::paths::PathLikeWithPosition;
use workspace::{AppState, ModalView, Workspace};

pub struct SshPrompt {
    host: SharedString,
    status_message: Option<SharedString>,
    prompt: Option<(SharedString, oneshot::Sender<Result<String>>)>,
    editor: View<Editor>,
}

pub struct SshConnectionModal {
    prompt: View<SshPrompt>,
}
impl SshPrompt {
    pub fn new(host: impl Into<SharedString>, cx: &mut ViewContext<Self>) -> Self {
        let host = host.into();
        Self {
            host,
            status_message: None,
            prompt: None,
            editor: cx.new_view(|cx| Editor::single_line(cx)),
        }
    }

    pub fn set_prompt(
        &mut self,
        prompt: String,
        tx: oneshot::Sender<Result<String>>,
        cx: &mut ViewContext<Self>,
    ) {
        self.editor.update(cx, |editor, cx| {
            if prompt.contains("yes/no") {
                editor.set_redact_all(false, cx);
            } else {
                editor.set_redact_all(true, cx);
            }
        });
        dbg!("prompt", &prompt);
        self.prompt = Some((prompt.into(), tx));
        self.status_message.take();
        cx.focus_view(&self.editor);
        cx.notify();
    }

    pub fn set_status(&mut self, status: Option<String>, cx: &mut ViewContext<Self>) {
        dbg!("status", &status);
        self.status_message = status.map(|s| s.into());
        cx.notify();
    }

    pub fn confirm(&mut self, cx: &mut ViewContext<Self>) {
        if let Some((_, tx)) = self.prompt.take() {
            self.editor.update(cx, |editor, cx| {
                tx.send(Ok(editor.text(cx))).ok();
                editor.clear(cx);
            });
        }
    }
}

impl Render for SshPrompt {
    fn render(&mut self, cx: &mut ViewContext<Self>) -> impl IntoElement {
        v_flex()
            .key_context("PasswordPrompt")
            .p_4()
            .size_full()
            .child(Label::new(format!("SSH: {}", self.host)).size(ui::LabelSize::Large))
            .when_some(self.status_message.as_ref(), |el, status| {
                el.child(Label::new(status.clone()))
            })
            .when_some(self.prompt.as_ref(), |el, prompt| {
                el.child(Label::new(prompt.0.clone()))
                    .child(self.editor.clone())
            })
    }
}

impl SshConnectionModal {
    pub fn new(host: String, cx: &mut ViewContext<Self>) -> Self {
        Self {
            prompt: cx.new_view(|cx| SshPrompt::new(host, cx)),
        }
    }

    fn confirm(&mut self, _: &menu::Confirm, cx: &mut ViewContext<Self>) {
        self.prompt.update(cx, |prompt, cx| prompt.confirm(cx))
    }

    fn dismiss(&mut self, _: &menu::Cancel, cx: &mut ViewContext<Self>) {
        cx.remove_window();
    }
}

impl Render for SshConnectionModal {
    fn render(&mut self, cx: &mut ui::ViewContext<Self>) -> impl ui::IntoElement {
        v_flex()
            .elevation_3(cx)
            .p_4()
            .gap_2()
            .on_action(cx.listener(Self::dismiss))
            .on_action(cx.listener(Self::confirm))
            .w(px(400.))
            .child(self.prompt.clone())
    }
}

impl FocusableView for SshConnectionModal {
    fn focus_handle(&self, cx: &gpui::AppContext) -> gpui::FocusHandle {
        self.prompt.read(cx).editor.focus_handle(cx)
    }
}

impl EventEmitter<DismissEvent> for SshConnectionModal {}

impl ModalView for SshConnectionModal {}

#[derive(Clone)]
pub struct SshClientDelegate {
    window: AnyWindowHandle,
    ui: View<SshPrompt>,
    known_password: Option<String>,
}

impl remote::SshClientDelegate for SshClientDelegate {
    fn ask_password(
        &self,
        prompt: String,
        cx: &mut AsyncAppContext,
    ) -> oneshot::Receiver<Result<String>> {
        let (tx, rx) = oneshot::channel();
        let mut known_password = self.known_password.clone();
        if let Some(password) = known_password.take() {
            tx.send(Ok(password)).ok();
        } else {
            self.window
                .update(cx, |_, cx| {
                    self.ui.update(cx, |modal, cx| {
                        modal.set_prompt(prompt, tx, cx);
                    })
                })
                .ok();
        }
        rx
    }

    fn set_status(&self, status: Option<&str>, cx: &mut AsyncAppContext) {
        self.update_status(status, cx)
    }

    fn get_server_binary(
        &self,
        platform: SshPlatform,
        cx: &mut AsyncAppContext,
    ) -> oneshot::Receiver<Result<(PathBuf, SemanticVersion)>> {
        let (tx, rx) = oneshot::channel();
        let this = self.clone();
        cx.spawn(|mut cx| async move {
            tx.send(this.get_server_binary_impl(platform, &mut cx).await)
                .ok();
        })
        .detach();
        rx
    }

    fn remote_server_binary_path(&self, cx: &mut AsyncAppContext) -> Result<PathBuf> {
        let release_channel = cx.update(|cx| ReleaseChannel::global(cx))?;
        Ok(format!(".local/zed-remote-server-{}", release_channel.dev_name()).into())
    }
}

impl SshClientDelegate {
    fn update_status(&self, status: Option<&str>, cx: &mut AsyncAppContext) {
        self.window
            .update(cx, |_, cx| {
                self.ui.update(cx, |modal, cx| {
                    modal.set_status(status.map(|s| s.to_string()), cx);
                })
            })
            .ok();
    }

    async fn get_server_binary_impl(
        &self,
        platform: SshPlatform,
        cx: &mut AsyncAppContext,
    ) -> Result<(PathBuf, SemanticVersion)> {
        let (version, release_channel) = cx.update(|cx| {
            let global = AppVersion::global(cx);
            (global, ReleaseChannel::global(cx))
        })?;

        // In dev mode, build the remote server binary from source
        #[cfg(debug_assertions)]
        if release_channel == ReleaseChannel::Dev
            && platform.arch == std::env::consts::ARCH
            && platform.os == std::env::consts::OS
        {
            use smol::process::{Command, Stdio};

            self.update_status(Some("building remote server binary from source"), cx);
            log::info!("building remote server binary from source");
            run_cmd(Command::new("cargo").args(["build", "--package", "remote_server"])).await?;
            run_cmd(Command::new("strip").args(["target/debug/remote_server"])).await?;
            run_cmd(Command::new("gzip").args(["-9", "-f", "target/debug/remote_server"])).await?;

            let path = std::env::current_dir()?.join("target/debug/remote_server.gz");
            return Ok((path, version));

            async fn run_cmd(command: &mut Command) -> Result<()> {
                let output = command.stderr(Stdio::inherit()).output().await?;
                if !output.status.success() {
                    Err(anyhow::anyhow!("failed to run command: {:?}", command))?;
                }
                Ok(())
            }
        }

        self.update_status(Some("checking for latest version of remote server"), cx);
        let binary_path = AutoUpdater::get_latest_remote_server_release(
            platform.os,
            platform.arch,
            release_channel,
            cx,
        )
        .await?;

        Ok((binary_path, version))
    }
}

pub fn connect_over_ssh(
    connection_options: SshConnectionOptions,
    ui: View<SshPrompt>,
    app_state: Arc<AppState>,
    cx: &mut WindowContext,
) -> Task<Result<Arc<SshSession>>> {
    let window = cx.window_handle();
    let known_password = connection_options.password.clone();

    cx.spawn(|mut cx| async move {
        remote::SshSession::client(
            connection_options,
            Arc::new(SshClientDelegate {
                window,
                ui,
                known_password,
            }),
            &mut cx,
        )
        .await
    })
}

pub async fn open_ssh_project(
    connection_options: SshConnectionOptions,
    paths: Vec<PathLikeWithPosition<PathBuf>>,
    app_state: Arc<AppState>,
    _open_options: workspace::OpenOptions,
    cx: &mut AsyncAppContext,
) -> Result<()> {
    let options = cx.update(|cx| (app_state.build_window_options)(None, cx))?;
    let window = cx.open_window(options, |cx| {
        let project = project::Project::local(
            app_state.client.clone(),
            app_state.node_runtime.clone(),
            app_state.user_store.clone(),
            app_state.languages.clone(),
            app_state.fs.clone(),
            cx,
        );
        cx.new_view(|cx| Workspace::new(None, project, app_state.clone(), cx))
    })?;

    let result = window
        .update(cx, |workspace, cx| {
            cx.activate_window();
            workspace.toggle_modal(cx, |cx| {
                SshConnectionModal::new(connection_options.host.clone(), cx)
            });
            let ui = workspace
                .active_modal::<SshConnectionModal>(cx)
                .unwrap()
                .read(cx)
                .prompt
                .clone();
            connect_over_ssh(connection_options, ui, workspace.app_state().clone(), cx)
        })?
        .await;

    if result.is_err() {
        window.update(cx, |_, cx| cx.remove_window()).ok();
    }

    let session = result?;

    let project = cx.update(|cx| {
        project::Project::ssh(
            session,
            app_state.client.clone(),
            app_state.node_runtime.clone(),
            app_state.user_store.clone(),
            app_state.languages.clone(),
            app_state.fs.clone(),
            cx,
        )
    })?;

    for path in paths {
        project
            .update(cx, |project, cx| {
                project.find_or_create_worktree(&path.path_like, true, cx)
            })?
            .await?;
    }

    window.update(cx, |_, cx| {
        cx.replace_root_view(|cx| Workspace::new(None, project, app_state, cx))
    })?;
    window.update(cx, |_, cx| cx.activate_window())?;

    Ok(())
}
