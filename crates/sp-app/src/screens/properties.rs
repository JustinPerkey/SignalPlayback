//! The Properties screen (`docs/DESIGN.md` §6.3, §12.1): the list of property
//! definitions and a form to declare a new one.

use std::fmt;

use iced::widget::{
    button, checkbox, column, container, pick_list, row, scrollable, text, text_input, Column,
    Space,
};
use iced::{Alignment, Element, Length, Task};
use sp_core::pulse::normalise_key;
use sp_core::{PropKind, PropScope, PropertyDef};
use sp_store::{props, Store};

use crate::jobs;
use crate::typography;
use crate::ui;

/// The kinds a user can pick in the form; each maps onto a [`PropKind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KindChoice {
    #[default]
    Float,
    Int,
    Bool,
    Text,
    Enum,
    FreqHz,
    DurationS,
    TimeUtc,
    Ratio,
    SignalRef,
}

impl KindChoice {
    pub const ALL: [Self; 10] = [
        Self::Float,
        Self::Int,
        Self::Bool,
        Self::Text,
        Self::Enum,
        Self::FreqHz,
        Self::DurationS,
        Self::TimeUtc,
        Self::Ratio,
        Self::SignalRef,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Float => "Number",
            Self::Int => "Integer",
            Self::Bool => "Yes / no",
            Self::Text => "Text",
            Self::Enum => "Choice",
            Self::FreqHz => "Frequency",
            Self::DurationS => "Duration",
            Self::TimeUtc => "Timestamp",
            Self::Ratio => "Ratio",
            Self::SignalRef => "Signal reference",
        }
    }

    fn to_kind(self, variants: &str) -> PropKind {
        match self {
            Self::Float => PropKind::Float {
                min: None,
                max: None,
                step: None,
            },
            Self::Int => PropKind::Int {
                min: None,
                max: None,
            },
            Self::Bool => PropKind::Bool,
            Self::Text => PropKind::Text {
                pattern: None,
                max_len: None,
            },
            Self::Enum => PropKind::Enum {
                variants: variants
                    .split(',')
                    .map(str::trim)
                    .filter(|v| !v.is_empty())
                    .map(str::to_owned)
                    .collect(),
            },
            Self::FreqHz => PropKind::FreqHz {
                min: None,
                max: None,
            },
            Self::DurationS => PropKind::DurationS,
            Self::TimeUtc => PropKind::TimeUtc,
            Self::Ratio => PropKind::Ratio { as_db: false },
            Self::SignalRef => PropKind::SignalRef,
        }
    }
}

impl fmt::Display for KindChoice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// `PropScope` with a display label for the picker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScopeChoice(pub PropScope);

impl ScopeChoice {
    pub const ALL: [Self; 3] = [
        Self(PropScope::Signal),
        Self(PropScope::Group),
        Self(PropScope::Dataset),
    ];
}

impl fmt::Display for ScopeChoice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self.0 {
            PropScope::Signal => "Signal",
            PropScope::Group => "Group",
            PropScope::Dataset => "Dataset",
        })
    }
}

/// The new-definition form.
#[derive(Debug, Clone)]
pub struct Form {
    pub key: String,
    pub label: String,
    pub scope: PropScope,
    pub kind: KindChoice,
    pub unit: String,
    pub section: String,
    pub variants: String,
    pub required: bool,
}

impl Default for Form {
    fn default() -> Self {
        Self {
            key: String::new(),
            label: String::new(),
            scope: PropScope::Signal,
            kind: KindChoice::default(),
            unit: String::new(),
            section: String::new(),
            variants: String::new(),
            required: false,
        }
    }
}

