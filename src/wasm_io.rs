//! Browser-only I/O helpers: file upload (hidden <input type=file> + FileReader)
//! and file download (Blob + anchor), plus removing the startup loading hint.
//!
//! Picked files are read fully and pushed into a thread-local inbox; the app
//! drains it once per frame. This module only compiles on wasm32.

use std::cell::RefCell;

use wasm_bindgen::prelude::Closure;
use wasm_bindgen::JsCast;

/// What a picked file is used for.
pub enum FileEvent {
    Project(Vec<u8>),
    Midi(Vec<u8>),
    Sample(Vec<u8>),
}

thread_local! {
    static INBOX: RefCell<Vec<FileEvent>> = const { RefCell::new(Vec::new()) };
}

/// Drain pending file events (call once per frame from the UI).
pub fn drain() -> Vec<FileEvent> {
    INBOX.with(|b| std::mem::take(&mut *b.borrow_mut()))
}

/// Open a hidden file picker; the chosen file is read fully and pushed into
/// the inbox as `event`.
pub fn pick_file(accept: &str, event: fn(Vec<u8>) -> FileEvent) {
    let window = web_sys::window().expect("window");
    let document = window.document().expect("document");
    let input = document
        .create_element("input")
        .expect("create input")
        .dyn_into::<web_sys::HtmlInputElement>()
        .expect("input element");
    input.set_type("file");
    input.set_accept(accept);
    let input = std::rc::Rc::new(input);
    let input2 = input.clone();
    let on_change = Closure::once(move || {
        let Some(file) = input.files().and_then(|files| files.get(0)) else {
            return;
        };
        let reader = std::rc::Rc::new(web_sys::FileReader::new().expect("FileReader"));
        let reader2 = reader.clone();
        let on_load = Closure::once(move || {
            if let Ok(buf) = reader2.result() {
                let bytes = js_sys::Uint8Array::new(&buf).to_vec();
                INBOX.with(|b| b.borrow_mut().push(event(bytes)));
            }
        });
        reader.set_onload(Some(on_load.as_ref().unchecked_ref()));
        let _ = reader.read_as_array_buffer(&file);
        on_load.forget();
    });
    input2.set_onchange(Some(on_change.as_ref().unchecked_ref()));
    if let Some(body) = document.body() {
        let _ = body.append_child(input2.as_ref());
    }
    input2.click();
    on_change.forget();
}

/// Trigger a browser download of `bytes` under `name`.
pub fn download(name: &str, bytes: &[u8]) {
    let window = web_sys::window().expect("window");
    let document = window.document().expect("document");
    let array = js_sys::Uint8Array::from(bytes);
    let blob = web_sys::Blob::new_with_u8_array_sequence(&js_sys::Array::of1(&array))
        .expect("blob");
    let url = web_sys::Url::create_object_url_with_blob(&blob).expect("object url");
    let a = document
        .create_element("a")
        .expect("create a")
        .dyn_into::<web_sys::HtmlAnchorElement>()
        .expect("anchor");
    a.set_href(&url);
    a.set_download(name);
    a.click();
    let _ = web_sys::Url::revoke_object_url(&url);
}

/// Remove the "Loading…" overlay once the app is actually rendering.
pub fn remove_loading() {
    if let Some(document) = web_sys::window().and_then(|w| w.document()) {
        if let Some(el) = document.get_element_by_id("loading") {
            let _ = el.remove();
        }
    }
}