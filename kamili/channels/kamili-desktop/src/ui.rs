//! The window: sidebar (pages and feature specs) and a main pane (Build, Spec,
//! Runs, Settings). An open approval takes over the main pane until decided.

use std::path::PathBuf;
use std::sync::Arc;

use gpui_kit::TestSupportExt;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme, Disableable, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use kamili::{CancelToken, Config, Decision, RunSummary};

use crate::bridge::{self, EngineFactory, Job, UiSender, ui_channel};
use crate::state::{
    AppState, BUILD_STEPS, FeatureEntry, FeedItem, FeedKind, FileStatus, JobKind, LineKind,
    StepState, list_features,
};

actions!(kamili, [Quit]);

/// Opens the window and runs until it is closed. Must be called on the main
/// thread. Engine jobs run on `runtime`.
pub fn run(
    factory: Arc<dyn EngineFactory>,
    project_root: PathBuf,
    runtime: tokio::runtime::Handle,
) -> Result<(), kamili::Error> {
    gpui_kit::application().run(move |cx| {
        gpui_kit::init(cx);
        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.bind_keys([KeyBinding::new("cmd-q", Quit, None)]);
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();

        let name = project_root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| project_root.display().to_string());
        let options = WindowOptions {
            titlebar: Some(TitlebarOptions {
                title: Some(format!("Kamili — {name}").into()),
                ..Default::default()
            }),
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                None,
                size(px(1240.), px(820.)),
                cx,
            ))),
            ..Default::default()
        };
        gpui_kit::open_window(options, cx, |window, cx| {
            cx.new(|cx| KamiliApp::new(factory, project_root, runtime, window, cx))
        })
        .expect("failed to open the Kamili window");
        cx.activate(true);
    });
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Page {
    Build,
    Spec,
    Runs,
    Settings,
}

