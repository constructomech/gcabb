//! The automations dialog: saved automations, their draft editor, and run history.
//!
//! All of the dialog's state lives in [`AutomationsPanel`] so `SessionMvpView`
//! carries one field instead of seventeen. The rendering and command-dispatch
//! methods stay on `SessionMvpView` because they reach across into the rest of
//! the view (projects, models, the service channel, the error banner).

use std::path::Path;

use app_model::{Automation, AutomationRun, AutomationRunStatus, AutomationSchedule};
use gpui::prelude::*;
use gpui::{
    Anchor, AnchoredPositionMode, Context, Entity, Role, SharedString, anchored, deferred, div,
    point, px, relative, rgb,
};

use crate::{
    AUTOMATION_FORM_SCROLL_ID, AUTOMATION_HISTORY_SCROLL_ID, AUTOMATION_LIST_SCROLL_ID, BACKGROUND,
    BLUE, BORDER, ELEVATED, GREEN, MUTED, PANEL, PRIMARY, RED, SUBTLE, ServiceCommand,
    SessionMvpView, TextInput, automation_runner, context_window_label, default_context_tier,
    effort_label, title_case,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AutomationsTab {
    Saved,
    History,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AutomationMenu {
    Model,
    Agent,
    Project,
    Mode,
    Effort,
    Context,
}

/// Every piece of state the automations dialog owns.
pub struct AutomationsPanel {
    /// Whether the dialog is showing.
    pub open: bool,
    pub tab: AutomationsTab,
    /// Saved automations, as last reported by the service.
    pub automations: Vec<Automation>,
    /// Recent runs across all automations, newest first.
    pub runs: Vec<AutomationRun>,
    /// The automation the draft fields below are editing, if it already exists.
    pub editing: Option<String>,
    pub name_input: Entity<TextInput>,
    pub schedule_input: Entity<TextInput>,
    pub condition_input: Entity<TextInput>,
    pub instructions_input: Entity<TextInput>,
    pub draft_model: Option<String>,
    pub draft_agent: Option<String>,
    pub draft_mode: String,
    pub draft_effort: Option<String>,
    pub draft_context_tier: Option<String>,
    pub draft_project: Option<String>,
    pub draft_enabled: bool,
    /// The dropdown currently expanded in the draft editor, if any.
    pub open_menu: Option<AutomationMenu>,
}

impl AutomationsPanel {
    pub fn new(cx: &mut Context<SessionMvpView>) -> Self {
        let name_input = cx.new(|cx| TextInput::new(cx, "automation-name", "Automation name"));
        let schedule_input =
            cx.new(|cx| TextInput::new(cx, "automation-schedule", "Every Wednesday at 2:00 PM"));
        let condition_input = cx.new(|cx| {
            TextInput::new(
                cx,
                "automation-condition",
                "Optional condition, for example: the build is failing",
            )
        });
        let instructions_input = cx.new(|cx| {
            TextInput::new(
                cx,
                "automation-instructions",
                "What should Copilot do when the condition is true?",
            )
        });
        for input in [
            &name_input,
            &schedule_input,
            &condition_input,
            &instructions_input,
        ] {
            cx.observe(input, |_, _, cx| cx.notify()).detach();
        }

        Self {
            open: false,
            tab: AutomationsTab::Saved,
            automations: Vec::new(),
            runs: Vec::new(),
            editing: None,
            name_input,
            schedule_input,
            condition_input,
            instructions_input,
            draft_model: None,
            draft_agent: None,
            draft_mode: "autopilot".to_owned(),
            draft_effort: Some("medium".to_owned()),
            draft_context_tier: None,
            draft_project: None,
            draft_enabled: true,
            open_menu: None,
        }
    }
}

impl SessionMvpView {
    pub fn open_automations(&mut self, cx: &mut Context<Self>) {
        self.automations_panel.open = true;
        self.automations_panel.tab = AutomationsTab::Saved;
        if let Some(automation) = self.automations_panel.automations.first().cloned() {
            self.edit_automation(automation, cx);
        } else {
            self.begin_new_automation(cx);
        }
        cx.notify();
    }

    pub fn close_automations(&mut self, cx: &mut Context<Self>) {
        self.automations_panel.open = false;
        self.automations_panel.open_menu = None;
        self.action_error = None;
        cx.notify();
    }

    pub fn begin_new_automation(&mut self, cx: &mut Context<Self>) {
        self.automations_panel.editing = None;
        self.automations_panel.open_menu = None;
        for input in [
            &self.automations_panel.name_input,
            &self.automations_panel.schedule_input,
            &self.automations_panel.condition_input,
            &self.automations_panel.instructions_input,
        ] {
            input.update(cx, TextInput::clear);
        }
        self.automations_panel.draft_model = None;
        self.automations_panel.draft_agent = None;
        self.automations_panel.draft_mode = self
            .mode_options()
            .into_iter()
            .find(|(mode, _, _)| mode == "autopilot")
            .or_else(|| self.mode_options().into_iter().next())
            .map_or_else(|| "autopilot".to_owned(), |(mode, _, _)| mode);
        self.automations_panel.draft_effort = Some("medium".to_owned());
        self.automations_panel.draft_context_tier = None;
        self.automations_panel.draft_project = None;
        self.automations_panel.draft_enabled = true;
        self.action_error = None;
        cx.notify();
    }

    pub fn edit_automation(&mut self, automation: Automation, cx: &mut Context<Self>) {
        self.automations_panel.editing = Some(automation.id);
        self.automations_panel.open_menu = None;
        for (input, value) in [
            (&self.automations_panel.name_input, automation.name),
            (
                &self.automations_panel.schedule_input,
                automation.schedule_description,
            ),
            (
                &self.automations_panel.condition_input,
                automation.condition.unwrap_or_default(),
            ),
            (
                &self.automations_panel.instructions_input,
                automation.instructions,
            ),
        ] {
            input.update(cx, |input, cx| input.set_value(value, cx));
        }
        self.automations_panel.draft_model = automation.model;
        self.automations_panel.draft_agent = automation.agent;
        self.automations_panel.draft_mode = automation.mode;
        self.automations_panel.draft_effort = automation.reasoning_effort;
        self.automations_panel.draft_context_tier = automation.context_tier;
        self.automations_panel.draft_project = automation.project_path;
        self.automations_panel.draft_enabled = automation.enabled;
        self.action_error = None;
        cx.notify();
    }

    pub fn save_automation(&mut self, cx: &mut Context<Self>) {
        let name = self.automations_panel.name_input.read(cx).value();
        let name = name.trim();
        if name.is_empty() {
            self.action_error = Some("Automation name is required.".to_owned());
            return;
        }
        let schedule_description = self.automations_panel.schedule_input.read(cx).value();
        let schedule_description = schedule_description.trim();
        let schedule = match schedule_description.parse::<AutomationSchedule>() {
            Ok(schedule) => schedule,
            Err(error) => {
                self.action_error = Some(error.to_string());
                return;
            }
        };
        let instructions = self.automations_panel.instructions_input.read(cx).value();
        let instructions = instructions.trim();
        if instructions.is_empty() {
            self.action_error = Some("Automation instructions are required.".to_owned());
            return;
        }
        let condition = self.automations_panel.condition_input.read(cx).value();
        let condition = (!condition.trim().is_empty()).then(|| condition.trim().to_owned());
        let existing = self.automations_panel.editing.as_deref().and_then(|id| {
            self.automations_panel
                .automations
                .iter()
                .find(|automation| automation.id == id)
        });
        let now = automation_runner::timestamp();
        let automation = Automation {
            id: existing.map_or_else(
                || uuid::Uuid::new_v4().to_string(),
                |automation| automation.id.clone(),
            ),
            name: name.to_owned(),
            schedule_description: schedule_description.to_owned(),
            schedule,
            condition,
            instructions: instructions.to_owned(),
            model: self.automations_panel.draft_model.clone(),
            agent: self.automations_panel.draft_agent.clone(),
            mode: self.automations_panel.draft_mode.clone(),
            reasoning_effort: self.automations_panel.draft_effort.clone(),
            context_tier: self.automations_panel.draft_context_tier.clone(),
            project_path: self.automations_panel.draft_project.clone(),
            enabled: self.automations_panel.draft_enabled,
            next_run_at: None,
            last_run_at: existing.and_then(|automation| automation.last_run_at.clone()),
            created_at: existing
                .map_or_else(|| now.clone(), |automation| automation.created_at.clone()),
            updated_at: now,
        };
        self.automations_panel.editing = Some(automation.id.clone());
        self.action_error = None;
        if self
            .commands
            .send(ServiceCommand::SaveAutomation(automation))
            .is_err()
        {
            self.action_error = Some("Automation service is unavailable.".to_owned());
        }
        cx.notify();
    }

    pub fn delete_current_automation(&mut self, cx: &mut Context<Self>) {
        let Some(automation_id) = self.automations_panel.editing.clone() else {
            return;
        };
        if self
            .commands
            .send(ServiceCommand::DeleteAutomation { automation_id })
            .is_err()
        {
            self.action_error = Some("Automation service is unavailable.".to_owned());
            return;
        }
        self.begin_new_automation(cx);
    }

    pub fn run_current_automation(&mut self) {
        let Some(automation_id) = self.automations_panel.editing.clone() else {
            return;
        };
        if self
            .commands
            .send(ServiceCommand::RunAutomationNow { automation_id })
            .is_err()
        {
            self.action_error = Some("Automation service is unavailable.".to_owned());
        } else {
            self.automations_panel.tab = AutomationsTab::History;
        }
    }

    pub fn toggle_automation_menu(&mut self, menu: AutomationMenu) {
        self.automations_panel.open_menu =
            (self.automations_panel.open_menu != Some(menu)).then_some(menu);
    }

    #[allow(clippy::too_many_lines)]
    pub fn automation_menu_options(
        &self,
        menu: AutomationMenu,
    ) -> (&'static str, String, Vec<(String, String, String)>) {
        match menu {
            AutomationMenu::Model => {
                let mut options = vec![(
                    String::new(),
                    "Default model".to_owned(),
                    "Use the provider's default model".to_owned(),
                )];
                options.extend(self.model_options());
                (
                    "Model",
                    self.automations_panel
                        .draft_model
                        .clone()
                        .unwrap_or_default(),
                    options,
                )
            }
            AutomationMenu::Agent => (
                "Agent",
                self.automations_panel
                    .draft_agent
                    .clone()
                    .unwrap_or_default(),
                self.agent_options(),
            ),
            AutomationMenu::Project => {
                let mut options = vec![(
                    String::new(),
                    "No workspace".to_owned(),
                    "Run without repository context".to_owned(),
                )];
                options.extend(
                    self.projects
                        .iter()
                        .filter(|project| Path::new(&project.path).is_dir())
                        .map(|project| {
                            (
                                project.path.clone(),
                                project.name.clone(),
                                project.path.clone(),
                            )
                        }),
                );
                (
                    "Workspace",
                    self.automations_panel
                        .draft_project
                        .clone()
                        .unwrap_or_default(),
                    options,
                )
            }
            AutomationMenu::Mode => {
                let mut options = self.mode_options();
                if options.is_empty() {
                    options = ["autopilot", "interactive", "plan"]
                        .into_iter()
                        .map(|mode| (mode.to_owned(), title_case(mode), String::new()))
                        .collect();
                }
                ("Mode", self.automations_panel.draft_mode.clone(), options)
            }
            AutomationMenu::Effort => {
                let mut options = vec![(
                    String::new(),
                    "Default reasoning".to_owned(),
                    "Use the model's default reasoning effort".to_owned(),
                )];
                if let Some(model) = self.automations_panel.draft_model.as_deref() {
                    options.extend(self.supported_reasoning_efforts(model).into_iter().map(
                        |effort| {
                            let label = effort_label(&effort);
                            (effort, label, String::new())
                        },
                    ));
                }
                (
                    "Reasoning effort",
                    self.automations_panel
                        .draft_effort
                        .clone()
                        .unwrap_or_default(),
                    options,
                )
            }
            AutomationMenu::Context => {
                let mut options = vec![(
                    String::new(),
                    "Default context".to_owned(),
                    "Use the model's default context window".to_owned(),
                )];
                if let Some(model) = self.automations_panel.draft_model.as_deref() {
                    options.extend(self.context_windows(model).into_iter().map(|window| {
                        let label = context_window_label(&window);
                        (window.tier, label, String::new())
                    }));
                }
                (
                    "Context length",
                    self.automations_panel
                        .draft_context_tier
                        .clone()
                        .unwrap_or_default(),
                    options,
                )
            }
        }
    }

    pub fn choose_automation_option(&mut self, menu: AutomationMenu, value: String) {
        match menu {
            AutomationMenu::Model => {
                self.automations_panel.draft_model = (!value.is_empty()).then_some(value);
                if let Some(model) = self.automations_panel.draft_model.as_deref() {
                    let efforts = self.supported_reasoning_efforts(model);
                    self.automations_panel.draft_effort = efforts
                        .iter()
                        .find(|effort| effort.as_str() == "medium")
                        .or_else(|| efforts.first())
                        .cloned();
                    self.automations_panel.draft_context_tier =
                        default_context_tier(&self.context_windows(model));
                } else {
                    self.automations_panel.draft_effort = None;
                    self.automations_panel.draft_context_tier = None;
                }
            }
            AutomationMenu::Agent => {
                self.automations_panel.draft_agent = (!value.is_empty()).then_some(value);
            }
            AutomationMenu::Project => {
                self.automations_panel.draft_project = (!value.is_empty()).then_some(value);
            }
            AutomationMenu::Mode => self.automations_panel.draft_mode = value,
            AutomationMenu::Effort => {
                self.automations_panel.draft_effort = (!value.is_empty()).then_some(value);
            }
            AutomationMenu::Context => {
                self.automations_panel.draft_context_tier = (!value.is_empty()).then_some(value);
            }
        }
        self.automations_panel.open_menu = None;
    }

    #[allow(clippy::too_many_lines)]
    pub fn automation_choice_control(
        &self,
        menu: AutomationMenu,
        id: &'static str,
        label: String,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let expanded = self.automations_panel.open_menu == Some(menu);
        div()
            .relative()
            .flex()
            .flex_col()
            .min_w_0()
            .child(automation_choice_button(id, label, expanded, menu, cx))
            .when(expanded, |control| {
                let (title, selected, options) = self.automation_menu_options(menu);
                let menu_above = !matches!(menu, AutomationMenu::Model | AutomationMenu::Agent);
                let anchor = if menu_above {
                    Anchor::BottomLeft
                } else {
                    Anchor::TopLeft
                };
                let position = if menu_above {
                    point(px(0.0), px(-42.0))
                } else {
                    point(px(0.0), px(42.0))
                };
                control.child(deferred(
                    anchored()
                        .anchor(anchor)
                        .position(position)
                        .position_mode(AnchoredPositionMode::Local)
                        .snap_to_window_with_margin(px(12.0))
                        .child(
                            div()
                                .id(SharedString::from(format!("{id}-menu")))
                                .debug_selector(move || format!("{id}-menu"))
                                .occlude()
                                .role(Role::ListBox)
                                .aria_label(title)
                                .w_full()
                                .min_w(px(260.0))
                                .max_h(px(260.0))
                                .overflow_y_scroll()
                                .p_1()
                                .rounded_md()
                                .border_1()
                                .border_color(rgb(BORDER))
                                .bg(rgb(PANEL))
                                .shadow_lg()
                                .children(options.into_iter().enumerate().map(
                                    |(index, (value, label, description))| {
                                        let selected = value == selected;
                                        let option_value = value.clone();
                                        let has_description = !description.is_empty();
                                        let option_selector = format!("{id}-option-{index}");
                                        div()
                                            .id((id, index))
                                            .debug_selector(move || option_selector.clone())
                                            .role(Role::ListBoxOption)
                                            .aria_label(label.clone())
                                            .aria_selected(selected)
                                            .when(has_description, |option| {
                                                option.aria_description(description.clone())
                                            })
                                            .focusable()
                                            .tab_stop(true)
                                            .flex()
                                            .items_center()
                                            .gap_2()
                                            .px_2()
                                            .py_2()
                                            .rounded_md()
                                            .bg(rgb(if selected { ELEVATED } else { PANEL }))
                                            .hover(|style| style.bg(rgb(ELEVATED)).cursor_pointer())
                                            .on_click(cx.listener(move |view, _, _, cx| {
                                                view.choose_automation_option(
                                                    menu,
                                                    option_value.clone(),
                                                );
                                                cx.notify();
                                            }))
                                            .child(
                                                div()
                                                    .w(px(16.0))
                                                    .flex_shrink_0()
                                                    .text_color(rgb(BLUE))
                                                    .child(if selected { "\u{2713}" } else { "" }),
                                            )
                                            .child(
                                                div()
                                                    .min_w_0()
                                                    .flex()
                                                    .flex_col()
                                                    .child(label)
                                                    .when(has_description, |content| {
                                                        content.child(
                                                            div()
                                                                .text_xs()
                                                                .text_color(rgb(MUTED))
                                                                .child(description),
                                                        )
                                                    }),
                                            )
                                    },
                                )),
                        ),
                ))
            })
    }

    #[allow(clippy::too_many_lines)]
    pub fn automations_dialog(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        if !self.automations_panel.open {
            return None;
        }

        let (list_scroll, form_scroll, history_scroll) = {
            let mut scrolls = self.detail_scrolls.borrow_mut();
            (
                scrolls
                    .entry(AUTOMATION_LIST_SCROLL_ID.to_owned())
                    .or_default()
                    .clone(),
                scrolls
                    .entry(AUTOMATION_FORM_SCROLL_ID.to_owned())
                    .or_default()
                    .clone(),
                scrolls
                    .entry(AUTOMATION_HISTORY_SCROLL_ID.to_owned())
                    .or_default()
                    .clone(),
            )
        };
        let list_group = SharedString::from("automation-list-scroll-group");
        let form_group = SharedString::from("automation-form-scroll-group");
        let history_group = SharedString::from("automation-history-scroll-group");
        let list_scrollbar = Self::visible_scrollbar(
            AUTOMATION_LIST_SCROLL_ID,
            &list_scroll,
            list_group.clone(),
            cx,
        );
        let form_scrollbar = Self::visible_scrollbar(
            AUTOMATION_FORM_SCROLL_ID,
            &form_scroll,
            form_group.clone(),
            cx,
        );
        let history_scrollbar = Self::visible_scrollbar(
            AUTOMATION_HISTORY_SCROLL_ID,
            &history_scroll,
            history_group.clone(),
            cx,
        );

        let automation_rows = self
            .automations_panel
            .automations
            .iter()
            .cloned()
            .map(|automation| {
                let selected = self.automations_panel.editing.as_deref() == Some(&automation.id);
                let status = if automation.enabled {
                    automation
                        .next_run_at
                        .as_deref()
                        .map_or_else(|| "Enabled".to_owned(), |next| format!("Next: {next}"))
                } else {
                    "Disabled".to_owned()
                };
                let automation_for_click = automation.clone();
                div()
                    .id(SharedString::from(format!(
                        "automation-row-{}",
                        automation.id
                    )))
                    .role(Role::Button)
                    .aria_label(format!("Edit {}", automation.name))
                    .focusable()
                    .tab_stop(true)
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .px_3()
                    .py_2()
                    .rounded_md()
                    .bg(rgb(if selected { ELEVATED } else { PANEL }))
                    .border_1()
                    .border_color(rgb(if selected { BLUE } else { BORDER }))
                    .hover(|style| style.bg(rgb(ELEVATED)).cursor_pointer())
                    .on_click(cx.listener(move |view, _, _, cx| {
                        view.edit_automation(automation_for_click.clone(), cx);
                    }))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .w(px(7.0))
                                    .h(px(7.0))
                                    .rounded_full()
                                    .bg(rgb(if automation.enabled { GREEN } else { MUTED })),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .child(automation.name),
                            ),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(MUTED))
                            .child(automation.schedule_description),
                    )
                    .child(div().text_xs().text_color(rgb(MUTED)).child(status))
                    .into_any_element()
            })
            .collect::<Vec<_>>();
        let model_name = self.automations_panel.draft_model.as_ref().map_or_else(
            || "Default".to_owned(),
            |model| {
                self.model_options()
                    .into_iter()
                    .find(|(id, _, _)| id == model)
                    .map_or_else(|| model.clone(), |(_, name, _)| name)
            },
        );
        let agent_name = self.automations_panel.draft_agent.as_ref().map_or_else(
            || "Default".to_owned(),
            |agent| {
                self.agent_options()
                    .into_iter()
                    .find(|(id, _, _)| id == agent)
                    .map_or_else(|| agent.clone(), |(_, name, _)| name)
            },
        );
        let project_name = self.automations_panel.draft_project.as_ref().map_or_else(
            || "None".to_owned(),
            |path| {
                self.projects
                    .iter()
                    .find(|project| &project.path == path)
                    .map_or_else(|| path.clone(), |project| project.name.clone())
            },
        );

        let saved_body = div()
            .flex()
            .flex_1()
            .min_h_0()
            .child(
                div()
                    .w(px(270.0))
                    .h_full()
                    .flex_shrink_0()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .pr_4()
                    .border_r_1()
                    .border_color(rgb(BORDER))
                    .child(automation_dialog_button(
                        "automation-new",
                        "+ New automation",
                        ELEVATED,
                        cx,
                        SessionMvpView::begin_new_automation,
                    ))
                    .child(
                        div()
                            .id("automation-list-scroll-frame")
                            .group(list_group)
                            .relative()
                            .flex_1()
                            .min_h_0()
                            .child(
                                div()
                                    .id(AUTOMATION_LIST_SCROLL_ID)
                                    .track_scroll(&list_scroll)
                                    .flex()
                                    .flex_col()
                                    .gap_2()
                                    .size_full()
                                    .pr_3()
                                    .overflow_y_scroll()
                                    .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
                                    .children(automation_rows)
                                    .when(self.automations_panel.automations.is_empty(), |list| {
                                        list.child(
                                            div()
                                                .px_2()
                                                .py_4()
                                                .text_sm()
                                                .text_color(rgb(MUTED))
                                                .child("No automations saved yet."),
                                        )
                                    }),
                            )
                            .children(list_scrollbar),
                    ),
            )
            .child(
                div()
                    .id("automation-form-scroll-frame")
                    .group(form_group)
                    .relative()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .child(
                        div()
                            .id(AUTOMATION_FORM_SCROLL_ID)
                            .track_scroll(&form_scroll)
                            .flex()
                            .flex_col()
                            .gap_3()
                            .size_full()
                            .pl_4()
                            .pr_5()
                            .pb_1()
                            .overflow_y_scroll()
                            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .text_lg()
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .child(if self.automations_panel.editing.is_some() {
                                        "Edit automation"
                                    } else {
                                        "New automation"
                                    }),
                            )
                            .child(automation_input_field(
                                "Name",
                                self.automations_panel.name_input.clone(),
                                None,
                            ))
                            .child(automation_input_field(
                                "Schedule",
                                self.automations_panel.schedule_input.clone(),
                                None,
                            ))
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .text_xs()
                                    .text_color(rgb(MUTED))
                                    .child(
                                        "Uses local time and 5-minute increments. Supports \
                                         intervals, weekdays, named days, monthly, and yearly \
                                         schedules.",
                                    ),
                            )
                            .child(automation_input_field(
                                "Condition (optional)",
                                self.automations_panel.condition_input.clone(),
                                None,
                            ))
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .text_xs()
                                    .text_color(rgb(MUTED))
                                    .child(
                                        "Copilot evaluates this as true or false before running \
                                         the action.",
                                    ),
                            )
                            .child(automation_input_field(
                                "Instructions",
                                self.automations_panel.instructions_input.clone(),
                                Some(84.0),
                            ))
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .grid()
                                    .grid_cols(2)
                                    .gap_2()
                                    .child(self.automation_choice_control(
                                        AutomationMenu::Model,
                                        "automation-model",
                                        format!("Model: {model_name}"),
                                        cx,
                                    ))
                                    .child(self.automation_choice_control(
                                        AutomationMenu::Agent,
                                        "automation-agent",
                                        format!("Agent: {agent_name}"),
                                        cx,
                                    ))
                                    .child(self.automation_choice_control(
                                        AutomationMenu::Project,
                                        "automation-project",
                                        format!("Workspace: {project_name}"),
                                        cx,
                                    ))
                                    .child(self.automation_choice_control(
                                        AutomationMenu::Mode,
                                        "automation-mode",
                                        format!(
                                            "Mode: {}",
                                            title_case(&self.automations_panel.draft_mode)
                                        ),
                                        cx,
                                    ))
                                    .child(self.automation_choice_control(
                                        AutomationMenu::Effort,
                                        "automation-effort",
                                        format!(
                                            "Reasoning: {}",
                                            self.automations_panel.draft_effort
                                                .as_deref()
                                                .map_or("Default".to_owned(), effort_label)
                                        ),
                                        cx,
                                    ))
                                    .child(self.automation_choice_control(
                                        AutomationMenu::Context,
                                        "automation-context",
                                        format!(
                                            "Context: {}",
                                            self.automations_panel.draft_context_tier
                                                .as_deref()
                                                .unwrap_or("Default")
                                        ),
                                        cx,
                                    )),
                            )
                            .child(
                                div()
                                    .id("automation-enabled")
                                    .role(Role::CheckBox)
                                    .aria_label("Enable automation")
                                    .aria_selected(self.automations_panel.draft_enabled)
                                    .focusable()
                                    .tab_stop(true)
                                    .flex_shrink_0()
                                    .flex()
                                    .items_center()
                                    .gap_2()
                                    .hover(gpui::Styled::cursor_pointer)
                                    .on_click(cx.listener(|view, _, _, cx| {
                                        view.automations_panel.draft_enabled =
                                            !view.automations_panel.draft_enabled;
                                        cx.notify();
                                    }))
                                    .child(
                                        div()
                                            .w(px(18.0))
                                            .h(px(18.0))
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .rounded_sm()
                                            .border_1()
                                            .border_color(rgb(BORDER))
                                            .bg(rgb(if self.automations_panel.draft_enabled {
                                                BLUE
                                            } else {
                                                SUBTLE
                                            }))
                                            .child(if self.automations_panel.draft_enabled {
                                                "✓"
                                            } else {
                                                ""
                                            }),
                                    )
                                    .child("Enabled"),
                            )
                            .when_some(self.action_error.clone(), |form, error| {
                                form.child(div().text_sm().text_color(rgb(RED)).child(error))
                            })
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .flex()
                                    .items_center()
                                    .justify_between()
                                    .gap_2()
                                    .child(div().flex().gap_2().when(
                                        self.automations_panel.editing.is_some(),
                                        |buttons| {
                                            buttons
                                                .child(automation_dialog_button(
                                                    "automation-delete",
                                                    "Delete",
                                                    RED,
                                                    cx,
                                                    |view, cx| {
                                                        view.delete_current_automation(cx);
                                                    },
                                                ))
                                                .child(automation_dialog_button(
                                                    "automation-run-now",
                                                    "Run now",
                                                    GREEN,
                                                    cx,
                                                    |view, _| {
                                                        view.run_current_automation();
                                                    },
                                                ))
                                        },
                                    ))
                                    .child(automation_dialog_button(
                                        "automation-save",
                                        "Save automation",
                                        BLUE,
                                        cx,
                                        SessionMvpView::save_automation,
                                    )),
                            )
                            .child(div().h(px(1.0)).w_full().flex_shrink_0()),
                    )
                    .children(form_scrollbar),
            )
            .into_any_element();

        let history_rows = self
            .automations_panel
            .runs
            .iter()
            .map(|run| {
                let color = match run.status {
                    AutomationRunStatus::Running | AutomationRunStatus::Succeeded => GREEN,
                    AutomationRunStatus::Skipped => MUTED,
                    AutomationRunStatus::Failed => RED,
                };
                let detail = run
                    .error
                    .as_ref()
                    .or(run.output.as_ref())
                    .cloned()
                    .unwrap_or_else(|| {
                        run.condition_result.map_or_else(
                            || "Run in progress…".to_owned(),
                            |result| format!("Condition evaluated to {result}."),
                        )
                    });
                div()
                    .id(SharedString::from(format!("automation-run-{}", run.id)))
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_3()
                    .rounded_md()
                    .border_1()
                    .border_color(rgb(BORDER))
                    .bg(rgb(SUBTLE))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(div().w(px(8.0)).h(px(8.0)).rounded_full().bg(rgb(color)))
                            .child(
                                div()
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .child(run.automation_name.clone()),
                            )
                            .child(
                                div()
                                    .ml_auto()
                                    .text_xs()
                                    .text_color(rgb(color))
                                    .child(title_case(run.status.as_str())),
                            ),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(MUTED))
                            .child(format!("Started {}", run.started_at)),
                    )
                    .child(div().text_sm().child(detail))
                    .into_any_element()
            })
            .collect::<Vec<_>>();
        let history_body = div()
            .id("automation-history-scroll-frame")
            .group(history_group)
            .relative()
            .flex_1()
            .min_h_0()
            .child(
                div()
                    .id(AUTOMATION_HISTORY_SCROLL_ID)
                    .track_scroll(&history_scroll)
                    .flex()
                    .flex_col()
                    .gap_3()
                    .size_full()
                    .pr_3()
                    .overflow_y_scroll()
                    .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
                    .children(history_rows)
                    .when(self.automations_panel.runs.is_empty(), |history| {
                        history.child(
                            div()
                                .py_8()
                                .text_center()
                                .text_color(rgb(MUTED))
                                .child("Automation runs will appear here."),
                        )
                    }),
            )
            .children(history_scrollbar)
            .into_any_element();

        let body = match self.automations_panel.tab {
            AutomationsTab::Saved => saved_body,
            AutomationsTab::History => history_body,
        };

        Some(
            div()
                .id("automations-dialog")
                .accessibility_id("automations-dialog")
                .role(Role::Dialog)
                .aria_label("Automations")
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .p_4()
                .bg(gpui::rgba(0x0000_00a8))
                .child(
                    div()
                        .w(px(960.0))
                        .h(px(720.0))
                        .max_w(relative(0.96))
                        .max_h(relative(0.94))
                        .flex()
                        .flex_col()
                        .gap_4()
                        .p_5()
                        .rounded_lg()
                        .bg(rgb(PANEL))
                        .border_1()
                        .border_color(rgb(BORDER))
                        .shadow_lg()
                        .child(
                            div()
                                .flex_shrink_0()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(
                                    div()
                                        .text_xl()
                                        .font_weight(gpui::FontWeight::BOLD)
                                        .child("Automations"),
                                )
                                .child(automation_tab_button(
                                    "automation-tab-saved",
                                    "Saved",
                                    self.automations_panel.tab == AutomationsTab::Saved,
                                    cx,
                                    AutomationsTab::Saved,
                                ))
                                .child(automation_tab_button(
                                    "automation-tab-history",
                                    "Run history",
                                    self.automations_panel.tab == AutomationsTab::History,
                                    cx,
                                    AutomationsTab::History,
                                ))
                                .child(
                                    div()
                                        .id("automations-close")
                                        .role(Role::Button)
                                        .aria_label("Close automations")
                                        .focusable()
                                        .tab_stop(true)
                                        .ml_auto()
                                        .px_3()
                                        .py_2()
                                        .rounded_md()
                                        .bg(rgb(ELEVATED))
                                        .child("Close")
                                        .hover(|style| style.opacity(0.85).cursor_pointer())
                                        .on_click(cx.listener(|view, _, _, cx| {
                                            view.close_automations(cx);
                                        })),
                                ),
                        )
                        .child(body),
                ),
        )
    }
}

