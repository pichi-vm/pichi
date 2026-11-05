// SPDX-FileCopyrightText: Advanced Micro Devices, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Small terminal accents; redirected output stays plain text.

use std::io::IsTerminal as _;

#[derive(Debug)]
pub(super) struct Presentation {
    color: bool,
}

impl Presentation {
    pub(super) fn stdout() -> Self {
        Self::new(std::io::stdout().is_terminal())
    }

    pub(super) fn stderr() -> Self {
        Self::new(std::io::stderr().is_terminal())
    }

    fn new(terminal: bool) -> Self {
        Self {
            color: terminal
                && std::env::var_os("NO_COLOR").is_none()
                && std::env::var_os("TERM").is_some_and(|term| term != "dumb"),
        }
    }

    pub(super) fn heading(&self, text: &str) -> String {
        if self.color {
            format!("\x1b[1;36m{text}\x1b[0m")
        } else {
            text.to_owned()
        }
    }

    pub(super) fn success(&self, text: &str) -> String {
        if self.color {
            format!("\x1b[1;32m{text}\x1b[0m")
        } else {
            text.to_owned()
        }
    }
}