impl Page {
    const ALL: [(Page, &'static str); 4] = [
        (Page::Build, "Build"),
        (Page::Spec, "Spec"),
        (Page::Runs, "Runs"),
        (Page::Settings, "Settings"),
    ];
}

struct KamiliApp {
    root: PathBuf,
    factory: Arc<dyn EngineFactory>,
    runtime: tokio::runtime::Handle,
    tx: UiSender,
    state: AppState,
    page: Page,
    config: Result<Config, String>,
    features: Vec<FeatureEntry>,
    runs: Vec<RunSummary>,
    cancel: Option<CancelToken>,
    force: bool,
    docs_only: bool,
    /// Which file of the open approval is shown.
    selected_file: usize,
    feature_input: Entity<InputState>,
    task_input: Entity<InputState>,
    spec_input: Entity<InputState>,
    answer_input: Entity<InputState>,
    _subscriptions: Vec<Subscription>,
}

impl KamiliApp {
    fn new(
        factory: Arc<dyn EngineFactory>,
        root: PathBuf,
        runtime: tokio::runtime::Handle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let (tx, mut rx) = ui_channel();
        // Engine messages arrive on a tokio channel; tokio's sync primitives
        // work on any executor, so GPUI can await them directly.
        cx.spawn(async move |this, cx| {
            while let Some(msg) = rx.recv().await {
                if this
                    .update(cx, |app, cx| {
                        app.on_message(msg);
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();

        let input = |placeholder: &'static str, window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| InputState::new(window, cx).placeholder(placeholder))
        };
        let feature_input = input("feature, e.g. user-auth", window, cx);
        let task_input = input(
            "what to build, e.g. add password reset by email",
            window,
            cx,
        );
        let spec_input = input("feature, e.g. user-auth", window, cx);
        let answer_input = input("your answer (Enter to send)", window, cx);

        let subscriptions = vec![cx.subscribe_in(
            &answer_input,
            window,
            |this, _, event: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    this.send_answer(window, cx);
                }
            },
        )];

        let mut app = Self {
            root,
            factory,
            runtime,
            tx,
            state: AppState::default(),
            page: Page::Build,
            config: Err("not loaded".into()),
            features: Vec::new(),
            runs: Vec::new(),
            cancel: None,
            force: false,
            docs_only: false,
            selected_file: 0,
            feature_input,
            task_input,
            spec_input,
            answer_input,
            _subscriptions: subscriptions,
        };
        app.reload();
        app
    }

    /// Re-reads `.kamili.toml`, the feature specs and the runs.
    fn reload(&mut self) {
        self.config = kamili::load_config(&self.root).map_err(|e| e.to_string());
        let specs_dir = match &self.config {
            Ok(c) => self.root.join(&c.specs_dir),
            Err(_) => self.root.join("docs/features"),
        };
        self.features = list_features(&specs_dir);
        self.runs = kamili::list_runs(&self.root);
    }

    fn on_message(&mut self, msg: bridge::ToUi) {
        let finished = matches!(msg, bridge::ToUi::Finished(_));
        let approval = matches!(msg, bridge::ToUi::Approval { .. });
        self.state.apply(msg, &self.root);
        if approval {
            self.selected_file = 0;
        }
        if finished {
            self.cancel = None;
            self.reload();
        }
    }

    fn start(&mut self, job: Job, kind: JobKind, title: String) {
        if self.state.busy() {
            return;
        }
        self.state.start_job(kind, title);
        self.page = match kind {
            JobKind::Build => Page::Build,
            JobKind::Spec => Page::Spec,
        };
        self.cancel = Some(bridge::start(
            &self.runtime,
            self.factory.clone(),
            self.root.clone(),
            job,
            self.tx.clone(),
        ));
    }

    fn start_build(&mut self, cx: &mut Context<Self>) {
        let feature = self.feature_input.read(cx).value().trim().to_string();
        let task = self.task_input.read(cx).value().trim().to_string();
        if feature.is_empty() || task.is_empty() {
            return;
        }
        let title = format!("Build {feature}: {task}");
        self.start(
            Job::Build {
                feature,
                task,
                force: self.force,
                docs_only: self.docs_only,
            },
            JobKind::Build,
            title,
        );
    }

    fn start_spec(&mut self, resume: bool, cx: &mut Context<Self>) {
        let feature = self.spec_input.read(cx).value().trim().to_string();
        if feature.is_empty() {
            return;
        }
        let title = format!("Spec {feature}");
        self.start(Job::Spec { feature, resume }, JobKind::Spec, title);
    }

    fn resume(&mut self, run: String) {
        let title = format!("Resume run {run}");
        self.start(Job::Resume { run }, JobKind::Build, title);
    }

    /// Builds stop at their next safe point; spec sessions pause.
    fn stop(&mut self) {
        match self.state.job.as_ref().map(|j| j.kind) {
            Some(JobKind::Build) => {
                if let Some(c) = &self.cancel {
                    c.cancel();
                }
                // An open approval would block the stop; rejecting is safe.
                self.state.decide(Decision::Reject);
            }
            Some(JobKind::Spec) => self.state.answer(None),
            None => {}
        }
    }

    fn send_answer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.answer_input.read(cx).value().trim().to_string();
        if text.is_empty() || self.state.question.is_none() {
            return;
        }
        self.state.answer(Some(text));
        self.answer_input
            .update(cx, |s, cx| s.set_value("", window, cx));
        cx.notify();
    }

    fn pick_feature(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        let name = name.to_string();
        self.feature_input
            .update(cx, |s, cx| s.set_value(name.clone(), window, cx));
        self.spec_input
            .update(cx, |s, cx| s.set_value(name, window, cx));
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

impl Render for KamiliApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        h_flex()
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .text_sm()
            .child(self.render_sidebar(window, cx))
            .child(
                v_flex()
                    .flex_1()
                    .h_full()
                    .min_w_0()
                    .child(self.render_header(cx))
                    .child(div().flex_1().min_h_0().p_4().child(
                        if self.state.approval.is_some() {
                            self.render_approval(cx).into_any_element()
                        } else {
                            match self.page {
                                Page::Build => self.render_build(cx).into_any_element(),
                                Page::Spec => self.render_spec(cx).into_any_element(),
                                Page::Runs => self.render_runs(cx).into_any_element(),
                                Page::Settings => self.render_settings(cx).into_any_element(),
                            }
                        },
                    )),
            )
    }
}

impl KamiliApp {
    fn render_sidebar(&self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let project = self
            .root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let nav = Page::ALL.iter().map(|(page, label)| {
            let page = *page;
            let mut b = Button::new(SharedString::from(format!("nav-{label}")))
                .label(*label)
                .w_full()
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.page = page;
                    if page == Page::Runs {
                        this.reload();
                    }
                    cx.notify();
                }));
            b = if self.page == page && self.state.approval.is_none() {
                b.primary()
            } else {
                b.ghost()
            };
            b
        });
        let features = self.features.iter().enumerate().map(|(i, f)| {
            let name = f.name.clone();
            h_flex()
                .id(("feature", i))
                .test_support()
                .justify_between()
                .gap_2()
                .px_2()
                .py_1()
                .rounded_md()
                .cursor_pointer()
                .hover(|s| s.bg(cx.theme().sidebar_accent))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.pick_feature(&name, window, cx);
                    cx.notify();
                }))
                .child(div().truncate().child(f.name.clone()))
                .child(status_pill(&f.status, cx))
        });
        v_flex()
            .w(px(240.))
            .h_full()
            .flex_none()
            .p_3()
            .gap_3()
            .bg(theme.sidebar)
            .text_color(theme.sidebar_foreground)
            .border_r_1()
            .border_color(theme.sidebar_border)
            .child(
                v_flex()
                    .child(
                        div()
                            .text_lg()
                            .font_weight(FontWeight::BOLD)
                            .child("Kamili"),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .truncate()
                            .child(project),
                    ),
            )
            .child(v_flex().gap_1().children(nav))
            .child(section_label("Feature specs", cx))
            .child(
                v_flex()
                    .id("features")
                    .flex_1()
                    .overflow_y_scroll()
                    .gap_0p5()
                    .children(features)
                    .when(self.features.is_empty(), |d| {
                        d.child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child("No specs yet. Write one in Spec."),
                        )
                    }),
            )
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (title, detail) = match &self.state.job {
            Some(job) => (
                job.title.clone(),
                match &job.finished {
                    Some(f) if f.ok => "finished".to_string(),
                    Some(f) => format!("stopped (exit {})", f.exit_code),
                    None => job.state.clone(),
                },
            ),
            None => ("No job running".to_string(), String::new()),
        };
        h_flex()
            .h(px(52.))
            .flex_none()
            .px_4()
            .gap_3()
            .justify_between()
            .border_b_1()
            .border_color(theme.border)
            .child(
                h_flex()
                    .gap_3()
                    .min_w_0()
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .truncate()
                            .child(title),
                    )
                    .child(
                        div()
                            .text_color(theme.muted_foreground)
                            .truncate()
                            .child(detail),
                    ),
            )
            .when(self.state.busy(), |d| {
                d.child(
                    Button::new("stop")
                        .label("Stop")
                        .danger()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.stop();
                            cx.notify();
                        })),
                )
            })
    }

    fn render_build(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let busy = self.state.busy();
        let form = v_flex()
            .gap_2()
            .child(
                h_flex()
                    .gap_2()
                    .child(div().w(px(220.)).child(Input::new(&self.feature_input)))
                    .child(div().flex_1().child(Input::new(&self.task_input))),
            )
            .child(
                h_flex()
                    .gap_4()
                    .child(
                        Checkbox::new("force")
                            .label("Build a draft spec")
                            .checked(self.force)
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                this.force = *checked;
                                cx.notify();
                            })),
                    )
                    .child(
                        Checkbox::new("docs-only")
                            .label("Docs only")
                            .checked(self.docs_only)
                            .on_click(cx.listener(|this, checked: &bool, _, cx| {
                                this.docs_only = *checked;
                                cx.notify();
                            })),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("start-build")
                            .label("Start build")
                            .primary()
                            .disabled(busy)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.start_build(cx);
                                cx.notify();
                            })),
                    ),
            );

        let job = self.state.job.as_ref().filter(|j| j.kind == JobKind::Build);
        let body = match job {
            None => empty_hint(
                "Pick a feature on the left, describe the task, and start a build. \
                 Nothing is written to your project until you approve it.",
                cx,
            )
            .into_any_element(),
            Some(job) => v_flex()
                .flex_1()
                .min_h_0()
                .gap_3()
                .child(steps_bar(&job.steps, cx))
                .child(
                    h_flex()
                        .flex_1()
                        .min_h_0()
                        .gap_3()
                        .items_start()
                        .child(feed_panel("build-feed", &job.feed, cx))
                        .child(tests_panel(job, cx)),
                )
                .into_any_element(),
        };
        v_flex().size_full().gap_4().child(form).child(body)
    }

    fn render_spec(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let busy = self.state.busy();
        let form = h_flex()
            .gap_2()
            .child(div().w(px(280.)).child(Input::new(&self.spec_input)))
            .child(
                Button::new("start-spec")
                    .label("New session")
                    .primary()
                    .disabled(busy)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.start_spec(false, cx);
                        cx.notify();
                    })),
            )
            .child(
                Button::new("resume-spec")
                    .label("Resume")
                    .disabled(busy)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.start_spec(true, cx);
                        cx.notify();
                    })),
            );

        let job = self.state.job.as_ref().filter(|j| j.kind == JobKind::Spec);
        let body = match job {
            None => empty_hint(
                "The Analyst interviews you one question at a time, then drafts the spec. \
                 The Architect adds the design, and you approve the result as a diff.",
                cx,
            )
            .into_any_element(),
            Some(job) => feed_panel("spec-feed", &job.feed, cx).into_any_element(),
        };

        let answer =
            self.state.question.as_ref().map(|q| {
                v_flex()
                    .gap_2()
                    .p_3()
                    .rounded_lg()
                    .border_1()
                    .border_color(cx.theme().primary)
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(q.text.clone()),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(div().flex_1().child(Input::new(&self.answer_input)))
                            .child(Button::new("send").label("Send").primary().on_click(
                                cx.listener(|this, _, window, cx| this.send_answer(window, cx)),
                            ))
                            .child(Button::new("pause").label("Pause").ghost().on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.state.answer(None);
                                    cx.notify();
                                }),
                            )),
                    )
            });

        v_flex()
            .size_full()
            .gap_4()
            .child(form)
            .child(v_flex().flex_1().min_h_0().child(body))
            .children(answer)
    }

    fn render_runs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let busy = self.state.busy();
        let theme = cx.theme();
        let rows = self.runs.iter().enumerate().map(|(i, r)| {
            let id = r.id.clone();
            h_flex()
                .gap_3()
                .px_3()
                .py_2()
                .border_b_1()
                .border_color(theme.border)
                .child(
                    div()
                        .w(px(150.))
                        .font_family("Menlo")
                        .text_xs()
                        .child(r.id.clone()),
                )
                .child(div().w(px(140.)).truncate().child(r.feature.clone()))
                .child(div().flex_1().truncate().child(r.task.clone()))
                .child(
                    div()
                        .w(px(180.))
                        .truncate()
                        .text_color(theme.muted_foreground)
                        .child(r.state.clone()),
                )
                .child(
                    div()
                        .w(px(90.))
                        .text_right()
                        .text_color(theme.muted_foreground)
                        .child(format!("{} tok", r.tokens)),
                )
                .child(div().w(px(90.)).when(r.resumable, |d| {
                    d.child(
                        Button::new(("resume", i))
                            .label("Resume")
                            .disabled(busy)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.resume(id.clone());
                                cx.notify();
                            })),
                    )
                }))
        });
        v_flex()
            .size_full()
            .gap_3()
            .child(
                h_flex()
                    .justify_between()
                    .child(section_label("Build runs, newest first", cx))
                    .child(
                        Button::new("refresh-runs")
                            .label("Refresh")
                            .ghost()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.reload();
                                cx.notify();
                            })),
                    ),
            )
            .child(if self.runs.is_empty() {
                empty_hint("No runs yet.", cx).into_any_element()
            } else {
                v_flex()
                    .id("runs")
                    .flex_1()
                    .overflow_y_scroll()
                    .rounded_lg()
                    .border_1()
                    .border_color(cx.theme().border)
                    .children(rows)
                    .into_any_element()
            })
    }

    fn render_settings(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let body = match &self.config {
            Err(e) => v_flex()
                .gap_2()
                .child(
                    div()
                        .text_color(cx.theme().danger)
                        .child("Cannot read .kamili.toml"),
                )
                .child(div().font_family("Menlo").text_xs().child(e.clone())),
            Ok(c) => {
                let p = &c.provider;
                let m = &c.models;
                let provider = match p.kind {
                    kamili::ProviderKind::Anthropic => "anthropic (Claude)",
                    kamili::ProviderKind::Openai => "openai-compatible",
                };
                let or_default = |s: &str, d: &str| {
                    if s.is_empty() {
                        d.to_string()
                    } else {
                        s.to_string()
                    }
                };
                v_flex()
                    .gap_4()
                    .child(settings_group(
                        "Provider",
                        vec![
                            ("kind", provider.to_string()),
                            (
                                "base_url",
                                or_default(&p.base_url, "https://api.anthropic.com/v1"),
                            ),
                            (
                                "api_key_env",
                                or_default(&p.api_key_env, "(kamili login credentials)"),
                            ),
                        ],
                        cx,
                    ))
                    .child(settings_group(
                        "Models",
                        vec![
                            ("analyst", m.analyst.clone()),
                            ("architect", m.architect.clone()),
                            ("planner", m.planner.clone()),
                            ("worker", m.worker.clone()),
                            ("tester", m.tester.clone()),
                            ("documentor", m.documentor.clone()),
                            ("effort", or_default(&m.effort, "(model default)")),
                        ],
                        cx,
                    ))
                    .child(settings_group(
                        "Build",
                        vec![
                            ("specs_dir", c.specs_dir.display().to_string()),
                            (
                                "test_command",
                                or_default(&c.build.test_command.join(" "), "(not set)"),
                            ),
                            ("max_retries", c.build.max_retries.to_string()),
                            ("approve_write_back", c.build.approve_write_back.to_string()),
                        ],
                        cx,
                    ))
            }
        };
        v_flex()
            .id("settings")
            .size_full()
            .overflow_y_scroll()
            .gap_4()
            .child(
                h_flex()
                    .justify_between()
                    .child(div().text_color(cx.theme().muted_foreground).child(format!(
                        "From {}. Edit the file, then reload.",
                        self.root.join(".kamili.toml").display()
                    )))
                    .child(
                        Button::new("reload")
                            .label("Reload")
                            .ghost()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.reload();
                                cx.notify();
                            })),
                    ),
            )
            .child(body)
    }

    fn render_approval(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(a) = &self.state.approval else {
            return div().into_any_element();
        };
        let theme = cx.theme();
        let selected = self.selected_file.min(a.files.len().saturating_sub(1));
        let file_list = a.files.iter().enumerate().map(|(i, f)| {
            let mark = match f.status {
                FileStatus::Added => "A",
                FileStatus::Modified => "M",
                FileStatus::Deleted => "D",
            };
            h_flex()
                .id(("file", i))
                .test_support()
                .gap_2()
                .px_2()
                .py_1()
                .rounded_md()
                .cursor_pointer()
                .when(i == selected, |d| d.bg(cx.theme().accent))
                .hover(|s| s.bg(cx.theme().accent))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.selected_file = i;
                    cx.notify();
                }))
                .child(
                    div()
                        .w(px(14.))
                        .text_color(cx.theme().muted_foreground)
                        .child(mark),
                )
                .child(div().flex_1().truncate().child(f.path.clone()))
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().success)
                        .child(format!("+{}", f.added)),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().danger)
                        .child(format!("-{}", f.removed)),
                )
        });
        let lines = match (a.files.get(selected), &a.spec_diff) {
            (Some(f), _) => f.lines.clone(),
            (None, Some(d)) => d.clone(),
            (None, None) => Vec::new(),
        };
        let diff = v_flex()
            .id("diff")
            .flex_1()
            .min_w_0()
            .overflow_y_scroll()
            .rounded_lg()
            .border_1()
            .border_color(theme.border)
            .py_1()
            .font_family("Menlo")
            .text_xs()
            .children(lines.into_iter().map(|l| diff_line(l.kind, l.text, cx)))
            .when(a.files.is_empty() && a.spec_diff.is_none(), |d| {
                d.child(div().p_3().child("No file previews for this approval."))
            });

        v_flex()
            .size_full()
            .gap_3()
            .child(
                h_flex()
                    .justify_between()
                    .gap_3()
                    .child(
                        v_flex()
                            .min_w_0()
                            .child(
                                div()
                                    .text_lg()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(a.title()),
                            )
                            .child(
                                div()
                                    .text_color(theme.muted_foreground)
                                    .child(a.summary.clone()),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(Button::new("reject").label("Reject").danger().on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.state.decide(Decision::Reject);
                                    cx.notify();
                                }),
                            ))
                            .child(Button::new("approve").label("Approve").primary().on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.state.decide(Decision::Approve);
                                    cx.notify();
                                }),
                            )),
                    ),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .gap_3()
                    .items_start()
                    .when(!a.files.is_empty(), |d| {
                        d.child(
                            v_flex()
                                .id("files")
                                .w(px(300.))
                                .h_full()
                                .flex_none()
                                .overflow_y_scroll()
                                .gap_0p5()
                                .children(file_list),
                        )
                    })
                    .child(diff),
            )
            .into_any_element()
    }
}