fn automation_input_field(
    label: &'static str,
    input: Entity<TextInput>,
    min_height: Option<f32>,
) -> impl IntoElement {
    div()
        .flex_shrink_0()
        .flex()
        .flex_col()
        .gap_1()
        .child(div().text_xs().text_color(rgb(MUTED)).child(label))
        .child(
            div()
                .when_some(min_height, |field, height| field.min_h(px(height)))
                .border_1()
                .border_color(rgb(BORDER))
                .rounded_md()
                .bg(rgb(BACKGROUND))
                .child(input),
        )
}

fn automation_choice_button(
    id: &'static str,
    label: String,
    expanded: bool,
    menu: AutomationMenu,
    cx: &mut Context<SessionMvpView>,
) -> impl IntoElement {
    div()
        .id(id)
        .debug_selector(move || id.to_owned())
        .role(Role::Button)
        .aria_label(label.clone())
        .aria_expanded(expanded)
        .focusable()
        .tab_stop(true)
        .focus_visible(|style| style.border_color(rgb(BLUE)))
        .h(px(40.0))
        .flex_shrink_0()
        .flex()
        .items_center()
        .gap_2()
        .px_3()
        .rounded_md()
        .border_1()
        .border_color(rgb(BORDER))
        .bg(rgb(SUBTLE))
        .text_sm()
        .child(div().min_w_0().flex_1().truncate().child(label))
        .child(
            div()
                .flex_shrink_0()
                .text_color(rgb(MUTED))
                .child(if expanded { "\u{25b4}" } else { "\u{25be}" }),
        )
        .hover(|style| style.bg(rgb(ELEVATED)).cursor_pointer())
        .on_click(cx.listener(move |view, _, _, cx| {
            view.toggle_automation_menu(menu);
            cx.notify();
        }))
}

