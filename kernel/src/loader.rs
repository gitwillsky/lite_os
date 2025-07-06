use alloc::vec::Vec;
use lazy_static::lazy_static;

unsafe extern "C" {
    fn _num_app();
    fn _app_names();
}

pub fn get_num_app() -> usize {
    unsafe { (_num_app as usize as *const usize).read_volatile() }
}

fn get_app_data(app_id: usize) -> &'static [u8] {
    let num_app_ptr = _num_app as usize as *const usize;
    let num_app = get_num_app();

    let app_start = unsafe { core::slice::from_raw_parts(num_app_ptr.add(1), num_app + 1) };

    assert!(app_id < num_app);
    unsafe {
        core::slice::from_raw_parts(
            app_start[app_id] as *const u8,
            app_start[app_id + 1] - app_start[app_id],
        )
    }
}

pub fn get_app_data_by_name(app_name: &str) -> Option<&'static [u8]> {
    let app_names = APP_NAMES.as_slice();
    let app_id = app_names.iter().position(|&name| name == app_name);
    app_id.map(|id| get_app_data(id))
}

lazy_static! {
    static ref APP_NAMES: Vec<&'static str> = {
        let num_app = get_num_app();
        let mut app_names_ptr = _app_names as usize as *const u8;
        let mut app_names = Vec::new();
        for _ in 0..num_app {
            let mut end = app_names_ptr;
            unsafe {
                while end.read_volatile() != '\0' as u8 {
                    end = end.add(1);
                }
            }
            let app_name = unsafe {
                core::str::from_utf8_unchecked(core::slice::from_raw_parts(
                    app_names_ptr,
                    end.offset_from(app_names_ptr) as usize,
                ))
            };
            app_names.push(app_name);
            unsafe {
                app_names_ptr = end.add(1);
            }
        }
        app_names
    };
}
