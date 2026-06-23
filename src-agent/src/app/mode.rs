use crate::model::store::SessionMeta;

pub enum Mode {
    KeyInput(KeyInputForm),
    SessionPicker(PickerState),
    Chat,
}

#[derive(Debug, Clone, Default)]
pub struct KeyInputForm {
    pub api_key: String,
    pub model: String,
    pub field: usize,    // 0 = api_key, 1 = model
    pub first_run: bool, // true when no usable prior session/client exists
    /// true when this form was entered from the --resume session picker; Esc
    /// returns to the picker instead of Quit/Chat.
    pub from_picker: bool,
}

impl KeyInputForm {
    /// first_run = true: Esc quits the app (no broken Chat fallback).
    /// from_picker = true: Esc returns to the session picker.
    pub fn prefilled(
        api_key: String,
        model: String,
        first_run: bool,
        from_picker: bool,
    ) -> Self {
        Self {
            api_key,
            model,
            field: 0,
            first_run,
            from_picker,
        }
    }

    pub fn next_field(&mut self) {
        if self.field < 1 {
            self.field += 1;
        }
    }

    pub fn prev_field(&mut self) {
        if self.field > 0 {
            self.field -= 1;
        }
    }

    pub fn push_char(&mut self, c: char) {
        match self.field {
            0 => self.api_key.push(c),
            _ => self.model.push(c),
        }
    }

    pub fn backspace(&mut self) {
        match self.field {
            0 => {
                self.api_key.pop();
            }
            _ => {
                self.model.pop();
            }
        };
    }

    pub fn is_last(&self) -> bool {
        self.field == 1
    }
}

pub struct PickerState {
    pub query: String,
    pub all: Vec<SessionMeta>,
    pub filtered_idx: Vec<usize>,
    pub selected: usize, // index into filtered_idx
}

impl PickerState {
    pub fn new(all: Vec<SessionMeta>) -> Self {
        let mut s = Self {
            query: String::new(),
            all,
            filtered_idx: vec![],
            selected: 0,
        };
        s.refilter();
        s
    }

    pub fn refilter(&mut self) {
        let q = self.query.to_lowercase();
        self.filtered_idx = self
            .all
            .iter()
            .enumerate()
            .filter(|(_, m)| {
                q.is_empty()
                    || m.name.to_lowercase().contains(&q)
                    || m.id.to_lowercase().contains(&q)
            })
            .map(|(i, _)| i)
            .collect();
        if self.selected >= self.filtered_idx.len() {
            self.selected = self.filtered_idx.len().saturating_sub(1);
        }
    }

    pub fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn move_down(&mut self) {
        if self.selected + 1 < self.filtered_idx.len() {
            self.selected += 1;
        }
    }

    pub fn selected_meta(&self) -> Option<&SessionMeta> {
        self.filtered_idx
            .get(self.selected)
            .and_then(|&i| self.all.get(i))
    }
}