// ---------------------------------------------------------------------------
// Small pieces
// ---------------------------------------------------------------------------

fn section_label(text: &'static str, cx: &App) -> impl IntoElement {
    div()
        .text_xs()
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(cx.theme().muted_foreground)
        .child(text.to_uppercase())
}

fn empty_hint(text: &'static str, cx: &App) -> impl IntoElement {
    div()
        .p_6()
        .rounded_lg()
        .border_1()
        .border_dashed()
        .border_color(cx.theme().border)
        .text_color(cx.theme().muted_foreground)
        .child(text)
}

fn status_pill(status: &str, cx: &App) -> impl IntoElement {
    let color = match status {
        "approved" => cx.theme().success,
        "draft" => cx.theme().warning,
        _ => cx.theme().muted_foreground,
    };
    div()
        .flex_none()
        .px_1p5()
        .rounded_sm()
        .text_xs()
        .bg(color.opacity(0.15))
        .text_color(color)
        .child(status.to_string())
}

fn steps_bar(steps: &[StepState; 5], cx: &App) -> impl IntoElement {
    let theme = cx.theme();
    h_flex()
        .gap_2()
        .children(BUILD_STEPS.iter().zip(steps).map(|((_, label), s)| {
            let (bg, fg, mark) = match s {
                StepState::Pending => (theme.muted, theme.muted_foreground, "○"),
                StepState::Active => (theme.primary.opacity(0.15), theme.primary, "●"),
                StepState::Done => (theme.success.opacity(0.15), theme.success, "✓"),
                StepState::Failed => (theme.danger.opacity(0.15), theme.danger, "✕"),
            };
            h_flex()
                .flex_1()
                .gap_2()
                .px_3()
                .py_2()
                .rounded_md()
                .bg(bg)
                .text_color(fg)
                .child(mark)
                .child(*label)
        }))
}

