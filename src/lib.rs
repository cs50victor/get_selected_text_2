use std::sync::Arc;

use accessibility_ng::{AXAttribute, AXUIElement};
use accessibility_sys_ng::{kAXFocusedUIElementAttribute, kAXSelectedTextAttribute};
use active_win_pos_rs::get_active_window;
use core_foundation::string::CFString;
use core_graphics::{
    event::{CGEvent, CGEventTapLocation, CGKeyCode},
    event_source::{CGEventSource, CGEventSourceStateID},
};
use log::error;
use objc2::rc::Retained;
use objc2_app_kit::{NSPasteboard, NSPasteboardItem, NSPasteboardTypeString};

use anyhow::{anyhow, bail};
use objc2_foundation::NSArray;

#[derive(Debug, Clone, serde::Serialize)]
pub struct SelectedText {
    pub is_file_paths: bool,
    pub app_name: String,
    pub text: Vec<String>,
}
pub struct PasteboardSavedState {
    pub saved_change_count: isize,
    pub saved_contents: Option<objc2::rc::Retained<NSArray<NSPasteboardItem>>>,
}

pub enum GetSelectedTextResult {
    Text(SelectedText),
    PasteboardState(PasteboardSavedState),
}

#[derive(Clone)]
pub struct PasteBoardContainer {
    pub inner: Arc<objc2::rc::Retained<NSPasteboard>>,
    pub pasteboard: Option<Retained<NSArray<NSPasteboardItem>>>,
}
unsafe impl Send for PasteBoardContainer {}
unsafe impl Sync for PasteBoardContainer {}

const CMD_KEY: CGKeyCode = core_graphics::event::KeyCode::COMMAND;
const KEY_C: CGKeyCode = 8;

pub fn simulate(key: CGKeyCode, key_down: bool) -> anyhow::Result<()> {
    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
        .map_err(|_| anyhow!("Failed to create CGEventSource"))?;
    if let Some(cg_event) = CGEvent::new_keyboard_event(source, key, key_down).ok() {
        cg_event.post(CGEventTapLocation::HID);
        // Let ths MacOS catchup
        std::thread::sleep(std::time::Duration::from_millis(20));
        Ok(())
    } else {
        bail!("Failed to simulate key press event for spotlight selected text copy")
    }
}

// KeyPress(Key),
// KeyRelease(Key),
// reference - https://github.com/Narsil/rdev/blob/main/src/macos/keycodes.rs
pub fn sim_ctrl_c() -> anyhow::Result<()> {
    // keydown
    println!("keydown cmd");
    simulate(CMD_KEY, true)?;
    // keydown
    println!("keydown c");
    simulate(KEY_C, true)?;
    // keyup
    println!("key up c");
    simulate(KEY_C, false)?;
    // keyup
    println!("key up cmd");
    simulate(CMD_KEY, false)?;
    Ok(())
}

const QUIET_CMD_C: &str = r#"
tell application "System Events"
    set savedAlertVolume to alert volume of (get volume settings)
    set volume alert volume 0
    keystroke "c" using {command down}
    set volume alert volume savedAlertVolume
end tell
"#;

fn quiet_cmd_c() -> anyhow::Result<()> {
    // debug_println!("get_selected_text_by_clipboard_using_applescript");
    let output = std::process::Command::new("osascript")
        .arg("-e")
        .arg(QUIET_CMD_C)
        .output()?;
    // .spawn()?;

    if !output.status.success() {
        bail!(output
            .stderr
            .into_iter()
            .map(|c| c as char)
            .collect::<String>());
    }
    Ok(())
}

pub fn ctrl_c_and_save_pasteboard(
    pasteboard: &objc2::rc::Retained<NSPasteboard>,
    use_applescript: bool,
) -> anyhow::Result<PasteboardSavedState> {
    let saved_change_count = unsafe { pasteboard.changeCount() };
    let saved_contents = unsafe { pasteboard.pasteboardItems() };

    if use_applescript {
        quiet_cmd_c()?;
    } else {
        sim_ctrl_c()?;
    }

    Ok(PasteboardSavedState {
        saved_change_count,
        saved_contents,
    })
}

