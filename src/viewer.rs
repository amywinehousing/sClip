//! Playback runs only in the viewer subprocess. The recording worker never loads libmpv.
use libloading::os::windows::Library;
use std::{
    ffi::{CStr, CString, c_char, c_int, c_void},
    io,
    path::Path,
    ptr::{self, NonNull},
};

type Handle = *mut c_void;
type Destroy = unsafe extern "C" fn(Handle);

struct Player {
    handle: NonNull<c_void>,
    destroy: Destroy,
}
impl Drop for Player {
    fn drop(&mut self) {
        unsafe { (self.destroy)(self.handle.as_ptr()) }
    }
}

// ABI prefixes from mpv/client.h. libmpv owns all event memory until the next wait.
#[repr(C)]
struct Event {
    id: c_int,
    error: c_int,
    userdata: u64,
    data: *const c_void,
}
#[repr(C)]
struct EndFile {
    reason: c_int,
    error: c_int,
}

pub fn play(path: &Path) -> io::Result<()> {
    let file = path.canonicalize()?;
    if !file.is_file() {
        return Err(io::Error::other("Choose a local video file."));
    }
    // libmpv's Windows API accepts UTF-8 filenames.
    let file = CString::new(
        file.to_str()
            .ok_or_else(|| io::Error::other("Invalid Unicode filename"))?,
    )?;
    let dll = std::env::current_exe()?
        .with_file_name("player")
        .join("libmpv-2.dll");
    // Only use the packaged DLL and its directory or Windows system DLLs for dependencies.
    unsafe {
        let lib = Library::load_with_flags(&dll, 0x100 | 0x800).map_err(|e| {
            io::Error::other(format!(
                "Could not load {} ({e}). Restore the player folder beside sClip.exe.",
                dll.display()
            ))
        })?;
        let create = lib
            .get::<unsafe extern "C" fn() -> Handle>(b"mpv_create\0")
            .map_err(io::Error::other)?;
        let destroy = *lib
            .get::<Destroy>(b"mpv_terminate_destroy\0")
            .map_err(io::Error::other)?;
        let init = lib
            .get::<unsafe extern "C" fn(Handle) -> c_int>(b"mpv_initialize\0")
            .map_err(io::Error::other)?;
        let set = lib
            .get::<unsafe extern "C" fn(Handle, *const c_char, *const c_char) -> c_int>(
                b"mpv_set_option_string\0",
            )
            .map_err(io::Error::other)?;
        let command = lib
            .get::<unsafe extern "C" fn(Handle, *const *const c_char) -> c_int>(b"mpv_command\0")
            .map_err(io::Error::other)?;
        let wait = lib
            .get::<unsafe extern "C" fn(Handle, f64) -> *const Event>(b"mpv_wait_event\0")
            .map_err(io::Error::other)?;
        let message = lib
            .get::<unsafe extern "C" fn(c_int) -> *const c_char>(b"mpv_error_string\0")
            .map_err(io::Error::other)?;
        let check = |code: c_int| -> io::Result<()> {
            if code < 0 {
                Err(io::Error::other(
                    CStr::from_ptr(message(code)).to_string_lossy().into_owned(),
                ))
            } else {
                Ok(())
            }
        };
        let player = Player {
            handle: NonNull::new(create())
                .ok_or_else(|| io::Error::other("Could not create the clip viewer"))?,
            destroy,
        };
        let handle = player.handle.as_ptr();
        for (name, value) in [
            (c"config", c"no"),
            (c"terminal", c"no"),
            (c"vo", c"gpu"),
            (c"gpu-api", c"d3d11"),
            (c"hwdec", c"auto-safe"),
            (c"vd-lavc-threads", c"2"),
            (c"framedrop", c"vo"),
            (c"demuxer-max-bytes", c"16MiB"),
            (c"osc", c"yes"),
            (c"input-default-bindings", c"yes"),
            (c"input-vo-keyboard", c"yes"),
            (c"keep-open", c"yes"),
            (c"autofit-larger", c"80%x80%"),
            (c"title", c"sClip - ${filename}"),
        ] {
            check(set(handle, name.as_ptr(), value.as_ptr())).map_err(|e| {
                io::Error::other(format!("Playback option {}: {e}", name.to_string_lossy()))
            })?;
        }
        check(init(handle))?;
        check(command(
            handle,
            [c"loadfile".as_ptr(), file.as_ptr(), ptr::null()].as_ptr(),
        ))?;
        loop {
            let event = &*wait(handle, -1.0);
            match event.id {
                1 => return Ok(()), // MPV_EVENT_SHUTDOWN (including a window close).
                7 if !event.data.is_null() => {
                    let end = &*event.data.cast::<EndFile>();
                    if end.reason == 4 {
                        check(end.error)?;
                    }
                }
                _ => {}
            }
        }
    }
}