fn feed_panel(id: &'static str, feed: &[FeedItem], cx: &App) -> impl IntoElement {
    let theme = cx.theme();
    v_flex()
        .id(id)
        .flex_1()
        .h_full()
        .min_w_0()
        .overflow_y_scroll()
        .p_3()
        .gap_1p5()
        .rounded_lg()
        .border_1()
        .border_color(theme.border)
        .children(feed.iter().map(|item| {
            let (label, color) = match &item.kind {
                FeedKind::Agent(a) => (a.to_string(), theme.foreground),
                FeedKind::Tool(a) => (format!("{a} ›"), theme.muted_foreground),
                FeedKind::Question => ("Analyst asks".into(), theme.primary),
                FeedKind::Answer => ("You".into(), theme.foreground),
                FeedKind::Stage => ("Stage".into(), theme.primary),
                FeedKind::Info => ("Info".into(), theme.muted_foreground),
                FeedKind::Warn => ("Warning".into(), theme.warning),
                FeedKind::Test { passed: true } => ("Tests".into(), theme.success),
                FeedKind::Test { passed: false } => ("Tests".into(), theme.danger),
            };
            let tool = matches!(item.kind, FeedKind::Tool(_));
            h_flex()
                .gap_3()
                .items_start()
                .child(
                    div()
                        .w(px(110.))
                        .flex_none()
                        .text_xs()
                        .text_color(color)
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(label),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .when(tool, |d| {
                            d.font_family("Menlo")
                                .text_xs()
                                .text_color(theme.muted_foreground)
                        })
                        .child(item.text.clone()),
                )
        }))
}