fn automation_dialog_button(
    id: &'static str,
    label: &'static str,
    color: u32,
    cx: &mut Context<SessionMvpView>,
    action: impl Fn(&mut SessionMvpView, &mut Context<SessionMvpView>) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .role(Role::Button)
        .aria_label(label)
        .focusable()
        .tab_stop(true)
        .focus_visible(|style| style.border_1().border_color(rgb(BLUE)))
        .px_3()
        .py_2()
        .rounded_md()
        .bg(rgb(color))
        .text_sm()
        .text_color(rgb(if color == ELEVATED {
            PRIMARY
        } else {
            BACKGROUND
        }))
        .child(label)
        .hover(|style| style.opacity(0.85).cursor_pointer())
        .on_click(cx.listener(move |view, _, _, cx| {
            action(view, cx);
            cx.notify();
        }))
}

fn automation_tab_button(
    id: &'static str,
    label: &'static str,
    selected: bool,
    cx: &mut Context<SessionMvpView>,
    tab: AutomationsTab,
) -> impl IntoElement {
    div()
        .id(id)
        .role(Role::Tab)
        .aria_label(label)
        .aria_selected(selected)
        .focusable()
        .tab_stop(true)
        .px_3()
        .py_2()
        .rounded_md()
        .bg(rgb(if selected { BLUE } else { ELEVATED }))
        .text_color(rgb(if selected { BACKGROUND } else { PRIMARY }))
        .child(label)
        .hover(|style| style.opacity(0.85).cursor_pointer())
        .on_click(cx.listener(move |view, _, _, cx| {
            view.automations_panel.tab = tab;
            cx.notify();
        }))
}

