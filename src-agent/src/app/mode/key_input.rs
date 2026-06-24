/// Transient state for the credentials input form.
///
/// Created with [`KeyInputForm::prefilled`]; fields are edited in place via
/// `push_char` / `backspace`; the controller reads `field` to know which
/// entry is active. Three fields in order: api_key (0), model (1), provider (2).
#[derive(Debug, Clone, Default)]
pub struct KeyInputForm {
    pub api_key: String,
    pub model: String,
    /// OpenRouter provider slug for strict-pin routing (optional, may be empty).
    pub provider: String,
    /// Active field index: `0` = api_key, `1` = model, `2` = provider.
    pub field: usize,
    /// `true` when no prior session / configured client exists.
    /// Controls Esc behaviour: if true, Esc must quit (there is no Chat view
    /// to return to).
    pub first_run: bool,
    /// `true` when this form was entered from the `--resume` session picker.
    /// Esc returns to the picker instead of Quit / Chat.
    pub from_picker: bool,
}

impl KeyInputForm {
    /// Construct a form pre-populated with existing credentials.
    ///
    /// - `first_run = true`:   Esc quits (no usable Chat fallback).
    /// - `from_picker = true`: Esc returns to the session picker.
    /// - `provider`:           OpenRouter provider slug (may be empty for default routing).
    pub fn prefilled(
        api_key: String,
        model: String,
        provider: String,
        first_run: bool,
        from_picker: bool,
    ) -> Self {
        Self {
            api_key,
            model,
            provider,
            field: 0,
            first_run,
            from_picker,
        }
    }

    /// Advance to the next field (clamps at the last field, index 2).
    pub fn next_field(&mut self) {
        if self.field < 2 {
            self.field += 1;
        }
    }

    /// Move to the previous field (clamps at zero).
    pub fn prev_field(&mut self) {
        if self.field > 0 {
            self.field -= 1;
        }
    }

    /// Append a character to whichever field is currently active.
    pub fn push_char(&mut self, c: char) {
        match self.field {
            0 => self.api_key.push(c),
            1 => self.model.push(c),
            _ => self.provider.push(c),
        }
    }

    /// Delete the last character from the active field.
    pub fn backspace(&mut self) {
        match self.field {
            0 => { self.api_key.pop(); }
            1 => { self.model.pop(); }
            _ => { self.provider.pop(); }
        };
    }

    /// Returns `true` when the cursor is on the last field (provider, index 2).
    ///
    /// Used by the controller to decide whether Enter should advance the
    /// cursor or submit the form.
    pub fn is_last(&self) -> bool {
        self.field == 2
    }
}