fn tests_panel(job: &crate::state::JobView, cx: &App) -> impl IntoElement {
    let theme = cx.theme();
    v_flex()
        .id("tests")
        .w(px(320.))
        .h_full()
        .flex_none()
        .overflow_y_scroll()
        .p_3()
        .gap_2()
        .rounded_lg()
        .border_1()
        .border_color(theme.border)
        .child(section_label("Test attempts", cx))
        .when(job.tests.is_empty(), |d| {
            d.child(div().text_color(theme.muted_foreground).child("None yet."))
        })
        .children(job.tests.iter().map(|t| {
            let color = if t.passed {
                theme.success
            } else {
                theme.danger
            };
            v_flex()
                .gap_1()
                .child(
                    div()
                        .text_color(color)
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(format!(
                            "Attempt {}: {}",
                            t.attempt,
                            if t.passed { "passed" } else { "failed" }
                        )),
                )
                .child(
                    div()
                        .font_family("Menlo")
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(t.summary.clone()),
                )
        }))
}

fn settings_group(
    title: &'static str,
    rows: Vec<(&'static str, String)>,
    cx: &App,
) -> impl IntoElement {
    let theme = cx.theme();
    v_flex().gap_1().child(section_label(title, cx)).child(
        v_flex()
            .rounded_lg()
            .border_1()
            .border_color(theme.border)
            .children(rows.into_iter().map(|(k, v)| {
                h_flex()
                    .px_3()
                    .py_1p5()
                    .gap_3()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .w(px(160.))
                            .text_color(theme.muted_foreground)
                            .child(k),
                    )
                    .child(div().flex_1().font_family("Menlo").text_xs().child(v))
            })),
    )
}

