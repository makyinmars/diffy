use quark::{Px, view};
use quark_app::ViewContext;
use quark_components::{BadgeVariant, Button, ButtonSize, ButtonStyle, badge};
use quark_ui::accessibility::AccessibilityRole;
use quark_ui::element::*;
use quark_ui::icons::lucide;
use quark_ui::style::Styled;
use quark_ui::theme::Theme;

use super::state::{Blocked, GuideController, Status};
use crate::app::shell::Slot;
use crate::app::tokens::{self, Ico, Rad, Sp, TextStyle};
use crate::contracts::commands::{CommandsAction, SettingsSection};
use crate::contracts::guide::{Guide, GuideAction, GuideStep, StepKind};

const FILES_SHOWN: usize = 4;
const MECHANICAL_SHOWN: usize = 24;
const SHORT_SHA: usize = 7;

fn button(label: &str, action: impl Into<quark_ui::Action>, style: ButtonStyle) -> Button {
    Button::new(action)
        .label(label.to_owned())
        .style(style)
        .size(ButtonSize::Compact)
}

fn icon_button(icon: &'static str, tooltip: &str, action: GuideAction) -> Button {
    Button::new(action)
        .icon(icon)
        .tooltip(tooltip.to_owned())
        .style(ButtonStyle::Ghost)
        .size(ButtonSize::Compact)
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(SHORT_SHA)]
}

struct Panel<'a> {
    theme: &'a Theme,
    scale: f32,
    width: f32,
}

