use std::num::NonZeroUsize;

use accessibility_ng::{AXAttribute, AXUIElement};
use accessibility_sys_ng::{kAXFocusedUIElementAttribute, kAXSelectedTextAttribute};
use active_win_pos_rs::get_active_window;
use core_foundation::string::CFString;
use core_graphics::{event::{CGEvent, CGEventTapLocation, CGKeyCode}, event_source::{CGEventSource, CGEventSourceStateID}};
use log::error;
use lru::LruCache;
use objc2_app_kit::{NSPasteboard, NSPasteboardTypeString};
use parking_lot::Mutex;

#[derive(Debug, Clone, serde::Serialize)]
pub struct SelectedText {
    is_file_paths: bool,
    app_name: String,
    text: Vec<String>,
}

static GET_SELECTED_TEXT_METHOD: Mutex<Option<LruCache<String, u8>>> = Mutex::new(None);

use anyhow::{anyhow, bail};

const CMD_KEY: CGKeyCode = core_graphics::event::KeyCode::COMMAND;
const KEY_C: CGKeyCode = 8;

pub fn simulate(key: CGKeyCode, key_down: bool) -> anyhow::Result<()> {
    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState).map_err(|_| anyhow!("Failed to create CGEventSource"))?;
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
    simulate(CMD_KEY, true)?;
    // keyup
    simulate(CMD_KEY, false)?;
    // keydown
    simulate(KEY_C, true)?;
    // keyup
    simulate(KEY_C, false)?;
    Ok(())
}


#[cfg(target_os = "macos")]
pub fn get_selected_text_by_copy() -> anyhow::Result<String>{
    use objc2::runtime::ProtocolObject;
    use objc2_foundation::NSArray;

    let pasteboard = unsafe {NSPasteboard::generalPasteboard()};
    let saved_change_count = unsafe {pasteboard.changeCount()};
    let saved_contents = unsafe {pasteboard.pasteboardItems()};
    
    sim_ctrl_c()?;
    
    let start_time = std::time::Instant::now();
    let timeout = std::time::Duration::from_millis(200);
    let mut new_change_count = saved_change_count;
    while new_change_count == saved_change_count {
        if start_time.elapsed() > timeout {
            anyhow::bail!("Timeout waiting for pasteboard to update");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
        new_change_count = unsafe { pasteboard.changeCount() };
    }
    let copied_text =  unsafe { pasteboard.stringForType(NSPasteboardTypeString) };
    unsafe {
        if let Some(prev_contents) = saved_contents {
            pasteboard.clearContents();
            let max = prev_contents.count();
            let mut objs = Vec::with_capacity(max+10);
            for i in 0..max {
                objs.push(ProtocolObject::from_retained(prev_contents.objectAtIndex(i)));
            }

            let res = pasteboard.writeObjects(&NSArray::from_vec(objs)) ;
            if !res {
                bail!("Failed to write objects to pasteboard");
            }
        }
    }
    Ok(copied_text.map(|t| t.to_string()).unwrap_or_default())
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

pub fn in_finder_or_empty_window() -> bool {
    let (app_name, _) = get_window_meta();
    app_name == "Finder" || app_name == "Empty Window"
}


pub fn get_selected_text() -> anyhow::Result<SelectedText> {
    if GET_SELECTED_TEXT_METHOD.lock().is_none() {
        let cache = LruCache::new(NonZeroUsize::new(100).unwrap());
        *GET_SELECTED_TEXT_METHOD.lock() = Some(cache);
    }
    let mut cache = GET_SELECTED_TEXT_METHOD.lock();
    let cache = cache.as_mut().unwrap();
    
    let (app_name, window_title) = get_window_meta();

    let no_active_app = app_name == "Empty Window";
    if app_name == "Finder" || no_active_app {
        match get_selected_file_paths_by_clipboard_using_applescript(no_active_app) {
            Ok(text) => {
                println!("file paths: {:?}", text.split("\n"));
                return Ok(SelectedText {
                    is_file_paths: true,
                    app_name,
                    text: text.split("\n").map(|t| t.to_owned()).collect::<Vec<String>>(),
                });
            }
            Err(e) => {
                error!("get_selected_file_paths_by_clipboard_using_applescript failed: {:?}", e);
            }
        }
    }

    let mut selected_text = SelectedText {
        is_file_paths: false,
        app_name: app_name.clone(),
        text: vec![],
    };

    if let Some(text) = cache.get(&app_name) {
        if *text == 0 {
            let ax_text = get_selected_text_by_ax()?;
            if !ax_text.is_empty() {
                cache.put(app_name.clone(), 0);
                selected_text.text = vec![ax_text];
                return Ok(selected_text);
            }
        }
        let txt = get_selected_text_by_copy()?;
        selected_text.text = vec![txt];
        return Ok(selected_text);
    }
    match get_selected_text_by_ax() {
        Ok(txt) => {
            if !txt.is_empty() {
                cache.put(app_name.clone(), 0);
            }
            selected_text.text = vec![txt];
            Ok(selected_text)
        }
        Err(_) => match get_selected_text_by_copy() {
            Ok(txt) => {
                if !txt.is_empty() {
                    cache.put(app_name, 1);
                }
                selected_text.text = vec![txt];
                Ok(selected_text)
            }
            Err(e) => bail!(e),
        },
    }
}

fn get_selected_text_by_ax() -> anyhow::Result<String> {
    // debug_println!("get_selected_text_by_ax");
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


fn get_selected_file_paths_by_clipboard_using_applescript(for_empty_window: bool
) -> Result<String, Box<dyn std::error::Error>> {
    // debug_println!("get_selected_text_by_clipboard_using_applescript");
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_selected_text() {
        println!("--- get_selected_text ---");
        let text = get_selected_text().unwrap();
        println!("selected text: {:#?}", text);
        println!("--- get_selected_text ---");
        let text = get_selected_text().unwrap();
        println!("selected text: {:#?}", text);
        println!("--- get_selected_text ---");
        let text = get_selected_text().unwrap();
        println!("selected text: {:#?}", text);
    }
    

}