fn diff_line(kind: LineKind, text: String, cx: &App) -> impl IntoElement {
    let theme = cx.theme();
    let (bg, fg, sign) = match kind {
        LineKind::Added => (Some(theme.success.opacity(0.12)), theme.foreground, "+"),
        LineKind::Removed => (Some(theme.danger.opacity(0.12)), theme.foreground, "-"),
        LineKind::Hunk => (Some(theme.muted), theme.muted_foreground, ""),
        LineKind::Context => (None, theme.foreground, " "),
    };
    h_flex()
        .px_2()
        .when_some(bg, |d, bg| d.bg(bg))
        .text_color(fg)
        .child(div().w(px(14.)).flex_none().child(sign))
        .child(div().flex_1().whitespace_nowrap().child(text))
}

#[cfg(test)]
mod tests {
    //! Drives the real window headlessly: rendering, navigation, approvals and
    //! a job whose engine cannot be built.

    use std::path::Path;
    use std::time::Duration;

    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{AnyWindowHandle, TestAppContext, WindowHandle, base::Root};
    use kamili::{ApprovalKind, ApprovalRequest, Engine, Error, FileChangePreview};
    use tokio::sync::oneshot;

    use super::*;
    use crate::bridge::ToUi;
    // `gpui_kit::*` (via `super::*`) brings GPUI's `test` macro along with
    // test-support; keep `#[test]` meaning Rust's own.
    #[allow(unused_imports)]
    use ::core::prelude::v1::test;

