//! The core's part in a language change: notice it, point the string
//! catalogue and the fonts at the new language, and draw every window
//! again.
//!
//! Nothing here chooses the language.  The settings file names it; when
//! it says `system`, the shell's answer is in a file beside it, read on
//! the same once-a-second look the theme uses.  A change lands in the
//! language's version counter, which every pass of the loop checks with
//! one atomic load.

impl super::super::CoreApp {
    pub(super) fn follow_language(&mut self, looked: bool) {
        if looked {
            marspot::system_language::poll();
        }
        if marspot::system_language::changed("every window is drawn again in the language now in force") {
            marspot::ui::strings::follow_language();
            // glyph choices, shaped lines and widths were made for the old language
            self.renderer.font_mut().follow_language();
            for w in self.windows.iter_mut() {
                w.needs_render = true;
            }
        }
    }
}
