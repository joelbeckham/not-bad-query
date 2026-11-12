//! Every user command is an action, so it can be bound to a key and listed
//! in the command palette.

use gpui_kit::component::input::ShowCompletions;
use gpui_kit::*;

actions!(
    good_query,
    [
        RunStatement,
        RunScript,
        CancelQuery,
        Commit,
        Rollback,
        ToggleAutocommit,
        NewTab,
        CloseTab,
        NextTab,
        PrevTab,
        SaveFile,
        SaveFileAs,
        OpenFile,
        ExportCsv,
        CopyCell,
        NewConnection,
        CommandPalette,
        FocusEditor,
        FocusResults,
        FocusSidebar,
        ToggleTheme,
    ]
);

/// (keystroke, action, context, palette label)
pub fn bindings() -> Vec<KeyBinding> {
    vec![
        // Bound in "SqlEditor > Input" so it beats the editor's own ctrl-enter.
        KeyBinding::new("ctrl-enter", RunStatement, Some("SqlEditor > Input")),
        KeyBinding::new("ctrl-enter", RunStatement, Some("Workbench")),
        KeyBinding::new("alt-x", RunScript, Some("Workbench")),
        KeyBinding::new("alt-x", RunScript, Some("SqlEditor > Input")),
        KeyBinding::new("ctrl-.", CancelQuery, Some("SqlEditor > Input")),
        KeyBinding::new("ctrl-.", CancelQuery, Some("Workbench")),
        KeyBinding::new("alt-c", Commit, Some("Workbench")),
        KeyBinding::new("alt-r", Rollback, Some("Workbench")),
        KeyBinding::new("alt-a", ToggleAutocommit, Some("Workbench")),
        KeyBinding::new("ctrl-t", NewTab, Some("Workbench")),
        KeyBinding::new("ctrl-w", CloseTab, Some("Workbench")),
        KeyBinding::new("ctrl-tab", NextTab, Some("Workbench")),
        KeyBinding::new("ctrl-shift-tab", PrevTab, Some("Workbench")),
        KeyBinding::new("ctrl-s", SaveFile, Some("Workbench")),
        KeyBinding::new("ctrl-shift-s", SaveFileAs, Some("Workbench")),
        KeyBinding::new("ctrl-o", OpenFile, Some("Workbench")),
        KeyBinding::new("ctrl-shift-e", ExportCsv, Some("Workbench")),
        KeyBinding::new("ctrl-c", CopyCell, Some("DataTable")),
        KeyBinding::new("ctrl-k", CommandPalette, Some("Workbench")),
        KeyBinding::new("ctrl-k", CommandPalette, Some("SqlEditor > Input")),
        KeyBinding::new("ctrl-1", FocusSidebar, Some("Workbench")),
        KeyBinding::new("ctrl-2", FocusEditor, Some("Workbench")),
        KeyBinding::new("ctrl-3", FocusResults, Some("Workbench")),
        KeyBinding::new("ctrl-space", ShowCompletions, Some("Input && mode == editor")),
    ]
}

/// Commands shown in the palette: (label, keystroke hint, action).
pub fn palette() -> Vec<(&'static str, &'static str, Box<dyn Action>)> {
    vec![
        ("Run statement", "Ctrl+Enter", Box::new(RunStatement)),
        ("Run script", "Alt+X", Box::new(RunScript)),
        ("Cancel query", "Ctrl+.", Box::new(CancelQuery)),
        ("Commit", "Alt+C", Box::new(Commit)),
        ("Rollback", "Alt+R", Box::new(Rollback)),
        ("Toggle auto-commit", "Alt+A", Box::new(ToggleAutocommit)),
        ("New query tab", "Ctrl+T", Box::new(NewTab)),
        ("Close tab", "Ctrl+W", Box::new(CloseTab)),
        ("Open file…", "Ctrl+O", Box::new(OpenFile)),
        ("Save", "Ctrl+S", Box::new(SaveFile)),
        ("Save as…", "Ctrl+Shift+S", Box::new(SaveFileAs)),
        ("Export results to CSV…", "Ctrl+Shift+E", Box::new(ExportCsv)),
        ("New connection…", "", Box::new(NewConnection)),
        ("Toggle light/dark theme", "", Box::new(ToggleTheme)),
    ]
}