#[cfg(target_os = "macos")]
pub fn get_selected_text_from_pasteboard(
    app_name: String,
    pasteboard: &objc2::rc::Retained<NSPasteboard>,
    saved_change_count: isize,
    saved_contents: Option<objc2::rc::Retained<NSArray<NSPasteboardItem>>>,
    pasteboard_wait_timeout: u64,
) -> anyhow::Result<SelectedText> {
    use log::info;
    use objc2::runtime::ProtocolObject;

    let start_time = std::time::Instant::now();
    let timeout = std::time::Duration::from_millis(pasteboard_wait_timeout);
    let mut new_change_count = saved_change_count;
    while new_change_count == saved_change_count {
        if start_time.elapsed() > timeout {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
        new_change_count = unsafe { pasteboard.changeCount() };
    }
    if new_change_count == saved_change_count {
        println!("User didn't select any text or pasteboard took too long to update");
        info!("User didn't select any text or pasteboard took too long to update");
        return Ok(SelectedText {
            is_file_paths: false,
            app_name: app_name.clone(),
            text: vec![String::new()],
        });
    }
    let copied_text = unsafe { pasteboard.stringForType(NSPasteboardTypeString) };
    println!("copied_text: {:?}", copied_text);
    println!("new_change_count: {:?}", new_change_count);
    println!("saved_change_count: {:?}", saved_change_count);
    unsafe {
        if let Some(prev_contents) = saved_contents {
            pasteboard.clearContents();
            let max = prev_contents.count();
            println!("max: {:?}", max);
            println!("prev_contents: {:?}", prev_contents.lastObject());
            if max > 1 {
                let mut objs = Vec::with_capacity(max + 10);
                for i in 0..max - 1 {
                    objs.push(ProtocolObject::from_retained(
                        prev_contents.objectAtIndex(i),
                    ));
                }

                let res = pasteboard.writeObjects(&NSArray::from_vec(objs));
                if !res {
                    bail!("Failed to write objects to pasteboard");
                }
            }
        }
    }
    Ok(SelectedText {
        is_file_paths: false,
        app_name: app_name.clone(),
        text: vec![copied_text.map(|t| t.to_string()).unwrap_or_default()],
    })
}

pub fn get_window_meta() -> (String, String) {
    match get_active_window() {
        Ok(window) => (window.app_name, window.title),
        Err(_) => {
            // user might be in the desktop / home view
            ("Empty Window".into(), "Empty Window".into())
        }
    }
}

pub fn in_finder_or_empty_window() -> (bool, String) {
    let (app_name, _) = get_window_meta();
    (app_name == "Finder" || app_name == "Empty Window", app_name)
}

pub fn get_selected_files(window_name: &str) -> anyhow::Result<SelectedText> {
    let no_active_app = window_name == "Empty Window";
    match get_selected_file_paths_by_clipboard_using_applescript(no_active_app) {
        Ok(text) => {
            println!("file paths: {:?}", text.split("\n"));
            return Ok(SelectedText {
                is_file_paths: true,
                app_name: window_name.to_owned(),
                text: text
                    .split("\n")
                    .map(|t| t.to_owned())
                    .collect::<Vec<String>>(),
            });
        }
        Err(e) => {
            bail!(
                "get_selected_file_paths_by_clipboard_using_applescript failed: {:?}",
                e
            );
        }
    }
}
pub fn get_selected_text_using_ax_then_copy(
    app_name: String,
    pasteboard: &objc2::rc::Retained<NSPasteboard>,
    use_apple_script: bool,
) -> anyhow::Result<GetSelectedTextResult> {
    let mut selected_text = SelectedText {
        is_file_paths: false,
        app_name: app_name.clone(),
        text: vec![],
    };

    match get_selected_text_by_ax() {
        Ok(txt) => {
            selected_text.text = vec![txt];
            Ok(GetSelectedTextResult::Text(selected_text))
        }
        Err(e) => {
            error!("get_selected_text_by_ax failed: {:?}", e);
            Ok(GetSelectedTextResult::PasteboardState(
                ctrl_c_and_save_pasteboard(pasteboard, use_apple_script)?,
            ))
        }
    }
}

fn get_selected_text_by_ax() -> anyhow::Result<String> {
    log::info!("get_selected_text_by_ax");
    let system_element = AXUIElement::system_wide();
    let Some(selected_element) = system_element
        .attribute(&AXAttribute::new(&CFString::from_static_string(
            kAXFocusedUIElementAttribute,
        )))
        .map(|element| element.downcast_into::<AXUIElement>())
        .ok()
        .flatten()
    else {
        bail!("No selected element");
    };
    let Some(selected_text) = selected_element
        .attribute(&AXAttribute::new(&CFString::from_static_string(
            kAXSelectedTextAttribute,
        )))
        .map(|text| text.downcast_into::<CFString>())
        .ok()
        .flatten()
    else {
        bail!("No selected text");
    };
    Ok(selected_text.to_string())
}

const FILE_PATH_COPY_APPLE_SCRIPT: &str = r#"
tell application "Finder"
	set selectedItems to selection
	
	if selectedItems is {} then
		return "" -- Return an empty string if no items are selected
	end if
	
	set itemPaths to {}
	repeat with anItem in selectedItems
		set filePath to POSIX path of (anItem as alias)
		-- Escape any existing double quotes in the file path
		set escapedPath to my replace_chars(filePath, "\"", "\\\"")
		-- Add the escaped and quoted path to the list
		set end of itemPaths to "\"" & escapedPath & "\""
	end repeat
	
	set AppleScript's text item delimiters to linefeed
	set pathText to itemPaths as text
	
	return pathText -- Return the pathText content
end tell

on replace_chars(this_text, search_string, replacement_string)
	set AppleScript's text item delimiters to the search_string
	set the item_list to every text item of this_text
	set AppleScript's text item delimiters to the replacement_string
	set this_text to the item_list as string
	set AppleScript's text item delimiters to ""
	return this_text
end replace_chars
"#;

const EMPTY_WINDOW_PATH_COPY_APPLE_SCRIPT: &str = r#"
tell application "Finder"
	set desktopPath to (path to desktop folder as text)
	set selectedItems to (get selection)
	
	if selectedItems is {} then
		return "" -- Return an empty string if no items are selected
	end if
	
	set itemPaths to {}
	repeat with anItem in selectedItems
		set filePath to POSIX path of (anItem as alias)
		-- Escape any existing double quotes in the file path
		set escapedPath to my replace_chars(filePath, "\"", "\\\"")
		-- Add the escaped and quoted path to the list
		set end of itemPaths to "\"" & escapedPath & "\""
	end repeat
	
	set AppleScript's text item delimiters to linefeed
	set pathText to itemPaths as text
	
	return pathText -- Return the pathText content
end tell

on replace_chars(this_text, search_string, replacement_string)
	set AppleScript's text item delimiters to the search_string
	set the item_list to every text item of this_text
	set AppleScript's text item delimiters to the replacement_string
	set this_text to the item_list as string
	set AppleScript's text item delimiters to ""
	return this_text
end replace_chars
"#;

fn get_selected_file_paths_by_clipboard_using_applescript(
    for_empty_window: bool,
) -> Result<String, Box<dyn std::error::Error>> {
    log::info!("get_selected_text_by_clipboard_using_applescript");
    let mut binding = std::process::Command::new("osascript");
    let cmd = binding.arg("-e");

    if for_empty_window {
        cmd.arg(EMPTY_WINDOW_PATH_COPY_APPLE_SCRIPT);
    } else {
        cmd.arg(FILE_PATH_COPY_APPLE_SCRIPT);
    };

    let output = cmd.output()?;

    if output.status.success() {
        let content = String::from_utf8(output.stdout)?;
        let content = content.trim();
        Ok(content.to_string())
    } else {
        let err = output
            .stderr
            .into_iter()
            .map(|c| c as char)
            .collect::<String>()
            .into();
        Err(err)
    }
}

fn _selected_text(
    app_name: String,
    pasteboard: &objc2::rc::Retained<NSPasteboard>,
    use_apple_script: bool,
) -> anyhow::Result<SelectedText> {
    match get_selected_text_using_ax_then_copy(app_name.clone(), &pasteboard, use_apple_script)? {
        GetSelectedTextResult::Text(selected_text) => Ok(selected_text),
        GetSelectedTextResult::PasteboardState(mut pasteboard_saved_state) => {
            get_selected_text_from_pasteboard(
                app_name.clone(),
                &pasteboard,
                pasteboard_saved_state.saved_change_count,
                pasteboard_saved_state.saved_contents.take(),
                90,
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_selected_text() {
        const USE_APPLE_SCRIPT: bool = false;
        let dummy_app_name = "Dummy App".to_owned();
        let pasteboard = unsafe { NSPasteboard::generalPasteboard() };
        println!("--- get_selected_text ---");
        let mut start = std::time::Instant::now();
        let text = _selected_text(dummy_app_name.clone(), &pasteboard, USE_APPLE_SCRIPT).unwrap();
        let elapsed = start.elapsed();
        println!("Time elapsed: {} ms", elapsed.as_millis());
        println!("selected text: {:#?}", text);
        println!("--- get_selected_text ---");
        std::thread::sleep(std::time::Duration::from_millis(1000));
        start = std::time::Instant::now();
        let text = _selected_text(dummy_app_name.clone(), &pasteboard, USE_APPLE_SCRIPT).unwrap();
        let elapsed = start.elapsed();
        println!("Time elapsed: {} ms", elapsed.as_millis());
        println!("selected text: {:#?}", text);
        println!("--- get_selected_text ---");
        std::thread::sleep(std::time::Duration::from_millis(1000));
        start = std::time::Instant::now();
        let text = _selected_text(dummy_app_name.clone(), &pasteboard, USE_APPLE_SCRIPT).unwrap();
        let elapsed = start.elapsed();
        println!("Time elapsed: {} ms", elapsed.as_millis());
        println!("selected text: {:#?}", text);
    }
}
