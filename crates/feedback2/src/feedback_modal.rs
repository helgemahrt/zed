use std::{ops::RangeInclusive, sync::Arc};

use anyhow::bail;
use client::{Client, ZED_SECRET_CLIENT_TOKEN, ZED_SERVER_URL};
use db::kvp::KEY_VALUE_STORE;
use editor::{Editor, EditorEvent};
use futures::AsyncReadExt;
use gpui::{
    div, red, rems, serde_json, AppContext, DismissEvent, Div, EventEmitter, FocusHandle,
    FocusableView, Model, PromptLevel, Render, Task, View, ViewContext,
};
use isahc::Request;
use language::Buffer;
use project::Project;
use regex::Regex;
use serde_derive::Serialize;
use ui::{prelude::*, Button, ButtonStyle, Label, Tooltip};
use util::ResultExt;
use workspace::Workspace;

use crate::{system_specs::SystemSpecs, GiveFeedback, OpenZedCommunityRepo};

const DATABASE_KEY_NAME: &str = "email_address";
const EMAIL_REGEX: &str = r"\b[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Z|a-z]{2,}\b";
const FEEDBACK_CHAR_LIMIT: RangeInclusive<usize> = 10..=5000;
const FEEDBACK_SUBMISSION_ERROR_TEXT: &str =
    "Feedback failed to submit, see error log for details.";

#[derive(Serialize)]
struct FeedbackRequestBody<'a> {
    feedback_text: &'a str,
    email: Option<String>,
    metrics_id: Option<Arc<str>>,
    installation_id: Option<Arc<str>>,
    system_specs: SystemSpecs,
    is_staff: bool,
    token: &'a str,
}

pub struct FeedbackModal {
    system_specs: SystemSpecs,
    feedback_editor: View<Editor>,
    email_address_editor: View<Editor>,
    project: Model<Project>,
    character_count: usize,
    pending_submission: bool,
}

impl FocusableView for FeedbackModal {
    fn focus_handle(&self, cx: &AppContext) -> FocusHandle {
        self.feedback_editor.focus_handle(cx)
    }
}
impl EventEmitter<DismissEvent> for FeedbackModal {}

impl FeedbackModal {
    pub fn register(workspace: &mut Workspace, cx: &mut ViewContext<Workspace>) {
        let _handle = cx.view().downgrade();
        workspace.register_action(move |workspace, _: &GiveFeedback, cx| {
            let markdown = workspace
                .app_state()
                .languages
                .language_for_name("Markdown");

            let project = workspace.project().clone();

            cx.spawn(|workspace, mut cx| async move {
                let markdown = markdown.await.log_err();
                let buffer = project
                    .update(&mut cx, |project, cx| {
                        project.create_buffer("", markdown, cx)
                    })?
                    .expect("creating buffers on a local workspace always succeeds");

                workspace.update(&mut cx, |workspace, cx| {
                    let system_specs = SystemSpecs::new(cx);

                    workspace.toggle_modal(cx, move |cx| {
                        FeedbackModal::new(system_specs, project, buffer, cx)
                    });
                })?;

                anyhow::Ok(())
            })
            .detach_and_log_err(cx);
        });
    }

    pub fn new(
        system_specs: SystemSpecs,
        project: Model<Project>,
        buffer: Model<Buffer>,
        cx: &mut ViewContext<Self>,
    ) -> Self {
        let email_address_editor = cx.build_view(|cx| {
            let mut editor = Editor::single_line(cx);
            editor.set_placeholder_text("Email address (optional)", cx);

            if let Ok(Some(email_address)) = KEY_VALUE_STORE.read_kvp(DATABASE_KEY_NAME) {
                editor.set_text(email_address, cx)
            }

            editor
        });

        let feedback_editor = cx.build_view(|cx| {
            let mut editor = Editor::for_buffer(buffer, Some(project.clone()), cx);
            editor.set_vertical_scroll_margin(5, cx);
            editor
        });

        cx.subscribe(
            &feedback_editor,
            |this, editor, event: &EditorEvent, cx| match event {
                EditorEvent::Edited => {
                    this.character_count = editor
                        .read(cx)
                        .buffer()
                        .read(cx)
                        .as_singleton()
                        .expect("Feedback editor is never a multi-buffer")
                        .read(cx)
                        .len();
                    cx.notify();
                }
                _ => {}
            },
        )
        .detach();

        Self {
            system_specs: system_specs.clone(),
            feedback_editor,
            email_address_editor,
            project,
            pending_submission: false,
            character_count: 0,
        }
    }

    pub fn submit(&mut self, cx: &mut ViewContext<Self>) -> Task<anyhow::Result<()>> {
        let feedback_text = self.feedback_editor.read(cx).text(cx).trim().to_string();
        let email = self.email_address_editor.read(cx).text_option(cx);

        let answer = cx.prompt(
            PromptLevel::Info,
            "Ready to submit your feedback?",
            &["Yes, Submit!", "No"],
        );
        let client = cx.global::<Arc<Client>>().clone();
        let specs = self.system_specs.clone();
        cx.spawn(|this, mut cx| async move {
            let answer = answer.await.ok();
            if answer == Some(0) {
                if let Some(email) = email.clone() {
                    let _ = KEY_VALUE_STORE
                        .write_kvp(DATABASE_KEY_NAME.to_string(), email)
                        .await;
                }

                this.update(&mut cx, |feedback_editor, cx| {
                    feedback_editor.set_pending_submission(true, cx);
                })
                .log_err();
                match FeedbackModal::submit_feedback(&feedback_text, email, client, specs).await {
                    Ok(_) => {}
                    Err(error) => {
                        log::error!("{}", error);
                        this.update(&mut cx, |feedback_editor, cx| {
                            let prompt = cx.prompt(
                                PromptLevel::Critical,
                                FEEDBACK_SUBMISSION_ERROR_TEXT,
                                &["OK"],
                            );
                            cx.spawn(|_, _cx| async move {
                                prompt.await.ok();
                            })
                            .detach();
                            feedback_editor.set_pending_submission(false, cx);
                        })
                        .log_err();
                    }
                }
            }
        })
        .detach();
        Task::ready(Ok(()))
    }