impl Form {
    /// The definition the form describes. The key falls back to the label,
    /// normalised, so typing a label alone is enough.
    pub fn to_def(&self) -> Result<PropertyDef, String> {
        let label = self.label.trim();
        let key = if self.key.trim().is_empty() {
            normalise_key(label)
        } else {
            self.key.trim().to_owned()
        };
        if key.is_empty() {
            return Err("Give the property a key or a label.".into());
        }
        if self.kind == KindChoice::Enum && self.variants.trim().is_empty() {
            return Err("A choice property needs at least one variant.".into());
        }
        let mut def = PropertyDef::new(key.clone(), self.scope, self.kind.to_kind(&self.variants))
            .with_label(if label.is_empty() {
                key
            } else {
                label.to_owned()
            });
        if !self.unit.trim().is_empty() {
            def = def.with_unit(self.unit.trim());
        }
        if !self.section.trim().is_empty() {
            def = def.in_section(self.section.trim());
        }
        def.required = self.required;
        Ok(def)
    }
}

#[derive(Debug, Default)]
pub struct State {
    defs: Vec<PropertyDef>,
    error: Option<String>,
    notice: Option<String>,
    form: Form,
    busy: bool,
}

#[derive(Debug, Clone)]
pub enum Message {
    Loaded(Result<Vec<PropertyDef>, String>),
    KeyChanged(String),
    LabelChanged(String),
    UnitChanged(String),
    SectionChanged(String),
    VariantsChanged(String),
    ScopePicked(ScopeChoice),
    KindPicked(KindChoice),
    RequiredToggled(bool),
    Submit,
    Saved(Result<String, String>),
    Delete(PropScope, String),
    Deleted(Result<String, String>),
}

impl State {
    pub fn load(&mut self, store: &Store) -> Task<Message> {
        Task::perform(
            jobs::read(store.clone(), |conn| props::list_property_defs(conn, None)),
            Message::Loaded,
        )
    }

    pub fn update(&mut self, store: Option<&Store>, message: Message) -> Task<Message> {
        match message {
            Message::Loaded(result) => {
                match result {
                    Ok(defs) => {
                        self.defs = defs;
                        self.error = None;
                    }
                    Err(error) => self.error = Some(error),
                }
                Task::none()
            }
            Message::KeyChanged(v) => {
                self.form.key = v;
                Task::none()
            }
            Message::LabelChanged(v) => {
                self.form.label = v;
                Task::none()
            }
            Message::UnitChanged(v) => {
                self.form.unit = v;
                Task::none()
            }
            Message::SectionChanged(v) => {
                self.form.section = v;
                Task::none()
            }
            Message::VariantsChanged(v) => {
                self.form.variants = v;
                Task::none()
            }
            Message::ScopePicked(ScopeChoice(scope)) => {
                self.form.scope = scope;
                Task::none()
            }
            Message::KindPicked(kind) => {
                self.form.kind = kind;
                Task::none()
            }
            Message::RequiredToggled(v) => {
                self.form.required = v;
                Task::none()
            }
            Message::Submit => {
                let Some(store) = store else {
                    self.error = Some("No library is open.".into());
                    return Task::none();
                };
                let def = match self.form.to_def() {
                    Ok(def) => def,
                    Err(error) => {
                        self.error = Some(error);
                        return Task::none();
                    }
                };
                self.busy = true;
                self.error = None;
                let key = def.key.clone();
                Task::perform(
                    jobs::write(store.clone(), move |conn| {
                        props::insert_property_def(conn, &def)?;
                        Ok(key)
                    }),
                    Message::Saved,
                )
            }
            Message::Saved(result) => {
                self.busy = false;
                match result {
                    Ok(key) => {
                        self.notice = Some(format!("Added '{key}'."));
                        self.form = Form::default();
                        store.map_or_else(Task::none, |store| self.load(store))
                    }
                    Err(error) => {
                        self.error = Some(error);
                        Task::none()
                    }
                }
            }
            Message::Delete(scope, key) => {
                let Some(store) = store else {
                    return Task::none();
                };
                let label = key.clone();
                Task::perform(
                    jobs::write(store.clone(), move |conn| {
                        props::delete_property_def(conn, scope, &key)?;
                        Ok(label)
                    }),
                    Message::Deleted,
                )
            }
            Message::Deleted(result) => match result {
                Ok(key) => {
                    self.notice = Some(format!(
                        "Removed '{key}'. Values already stored under it are kept as unrecognised attributes."
                    ));
                    store.map_or_else(Task::none, |store| self.load(store))
                }
                Err(error) => {
                    self.error = Some(error);
                    Task::none()
                }
            },
        }
    }