#[cfg(test)]
mod tests {
    use gpui::{Modifiers, TestAppContext};

    use crate::tests::interaction::setup;
    use crate::{ServiceCommand, SessionMvpView};

    #[gpui::test]
    fn saving_an_automation_sends_a_valid_definition(cx: &mut TestAppContext) {
        let (view, cx, commands) = setup(cx);
        view.update(cx, |view, cx| {
            view.open_automations(cx);
            view.automations_panel.name_input.update(cx, |input, cx| {
                input.set_value("Review pull requests", cx);
            });
            view.automations_panel
                .schedule_input
                .update(cx, |input, cx| {
                    input.set_value("Every Wednesday at 2:00 PM", cx);
                });
            view.automations_panel
                .condition_input
                .update(cx, |input, cx| {
                    input.set_value("there are open pull requests", cx);
                });
            view.automations_panel
                .instructions_input
                .update(cx, |input, cx| {
                    input.set_value("Summarize the open pull requests.", cx);
                });
            view.save_automation(cx);
        });

        let ServiceCommand::SaveAutomation(automation) = commands.try_recv().unwrap() else {
            panic!("expected save automation command");
        };
        assert_eq!(automation.name, "Review pull requests");
        assert_eq!(
            automation.condition.as_deref(),
            Some("there are open pull requests")
        );
        assert_eq!(
            automation.schedule,
            app_model::AutomationSchedule::Weekly {
                weekdays: vec![app_model::ScheduleWeekday::Wednesday],
                minute_of_day: 14 * 60,
            }
        );
        assert!(automation.enabled);
    }