impl Panel<'_> {
    fn small(&self) -> f32 {
        TextStyle::BodySmall.font_size(self.theme)
    }

    /// A bordered card with a title, a detail line and optional buttons.
    fn notice(
        &self,
        role: AccessibilityRole,
        test_id: &str,
        icon: &'static str,
        title: &str,
        detail: &str,
        buttons: Vec<AnyElement>,
    ) -> AnyElement {
        let scale = self.scale;
        let tc = &self.theme.colors;
        let small = self.small();
        let tone = match role {
            AccessibilityRole::Alert => tc.status_error,
            _ => tc.icon,
        };
        view! { scale,
            <div class="flex-col shrink-0" w={Px(self.width)} p={Sp::MD} gap={Sp::SM}
                 rounded={Rad::LG} bg={tc.elevated_surface} border={tc.border_variant}
                 accessibility_role={role} aria-label={title.to_owned()} test_id={test_id.to_owned()}>
                <div class="flex-row items-center w-full" gap={Sp::SM}>
                    <icon svg={icon} size={Ico::SM} color={tone} />
                    <text size={Px(small)} class="font-medium" color={tc.text_strong}>{title.to_owned()}</text>
                </div>
                <text size={Px(small)} color={tc.text_muted}>{detail.to_owned()}</text>
                if !buttons.is_empty() {
                    <div class="flex-row flex-wrap items-center w-full" gap={Sp::SM}>{...buttons}</div>
                }
            </div>
        }
    }

    fn wrap(&self, test_id: &str, button: Button) -> AnyElement {
        view! { <div class="shrink-0" test_id={test_id.to_owned()}>{button}</div> }
    }

    fn stale(&self, guide: &Guide, head: Option<&str>) -> AnyElement {
        let detail = match head {
            Some(head) => format!(
                "This guide is for {}. The diff is now at {}.",
                short(&guide.head),
                short(head)
            ),
            None => format!("This guide is for {}.", short(&guide.head)),
        };
        let regenerate = self.wrap(
            "guide.regenerate",
            button("Regenerate", GuideAction::Generate, ButtonStyle::Subtle).icon(lucide::REFRESH),
        );
        self.notice(
            AccessibilityRole::Status,
            "guide.stale",
            crate::app::icons::CLOCK,
            "Out of date",
            &detail,
            vec![regenerate],
        )
    }

    fn step(&self, index: usize, step: &GuideStep, selected: Option<usize>) -> AnyElement {
        let scale = self.scale;
        let tc = &self.theme.colors;
        let small = self.small();
        let is_selected = selected.is_some();
        let number = match step.kind {
            StepKind::Mechanical => "\u{2026}".to_owned(),
            _ => (index + 1).to_string(),
        };
        let variant = if is_selected {
            BadgeVariant::Accent
        } else {
            BadgeVariant::Default
        };
        let shown = match step.kind {
            StepKind::Mechanical => MECHANICAL_SHOWN,
            _ => FILES_SHOWN,
        };
        let mut files: Vec<AnyElement> = step
            .files
            .iter()
            .take(shown)
            .map(|file| {
                let label = match &file.note {
                    Some(note) => format!("{} \u{b7} {note}", file.path),
                    None => file.path.clone(),
                };
                view! { scale,
                    <text size={Px(small)} class="truncate font-mono" color={tc.text_muted}>{label}</text>
                }
            })
            .collect();
        if step.files.len() > shown {
            files.push(view! { scale,
                <text size={Px(small)} color={tc.text_muted}>{format!("{} more files", step.files.len() - shown)}</text>
            });
        }
        let position = selected.map(|hunk| {
            let label = if step.anchors.len() == 1 {
                "1 place".to_owned()
            } else {
                format!("{} of {}", hunk + 1, step.anchors.len())
            };
            view! { scale,
                <div class="shrink-0" test_id="guide.position">
                    <text size={Px(small)} class="whitespace-nowrap" color={tc.text_accent}>{label}</text>
                </div>
            }
        });
        let bg = if is_selected {
            tc.ghost_element_selected
        } else {
            tc.elevated_surface
        };
        let border = if is_selected {
            tc.accent
        } else {
            tc.border_variant
        };
        let label = format!("Step {}: {}", index + 1, step.title);
        let names: Vec<String> = step
            .files
            .iter()
            .map(|file| match &file.note {
                Some(note) => format!("{} ({note})", file.path),
                None => file.path.clone(),
            })
            .collect();
        let mut description = format!("{} Files: {}.", step.body, names.join(", "));
        if let Some(hunk) = selected {
            description.push_str(&format!(" Place {} of {}.", hunk + 1, step.anchors.len()));
        }
        view! { scale,
            <div class="flex-col shrink-0" w={Px(self.width)} p={Sp::MD} gap={Sp::XS}
                 rounded={Rad::LG} bg={bg} border={border} hover_bg={tc.ghost_element_hover}
                 cursor={CursorHint::Pointer}
                 accessibility_role={AccessibilityRole::ListBoxOption}
                 aria-label={label}
                 aria-description={description}
                 aria-selected={is_selected}
                 test_id={format!("guide.step.{index}")}
                 on:click={GuideAction::SelectStep(index)}>
                <div class="flex-row items-center w-full" gap={Sp::SM}>
                    <div class="shrink-0">{badge(number).variant(variant)}</div>
                    <div class="flex-1 flex-col" min_w={0.0}>
                        <text size={Px(TextStyle::Body.font_size(self.theme))} class="font-medium" color={tc.text_strong}>{step.title.clone()}</text>
                    </div>
                    {?position}
                </div>
                if !step.body.is_empty() {
                    <text size={Px(small)} color={tc.text}>{step.body.clone()}</text>
                }
                <div class="flex-col w-full" gap={Sp::XXS}>{...files}</div>
            </div>
        }
    }

    fn guide(&self, guide: &Guide, selection: (usize, usize)) -> Vec<AnyElement> {
        let scale = self.scale;
        let tc = &self.theme.colors;
        let mut out = Vec::new();
        if !guide.overview.is_empty() {
            out.push(view! { scale,
                <div class="flex-col shrink-0" w={Px(self.width)}
                     accessibility_role={AccessibilityRole::Group} aria-label="Overview" test_id="guide.overview">
                    <text size={Px(TextStyle::Body.font_size(self.theme))} color={tc.text}>{guide.overview.clone()}</text>
                </div>
            });
        }
        let steps: Vec<AnyElement> = guide
            .steps
            .iter()
            .enumerate()
            .map(|(index, step)| {
                let selected = (index == selection.0).then_some(selection.1);
                self.step(index, step, selected)
            })
            .collect();
        out.push(view! { scale,
            <div class="flex-col shrink-0" w={Px(self.width)} gap={Sp::SM}
                 accessibility_role={AccessibilityRole::ListBox} aria-label="Steps" test_id="guide.steps">
                {...steps}
            </div>
        });
        out
    }
}