    struct NoEngine;

    #[async_trait::async_trait]
    impl EngineFactory for NoEngine {
        async fn engine(&self, _: &Path) -> Result<Engine, Error> {
            Err(Error::Config("no engine in tests".into()))
        }
    }

    struct Fixture {
        window: WindowHandle<Root>,
        app: Entity<KamiliApp>,
        root: tempfile::TempDir,
        _runtime: tokio::runtime::Runtime,
    }

    fn open(cx: &mut TestAppContext) -> Fixture {
        cx.update(gpui_kit::init);
        let root = tempfile::tempdir().unwrap();
        let specs = root.path().join("docs/features");
        std::fs::create_dir_all(&specs).unwrap();
        std::fs::write(
            specs.join("greeting.md"),
            "---\nstatus: approved\n---\n# Hi\n",
        )
        .unwrap();
        std::fs::write(specs.join("billing.md"), "---\nstatus: draft\n---\n").unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let handle = runtime.handle().clone();
        let path = root.path().to_path_buf();
        let (window, app) = cx.update(|cx| {
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds {
                    origin: Point::default(),
                    size: size(px(1240.), px(820.)),
                })),
                ..Default::default()
            };
            let (window, app) = gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| KamiliApp::new(Arc::new(NoEngine), path, handle, window, cx))
            })
            .unwrap();
            (window.downcast::<Root>().unwrap(), app)
        });
        Fixture {
            window,
            app,
            root,
            _runtime: runtime,
        }
    }

    fn with_window<R>(
        f: &Fixture,
        cx: &mut TestAppContext,
        then: impl FnOnce(&mut Window, &mut App) -> R,
    ) -> R {
        let handle: AnyWindowHandle = f.window.into();
        cx.update_window(handle, |_, window, cx| then(window, cx))
            .unwrap()
    }

    #[gpui_kit::test]
    fn lists_specs_and_renders_every_page(cx: &mut TestAppContext) {
        let f = open(cx);
        let names: Vec<_> = f.app.read_with(cx, |app, _| {
            app.features
                .iter()
                .map(|e| (e.name.clone(), e.status.clone()))
                .collect()
        });
        assert_eq!(
            names,
            [
                ("billing".to_string(), "draft".to_string()),
                ("greeting".to_string(), "approved".to_string())
            ]
        );
        with_window(&f, cx, |window, cx| {
            window.render_frame(cx);
            assert!(window.try_find(("feature", 1usize)).is_some());
            for page in ["nav-Spec", "nav-Runs", "nav-Settings", "nav-Build"] {
                window.click(page, cx);
                window.render_frame(cx);
            }
            assert!(window.try_find("start-build").is_some());
        });
        assert_eq!(f.app.read_with(cx, |app, _| app.page), Page::Build);
        assert!(f.root.path().exists());
    }

    #[gpui_kit::test]
    fn clicking_a_spec_fills_both_feature_fields(cx: &mut TestAppContext) {
        let f = open(cx);
        with_window(&f, cx, |window, cx| {
            window.render_frame(cx);
            window.click(("feature", 1usize), cx);
        });
        let (build, spec) = f.app.read_with(cx, |app, cx| {
            (
                app.feature_input.read(cx).value().to_string(),
                app.spec_input.read(cx).value().to_string(),
            )
        });
        assert_eq!((build.as_str(), spec.as_str()), ("greeting", "greeting"));
    }

    fn approval(root: &Path) -> ApprovalRequest {
        ApprovalRequest {
            kind: ApprovalKind::WriteBack,
            summary: "write 2 files".into(),
            diff: None,
            changes: vec![
                FileChangePreview {
                    path: root.join("src/lib.rs"),
                    old_text: Some("fn a() {}\n".into()),
                    new_text: Some("fn a() {}\nfn b() {}\n".into()),
                },
                FileChangePreview {
                    path: root.join("src/new.rs"),
                    old_text: None,
                    new_text: Some("pub fn n() {}\n".into()),
                },
            ],
        }
    }

    fn open_approval(f: &Fixture, cx: &mut TestAppContext) -> oneshot::Receiver<Decision> {
        let (reply, rx) = oneshot::channel();
        let request = approval(f.root.path());
        f.app.update(cx, |app, cx| {
            app.state.start_job(JobKind::Build, "Build greeting: test");
            app.on_message(ToUi::Approval { request, reply });
            cx.notify();
        });
        rx
    }

    #[gpui_kit::test]
    fn approve_button_sends_approve(cx: &mut TestAppContext) {
        let f = open(cx);
        let mut rx = open_approval(&f, cx);
        with_window(&f, cx, |window, cx| {
            window.render_frame(cx);
            // Both files are listed; the second can be selected.
            assert!(window.try_find(("file", 0usize)).is_some());
            window.click(("file", 1usize), cx);
            window.click("approve", cx);
        });
        assert_eq!(rx.try_recv().unwrap(), Decision::Approve);
        assert!(f.app.read_with(cx, |app, _| app.state.approval.is_none()));
    }

    #[gpui_kit::test]
    fn reject_button_sends_reject(cx: &mut TestAppContext) {
        let f = open(cx);
        let mut rx = open_approval(&f, cx);
        with_window(&f, cx, |window, cx| {
            window.render_frame(cx);
            window.click("reject", cx);
        });
        assert_eq!(rx.try_recv().unwrap(), Decision::Reject);
    }

    #[gpui_kit::test]
    fn failed_engine_start_is_reported(cx: &mut TestAppContext) {
        let f = open(cx);
        with_window(&f, cx, |window, cx| {
            f.app.update(cx, |app, cx| {
                app.feature_input
                    .update(cx, |s, cx| s.set_value("greeting", window, cx));
                app.task_input
                    .update(cx, |s, cx| s.set_value("say hello", window, cx));
            });
            window.render_frame(cx);
            window.click("start-build", cx);
        });
        assert!(f.app.read_with(cx, |app, _| app.state.job.is_some()));
        // The job runs on the tokio runtime; its result comes back through
        // the UI channel.
        let mut finished = None;
        for _ in 0..200 {
            cx.run_until_parked();
            finished = f.app.read_with(cx, |app, _| {
                app.state.job.as_ref().and_then(|j| j.finished.clone())
            });
            if finished.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let finished = finished.expect("job finished");
        assert!(!finished.ok);
        assert_eq!(finished.exit_code, 2);
        assert!(finished.message.contains("no engine in tests"));
        assert!(!f.app.read_with(cx, |app, _| app.state.busy()));
    }
}