    #[must_use]
    pub fn view(&self) -> Element<'_, Message> {
        let list = container(scrollable(self.definition_list()))
            .width(Length::Fill)
            .height(Length::Fill)
            .padding([12, 16]);
        let form = container(self.form_view())
            .width(Length::Fixed(360.0))
            .height(Length::Fill)
            .padding([16, 16])
            .style(ui::panel);
        row![list, form].height(Length::Fill).into()
    }

    fn definition_list(&self) -> Element<'_, Message> {
        let mut list = column![text("Property definitions")
            .size(typography::TITLE_SIZE)
            .font(typography::TITLE)]
        .spacing(6);
        if let Some(error) = &self.error {
            list = list.push(text(error).size(typography::BODY_SIZE).style(text::danger));
        }
        if let Some(notice) = &self.notice {
            list = list.push(
                text(notice)
                    .size(typography::BODY_SIZE)
                    .style(text::success),
            );
        }
        if self.defs.is_empty() {
            list = list
                .push(Space::with_height(Length::Fixed(8.0)))
                .push(ui::empty(
                    "No properties are declared yet.",
                    "Declare one on the right and an imported column can be bound to it — that \
                 is what turns a header the file happened to carry into a field the whole \
                 library can be searched on.",
                ));
            return list.into();
        }

        let widths = [80.0, 160.0, 160.0, 120.0, 70.0, 110.0, 70.0];
        let mut head = row![];
        for (label, width) in [
            "Scope", "Key", "Label", "Type", "Unit", "Section", "Required",
        ]
        .into_iter()
        .zip(widths)
        {
            head = head.push(ui::heading(label, width));
        }
        list = list
            .push(Space::with_height(Length::Fixed(6.0)))
            .push(head)
            .push(ui::rule());

        for def in &self.defs {
            let cells = [
                def.scope.to_string(),
                def.key.clone(),
                def.label.clone(),
                def.kind.type_name().to_owned(),
                def.unit.clone().unwrap_or_default(),
                def.section.clone().unwrap_or_default(),
                if def.required { "yes" } else { "" }.to_owned(),
            ];
            // The key and the type are the identifiers this table exists to
            // list, so they are set as identifiers; the label, the unit and
            // the section are words a person wrote.
            let mut line = row![].align_y(Alignment::Center);
            for (index, (value, width)) in cells.into_iter().zip(widths).enumerate() {
                line = line.push(if matches!(index, 1 | 3) {
                    container(
                        text(value)
                            .size(typography::BODY_SIZE)
                            .font(typography::READOUT),
                    )
                    .width(Length::Fixed(width))
                    .padding([3, 6])
                    .into()
                } else {
                    ui::cell(value, width)
                });
            }
            // Removing a definition is destructive, so it says so in colour —
            // but a filled red button on every row would make the table read
            // as a list of things to delete.
            line = line.push(
                button(
                    text("Remove")
                        .size(typography::LABEL_SIZE)
                        .font(typography::LABEL)
                        .style(text::danger),
                )
                .padding([2, 8])
                .style(button::text)
                .on_press(Message::Delete(def.scope, def.key.clone())),
            );
            list = list.push(line);
        }
        list.into()
    }

    fn form_view(&self) -> Element<'_, Message> {
        let form = &self.form;
        fn field<'a>(label: &'static str, input: Element<'a, Message>) -> Column<'a, Message> {
            column![ui::caption(label), input].spacing(4)
        }

        let mut form_col = column![
            text("New property")
                .size(typography::HEADING_SIZE)
                .font(typography::HEADING),
            field(
                "Label",
                text_input("PRF", &form.label)
                    .on_input(Message::LabelChanged)
                    .size(typography::BODY_SIZE)
                    .into()
            ),
            field(
                "Key (snake_case; derived from the label if blank)",
                // A key is an identifier the store will hold verbatim, so it
                // is typed in the face identifiers are shown in.
                text_input("prf_hz", &form.key)
                    .on_input(Message::KeyChanged)
                    .size(typography::BODY_SIZE)
                    .font(typography::READOUT)
                    .into()
            ),
            field(
                "Applies to",
                pick_list(
                    ScopeChoice::ALL,
                    Some(ScopeChoice(form.scope)),
                    Message::ScopePicked
                )
                .text_size(typography::BODY_SIZE)
                .width(Length::Fill)
                .into()
            ),
            field(
                "Type",
                pick_list(KindChoice::ALL, Some(form.kind), Message::KindPicked)
                    .text_size(typography::BODY_SIZE)
                    .width(Length::Fill)
                    .into()
            ),
        ]
        .spacing(10);

        if form.kind == KindChoice::Enum {
            form_col = form_col.push(field(
                "Choices (comma separated)",
                text_input("nrz, manchester", &form.variants)
                    .on_input(Message::VariantsChanged)
                    .size(typography::BODY_SIZE)
                    .into(),
            ));
        }

        form_col = form_col
            .push(field(
                "Unit",
                text_input("Hz", &form.unit)
                    .on_input(Message::UnitChanged)
                    .size(typography::BODY_SIZE)
                    .into(),
            ))
            .push(field(
                "Section (groups fields in the editor)",
                text_input("Timing", &form.section)
                    .on_input(Message::SectionChanged)
                    .size(typography::BODY_SIZE)
                    .into(),
            ))
            .push(
                checkbox("Required", form.required)
                    .on_toggle(Message::RequiredToggled)
                    .text_size(typography::BODY_SIZE),
            )
            .push(Space::with_height(Length::Fixed(4.0)))
            .push(
                button(
                    text(if self.busy {
                        "Saving…"
                    } else {
                        "Add property"
                    })
                    .size(typography::BODY_SIZE),
                )
                .padding([7, 14])
                .style(button::primary)
                .on_press_maybe((!self.busy).then_some(Message::Submit)),
            );

        scrollable(form_col).into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_key_falls_back_to_the_normalised_label() {
        let form = Form {
            label: "Pulse Width".into(),
            unit: "us".into(),
            ..Form::default()
        };
        let def = form.to_def().unwrap();
        assert_eq!(def.key, "pulse_width");
        assert_eq!(def.label, "Pulse Width");
        assert_eq!(def.unit.as_deref(), Some("us"));
        assert_eq!(def.scope, PropScope::Signal);
        assert!(matches!(def.kind, PropKind::Float { .. }));
    }

    #[test]
    fn an_explicit_key_wins_and_the_label_defaults_to_it() {
        let form = Form {
            key: "prf_hz".into(),
            kind: KindChoice::FreqHz,
            ..Form::default()
        };
        let def = form.to_def().unwrap();
        assert_eq!(def.key, "prf_hz");
        assert_eq!(def.label, "prf_hz");
        assert!(matches!(def.kind, PropKind::FreqHz { .. }));
    }

    #[test]
    fn choice_properties_need_variants() {
        let mut form = Form {
            label: "Coding".into(),
            kind: KindChoice::Enum,
            ..Form::default()
        };
        assert!(form.to_def().is_err());
        form.variants = "nrz, manchester,,".into();
        let def = form.to_def().unwrap();
        assert_eq!(
            def.kind,
            PropKind::Enum {
                variants: vec!["nrz".into(), "manchester".into()]
            }
        );
    }

    #[test]
    fn an_empty_form_is_rejected_before_touching_the_store() {
        let mut state = State::default();
        let _ = state.update(None, Message::Submit);
        assert!(state.error.is_some());
    }
}
