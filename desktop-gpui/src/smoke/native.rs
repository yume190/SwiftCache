use std::{
    ffi::{CString, c_void},
    path::Path,
};
type Id = *mut c_void;
#[link(name = "objc")]
unsafe extern "C" {
    fn objc_getClass(name: *const i8) -> Id;
    fn sel_registerName(name: *const i8) -> Id;
    fn objc_msgSend();
}
unsafe fn send0(obj: Id, sel: &str) -> Id {
    let sel = CString::new(sel).unwrap();
    let f: unsafe extern "C" fn(Id, Id) -> Id = unsafe { std::mem::transmute(objc_msgSend as unsafe extern "C" fn()) };
    unsafe { f(obj, sel_registerName(sel.as_ptr())) }
}
unsafe fn send1(obj: Id, sel: &str, arg: Id) -> Id {
    let sel = CString::new(sel).unwrap();
    let f: unsafe extern "C" fn(Id, Id, Id) -> Id =
        unsafe { std::mem::transmute(objc_msgSend as unsafe extern "C" fn()) };
    unsafe { f(obj, sel_registerName(sel.as_ptr()), arg) }
}
unsafe fn string(value: &str) -> Id {
    let value = CString::new(value).unwrap();
    unsafe {
        send1(
            objc_getClass(c"NSString".as_ptr()),
            "stringWithUTF8String:",
            value.as_ptr() as Id,
        )
    }
}
pub fn accessibility() {
    unsafe {
        let app = send0(objc_getClass(c"NSApplication".as_ptr()), "sharedApplication");
        let window = send0(app, "keyWindow");
        let window = if window.is_null() {
            send0(send0(app, "windows"), "firstObject")
        } else {
            window
        };
        if !window.is_null() {
            let view = send0(window, "contentView");
            let _ = send0(view, "accessibilityChildren");
        }
    }
}
pub fn save(path: &str) -> Result<(), String> {
    unsafe {
        let app = send0(objc_getClass(c"NSApplication".as_ptr()), "sharedApplication");
        let class = objc_getClass(c"NSSavePanel".as_ptr());
        let f: unsafe extern "C" fn(Id, Id, Id) -> bool = std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
        let windows = send0(app, "windows");
        let count = send0(windows, "count") as usize;
        let mut panel = std::ptr::null_mut();
        for i in 0..count {
            let candidate = send1(windows, "objectAtIndex:", i as Id);
            if f(candidate, sel_registerName(c"isKindOfClass:".as_ptr()), class) {
                panel = candidate;
                break;
            }
            let sheet = send0(candidate, "attachedSheet");
            if !sheet.is_null() && f(sheet, sel_registerName(c"isKindOfClass:".as_ptr()), class) {
                panel = sheet;
                break;
            }
        }
        if panel.is_null() {
            return Err("no native save panel in application windows".into());
        }
        let path = Path::new(path);
        let url = send1(
            objc_getClass(c"NSURL".as_ptr()),
            "fileURLWithPath:",
            string(path.parent().unwrap().to_str().unwrap()),
        );
        send1(panel, "setDirectoryURL:", url);
        send1(
            panel,
            "setNameFieldStringValue:",
            string(path.file_name().unwrap().to_str().unwrap()),
        );
        send1(panel, "ok:", std::ptr::null_mut());
    }
    Ok(())
}

unsafe fn text(obj: Id) -> String {
    if obj.is_null() {
        return String::new();
    }
    let desc = unsafe { send0(obj, "description") };
    let bytes = unsafe { send0(desc, "UTF8String") };
    if bytes.is_null() {
        String::new()
    } else {
        unsafe { std::ffi::CStr::from_ptr(bytes as *const i8) }
            .to_string_lossy()
            .into_owned()
    }
}
unsafe fn children(obj: Id) -> Vec<Id> {
    if obj.is_null() {
        return vec![];
    }
    let array = unsafe { send0(obj, "accessibilityChildren") };
    if array.is_null() {
        return vec![];
    }
    let count = unsafe { send0(array, "count") } as usize;
    (0..count.min(2000))
        .map(|i| unsafe { send1(array, "objectAtIndex:", i as Id) })
        .collect()
}
pub fn press(label: &str) -> Result<(), String> {
    unsafe {
        let app = send0(objc_getClass(c"NSApplication".as_ptr()), "sharedApplication");
        let windows = send0(app, "windows");
        let count = send0(windows, "count") as usize;
        let mut stack = (0..count)
            .map(|i| send0(send1(windows, "objectAtIndex:", i as Id), "contentView"))
            .collect::<Vec<_>>();
        let mut seen = std::collections::HashSet::new();
        let mut labels = Vec::new();
        while let Some(node) = stack.pop() {
            if node.is_null() || !seen.insert(node as usize) {
                continue;
            }
            let current = text(send0(node, "accessibilityLabel"));
            if current == label {
                let f: unsafe extern "C" fn(Id, Id) -> bool =
                    std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
                return if f(node, sel_registerName(c"accessibilityPerformPress".as_ptr())) {
                    Ok(())
                } else {
                    Err(format!("native accessibility rejected press: {label}"))
                };
            }
            if !current.is_empty() {
                labels.push(current)
            }
            stack.extend(children(node));
        }
        Err(format!("native control not found: {label}; labels={labels:?}"))
    }
}