    #[gpui::test]
    fn automation_configuration_controls_keep_readable_height(cx: &mut TestAppContext) {
        let (view, cx, _commands) = setup(cx);
        view.update(cx, SessionMvpView::open_automations);
        cx.run_until_parked();

        for selector in [
            "automation-model",
            "automation-agent",
            "automation-project",
            "automation-mode",
            "automation-effort",
            "automation-context",
        ] {
            let bounds = cx
                .debug_bounds(selector)
                .unwrap_or_else(|| panic!("{selector} was not rendered"));
            assert!(
                f32::from(bounds.size.height) >= 39.0,
                "{selector} was compressed to {bounds:?}"
            );
        }
    }

    #[gpui::test]
    fn automation_selectors_open_listboxes_and_scroll_overflow(cx: &mut TestAppContext) {
        let (view, cx, _commands) = setup(cx);
        view.update(cx, SessionMvpView::open_automations);
        cx.run_until_parked();

        let mode = cx
            .debug_bounds("automation-mode")
            .expect("mode selector rendered");
        cx.simulate_click(mode.center(), Modifiers::none());
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("automation-mode-menu").is_some(),
            "mode selector opens an actual listbox"
        );
        let interactive = cx
            .debug_bounds("automation-mode-option-1")
            .expect("interactive mode option rendered");
        cx.simulate_click(interactive.center(), Modifiers::none());
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert_eq!(view.automations_panel.draft_mode, "interactive");
            assert!(view.automations_panel.open_menu.is_none());
        });

        let model = cx
            .debug_bounds("automation-model")
            .expect("model selector rendered");
        cx.simulate_click(model.center(), Modifiers::none());
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            let handle = view
                .scroll_handle(super::super::AUTOMATION_FORM_SCROLL_ID)
                .expect("automation form tracks its scroll position");
            assert!(
                f32::from(handle.max_offset().y) > 0.0,
                "expanded dropdown makes overflow scrollable"
            );
        });
        assert!(
            cx.debug_bounds("automation-form-scroll-visible-scrollbar")
                .is_some(),
            "overflow shows a persistent styled scrollbar"
        );
    }

    #[gpui::test]
    fn every_automation_selector_opens_and_closes_its_listbox(cx: &mut TestAppContext) {
        let (view, cx, _commands) = setup(cx);
        view.update(cx, SessionMvpView::open_automations);
        cx.run_until_parked();

        for (selector, menu_selector) in [
            ("automation-model", "automation-model-menu"),
            ("automation-agent", "automation-agent-menu"),
            ("automation-project", "automation-project-menu"),
            ("automation-mode", "automation-mode-menu"),
            ("automation-effort", "automation-effort-menu"),
            ("automation-context", "automation-context-menu"),
        ] {
            let trigger = cx
                .debug_bounds(selector)
                .unwrap_or_else(|| panic!("{selector} trigger was not rendered"));
            cx.simulate_click(trigger.center(), Modifiers::none());
            cx.run_until_parked();
            assert!(
                cx.debug_bounds(menu_selector).is_some(),
                "{selector} did not open a floating listbox"
            );

            cx.simulate_click(trigger.center(), Modifiers::none());
            cx.run_until_parked();
            assert!(
                cx.debug_bounds(menu_selector).is_none(),
                "{selector} did not close its listbox"
            );
        }
    }

    #[gpui::test]
    fn automation_form_validation_blocks_invalid_saves(cx: &mut TestAppContext) {
        let (view, cx, commands) = setup(cx);
        view.update(cx, |view, cx| {
            view.open_automations(cx);
            view.save_automation(cx);
            assert_eq!(
                view.action_error.as_deref(),
                Some("Automation name is required.")
            );

            view.automations_panel
                .name_input
                .update(cx, |input, cx| input.set_value("Maintenance", cx));
            view.automations_panel
                .schedule_input
                .update(cx, |input, cx| input.set_value("every 3 minutes", cx));
            view.save_automation(cx);
            assert!(
                view.action_error
                    .as_deref()
                    .is_some_and(|error| error.contains("5-minute increments"))
            );

            view.automations_panel
                .schedule_input
                .update(cx, |input, cx| {
                    input.set_value("Every Wednesday at 2:00 PM", cx);
                });
            view.save_automation(cx);
            assert_eq!(
                view.action_error.as_deref(),
                Some("Automation instructions are required.")
            );
        });
        assert!(
            commands.try_recv().is_err(),
            "invalid drafts must not reach the automation service"
        );
    }
}
