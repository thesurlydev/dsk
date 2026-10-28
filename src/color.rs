//! Minimal ANSI styling without a color crate.

#[derive(Clone, Copy, Debug)]
pub struct Paint {
    enabled: bool,
}

impl Paint {
    pub fn new(enabled: bool) -> Self {
        Self { enabled }
    }

    /// Color only when the caller asked for it and `NO_COLOR` is unset.
    pub fn auto(force_off: bool, is_tty: bool) -> Self {
        let env_allows = std::env::var_os("NO_COLOR").is_none();
        Self::new(!force_off && is_tty && env_allows)
    }

    fn wrap(&self, code: &str, text: &str) -> String {
        if self.enabled {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }

    pub fn bold(&self, text: &str) -> String {
        self.wrap("1", text)
    }

    pub fn dim(&self, text: &str) -> String {
        self.wrap("2", text)
    }

    pub fn cyan(&self, text: &str) -> String {
        self.wrap("36", text)
    }

    pub fn green(&self, text: &str) -> String {
        self.wrap("32", text)
    }

    pub fn yellow(&self, text: &str) -> String {
        self.wrap("33", text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_paint_is_plain() {
        let paint = Paint::new(false);
        assert_eq!(paint.bold("x"), "x");
    }

    #[test]
    fn enabled_paint_wraps() {
        let paint = Paint::new(true);
        assert_eq!(paint.green("42"), "\x1b[32m42\x1b[0m");
        assert_eq!(paint.bold("x"), "\x1b[1mx\x1b[0m");
    }
}