    fn set_pending_submission(&mut self, pending_submission: bool, cx: &mut ViewContext<Self>) {
        self.pending_submission = pending_submission;
        cx.notify();
    }

    async fn submit_feedback(
        feedback_text: &str,
        email: Option<String>,
        zed_client: Arc<Client>,
        system_specs: SystemSpecs,
    ) -> anyhow::Result<()> {
        let feedback_endpoint = format!("{}/api/feedback", *ZED_SERVER_URL);
        let telemetry = zed_client.telemetry();
        let metrics_id = telemetry.metrics_id();
        let installation_id = telemetry.installation_id();
        let is_staff = telemetry.is_staff();
        let http_client = zed_client.http_client();
        let request = FeedbackRequestBody {
            feedback_text: &feedback_text,
            email,
            metrics_id,
            installation_id,
            system_specs,
            is_staff: is_staff.unwrap_or(false),
            token: ZED_SECRET_CLIENT_TOKEN,
        };
        let json_bytes = serde_json::to_vec(&request)?;
        let request = Request::post(feedback_endpoint)
            .header("content-type", "application/json")
            .body(json_bytes.into())?;
        let mut response = http_client.send(request).await?;
        let mut body = String::new();
        response.body_mut().read_to_string(&mut body).await?;
        let response_status = response.status();
        if !response_status.is_success() {
            bail!("Feedback API failed with error: {}", response_status)
        }
        Ok(())
    }
}

impl Render for FeedbackModal {
    type Element = Div;

    fn render(&mut self, cx: &mut ViewContext<Self>) -> Self::Element {
        let valid_email_address = match self.email_address_editor.read(cx).text_option(cx) {
            Some(email_address) => Regex::new(EMAIL_REGEX).unwrap().is_match(&email_address),
            None => true,
        };

        let allow_submission = FEEDBACK_CHAR_LIMIT.contains(&self.character_count)
            && valid_email_address
            && !self.pending_submission;

        let dismiss = cx.listener(|_, _, cx| cx.emit(DismissEvent));
        // let open_community_issues =
        //     cx.listener(|_, _, cx| cx.dispatch_action(Box::new(OpenZedCommunityRepo)));
        // let open_community_discussions = cx.listener(|_, _, cx| cx.emit(DismissEvent));

        v_stack()
            .elevation_3(cx)
            .min_w(rems(40.))
            .max_w(rems(96.))
            .h(rems(40.))
            .p_2()
            .gap_2()
            .child(
                v_stack().child(
                    div()
                        .size_full()
                        .border()
                        .border_color(red())
                        .child(Label::new("Give Feedback").color(Color::Default))
                        .child(Label::new("This editor supports markdown").color(Color::Muted)),
                ),
            )
            .child(
                div()
                    .flex_1()
                    .bg(cx.theme().colors().editor_background)
                    .border()
                    .border_color(cx.theme().colors().border)
                    .child(self.feedback_editor.clone()),
            )
            .child(
                div().border().border_color(red()).child(
                    Label::new(format!(
                        "{} / {} Characters",
                        self.character_count,
                        FEEDBACK_CHAR_LIMIT.end()
                    ))
                    .color(Color::Default),
                ),
            )
            .child(                div()
                .bg(cx.theme().colors().editor_background)
                .border()
                .border_color(cx.theme().colors().border)
                .child(self.email_address_editor.clone())
            )
            .child(
                h_stack()
                    .justify_between()
                    .gap_1()
                    .child(Button::new("community_repo", "Community Repo")
                        .style(ButtonStyle::Filled)
                        .color(Color::Muted)
                        // .on_click(cx.dispatch_action(Box::new(OpenZedCommunityRepo)))
                    )
                    .child(h_stack().justify_between().gap_1()
                        .child(
                            Button::new("cancel_feedback", "Cancel")
                                .style(ButtonStyle::Subtle)
                                .color(Color::Muted)
                                .on_click(dismiss),
                        )
                        .child(
                            Button::new("send_feedback", "Send Feedback")
                                .color(Color::Accent)
                                .style(ButtonStyle::Filled)
                                // TODO - error handling - show modal on error
                                .on_click(cx.listener(|this, _, cx| {let _ = this.submit(cx);}))
                                .tooltip(|cx| {
                                    Tooltip::with_meta(
                                        "Submit feedback to the Zed team.",
                                        None,
                                        "Provide an email address if you want us to be able to reply.",
                                        cx,
                                    )
                                })
                                .when(!allow_submission, |this| this.disabled(true)),
                        ),
                    )

            )
    }
}