pub fn panel(guide: &GuideController, slot: &Slot, cx: &mut ViewContext) -> AnyElement {
    let theme = cx.theme;
    let tc = &theme.colors;
    let scale = tokens::ui_scale(theme);
    let pad = tokens::scaled(Sp::MD, scale);
    let page = Panel {
        theme,
        scale,
        width: (slot.width - pad * 2.0).max(1.0),
    };
    let small = page.small();
    let current = guide.guide();
    let mut sections: Vec<AnyElement> = Vec::new();
    match guide.status() {
        Status::Generating { bytes, steps, .. } => {
            let detail = if *steps > 0 {
                format!(
                    "{steps} steps drafted, {} KB received.",
                    bytes.div_ceil(1024)
                )
            } else {
                "Reading the change and asking the model. This can take a minute on a big pull request.".to_owned()
            };
            let cancel = page.wrap(
                "guide.cancel",
                button("Cancel", GuideAction::Cancel, ButtonStyle::Subtle),
            );
            sections.push(page.notice(
                AccessibilityRole::Status,
                "guide.generating",
                lucide::LOADER,
                "Writing the guide\u{2026}",
                &detail,
                vec![cancel],
            ));
        }
        Status::Unavailable(reason) => {
            let settings = page.wrap(
                "guide.settings",
                button(
                    "Open AI settings",
                    CommandsAction::SetSettingsSection(SettingsSection::Clankers),
                    ButtonStyle::Filled,
                )
                .icon(lucide::SETTINGS),
            );
            let retry = page.wrap(
                "guide.retry",
                button("Try again", GuideAction::Generate, ButtonStyle::Subtle),
            );
            sections.push(page.notice(
                AccessibilityRole::Alert,
                "guide.unavailable",
                lucide::ALERT_CIRCLE,
                "Guided review needs an AI backend",
                reason,
                vec![settings, retry],
            ));
        }
        Status::Failed(message) => {
            let retry = page.wrap(
                "guide.retry",
                button("Try again", GuideAction::Generate, ButtonStyle::Subtle),
            );
            sections.push(page.notice(
                AccessibilityRole::Alert,
                "guide.failed",
                lucide::ALERT_CIRCLE,
                "Couldn\u{2019}t build a guide",
                message,
                vec![retry],
            ));
        }
        Status::Idle => {}
    }
    match (guide.blocked(), current) {
        (Some(Blocked::WorkingTree), _) => sections.push(page.notice(
            AccessibilityRole::Status,
            "guide.unsupported",
            lucide::INFO,
            "Guides need a commit range",
            "Working-tree and staged changes have no commit to pin a guide to. Compare two refs or open a pull request.",
            Vec::new(),
        )),
        (Some(Blocked::Waiting), None) => sections.push(view! { scale,
            <div class="flex-row shrink-0" test_id="guide.waiting">
                <text size={Px(small)} color={tc.text_muted}>"Waiting for the diff\u{2026}"</text>
            </div>
        }),
        (_, Some(current)) if !guide.generating() => {
            if guide.stale() {
                sections.push(page.stale(current, guide.current_head()));
            }
            sections.extend(page.guide(current, guide.selection()));
        }
        (None, None) if *guide.status() == Status::Idle => {
            let generate = page.wrap(
                "guide.generate",
                button("Generate guide", GuideAction::Generate, ButtonStyle::Filled)
                    .icon(lucide::SPARKLES),
            );
            sections.push(page.notice(
                AccessibilityRole::Group,
                "guide.empty",
                lucide::SPARKLES,
                "Guide this change",
                "A model reads the diff and splits it into steps you can walk through, hunk by hunk. It uses your backend from Settings \u{2192} AI and posts nothing to the forge.",
                vec![generate],
            ));
        }
        _ => {}
    }

    let model = current
        .filter(|_| !guide.generating())
        .map(|current| current.model.clone());
    let regenerate =
        (current.is_some() && !guide.generating() && guide.blocked().is_none()).then(|| {
            page.wrap(
                "guide.header.regenerate",
                icon_button(lucide::REFRESH, "Regenerate guide", GuideAction::Generate),
            )
        });
    let close = page.wrap(
        "guide.close",
        icon_button(lucide::X, "Hide guide (i)", GuideAction::Close),
    );
    view! { scale,
        <div w={Px(slot.width)} h={Px(slot.height)} class="flex-col shrink-0"
             bg={tc.sidebar_background} border_l={tc.border_variant}
             accessibility_role={AccessibilityRole::Complementary} aria-label="Guide" test_id="guide">
            <div class="flex-row items-center w-full shrink-0" px={Sp::MD} py={Sp::SM} gap={Sp::SM}
                 border_b={tc.border_variant} test_id="guide.header">
                <icon svg={lucide::SPARKLES} size={Ico::SM} color={tc.icon} />
                <text size={Px(TextStyle::Body.font_size(theme))} class="font-semibold" color={tc.text_strong}>"Guide"</text>
                if let Some(model) = model {
                    <div class="flex-1 flex-row" min_w={0.0} test_id="guide.model">
                        <text size={Px(small)} class="truncate" color={tc.text_muted}>{model}</text>
                    </div>
                } else {
                    <spacer />
                }
                {?regenerate}
                {close}
            </div>
            <div class="flex-1 flex-col overflow-y-scroll scrollbar-auto-hide" min_h={0.0} track_scroll={&guide.scroll}>
                <div class="flex-col w-full shrink-0" p={Sp::MD} gap={Sp::MD}>
                    {...sections}
                </div>
            </div>
        </div>
    }
}
