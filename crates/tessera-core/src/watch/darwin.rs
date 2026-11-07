//! Owned FSEvents source shared by the CLI and the native application.
//! The serial dispatch queue sleeps in the kernel when the disk is quiet.
use std::{
    ffi::{CStr, CString, c_char, c_void},
    io,
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
    ptr,
};
#[repr(C)]
struct StreamContext {
    version: isize,
    info: *mut c_void,
    retain: *const c_void,
    release: *const c_void,
    description: *const c_void,
}
type EventCallback =
    unsafe extern "C" fn(*mut c_void, *mut c_void, usize, *mut c_void, *const u32, *const u64);
#[link(name = "CoreServices", kind = "framework")]
unsafe extern "C" {
    fn FSEventStreamCreate(
        allocator: *const c_void,
        callback: EventCallback,
        context: *mut StreamContext,
        paths: *const c_void,
        since: u64,
        latency: f64,
        flags: u32,
    ) -> *mut c_void;
    fn FSEventStreamSetDispatchQueue(stream: *mut c_void, queue: *mut c_void);
    fn FSEventStreamStart(stream: *mut c_void) -> u8;
    fn FSEventStreamStop(stream: *mut c_void);
    fn FSEventStreamInvalidate(stream: *mut c_void);
    fn FSEventStreamRelease(stream: *mut c_void);
}
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFStringCreateWithCString(
        allocator: *const c_void,
        string: *const c_char,
        encoding: u32,
    ) -> *const c_void;
    fn CFArrayCreate(
        allocator: *const c_void,
        values: *const *const c_void,
        count: isize,
        callbacks: *const c_void,
    ) -> *const c_void;
    fn CFRelease(value: *const c_void);
    static kCFTypeArrayCallBacks: u8;
}
unsafe extern "C" {
    fn dispatch_queue_create(label: *const c_char, attr: *const c_void) -> *mut c_void;
    fn dispatch_sync_f(
        queue: *mut c_void,
        context: *mut c_void,
        function: unsafe extern "C" fn(*mut c_void),
    );
    fn dispatch_release(object: *mut c_void);
}
type CallbackFn = dyn Fn(&Path, u32) + Send + Sync;
struct Callback(Box<CallbackFn>);
pub struct Stream {
    stream: *mut c_void,
    queue: *mut c_void,
    context: *mut Callback,
}
// The source owns a serial queue, and teardown drains it before freeing context.
unsafe impl Send for Stream {}
impl Stream {
    pub fn new(
        root: &Path,
        callback: impl Fn(&Path, u32) + Send + Sync + 'static,
    ) -> io::Result<Self> {
        let root = CString::new(root.as_os_str().as_bytes()).map_err(io::Error::other)?;
        let string = unsafe { CFStringCreateWithCString(ptr::null(), root.as_ptr(), 0x08000100) };
        if string.is_null() {
            return Err(io::Error::other("FSEvents root is not representable"));
        }
        let array = unsafe {
            CFArrayCreate(
                ptr::null(),
                &string,
                1,
                ptr::addr_of!(kCFTypeArrayCallBacks).cast(),
            )
        };
        unsafe { CFRelease(string) };
        let context = Box::into_raw(Box::new(Callback(Box::new(callback))));
        let mut info = StreamContext {
            version: 0,
            info: context.cast(),
            retain: ptr::null(),
            release: ptr::null(),
            description: ptr::null(),
        };
        let queue = unsafe { dispatch_queue_create(c"tessera.fsevents".as_ptr(), ptr::null()) };
        // NoDefer | WatchRoot | FileEvents. Raw path strings preserve filesystem bytes.
        let stream = unsafe {
            FSEventStreamCreate(
                ptr::null(),
                receive,
                &mut info,
                array,
                u64::MAX,
                0.25,
                2 | 4 | 16,
            )
        };
        unsafe { CFRelease(array) };
        if stream.is_null() {
            unsafe {
                drop(Box::from_raw(context));
                dispatch_release(queue)
            };
            return Err(io::Error::other("could not create FSEvents stream"));
        }
        let result = Self {
            stream,
            queue,
            context,
        };
        unsafe { FSEventStreamSetDispatchQueue(stream, queue) };
        if unsafe { FSEventStreamStart(stream) } == 0 {
            return Err(io::Error::other("could not start FSEvents stream"));
        }
        Ok(result)
    }
}
unsafe extern "C" fn receive(
    _stream: *mut c_void,
    info: *mut c_void,
    count: usize,
    paths: *mut c_void,
    flags: *const u32,
    _ids: *const u64,
) {
    if info.is_null() || paths.is_null() {
        return;
    }
    let callback = unsafe { &*info.cast::<Callback>() };
    for i in 0..count {
        let path = unsafe { *paths.cast::<*const c_char>().add(i) };
        if path.is_null() {
            continue;
        }
        let path = PathBuf::from(std::ffi::OsString::from_vec(
            unsafe { CStr::from_ptr(path) }.to_bytes().to_vec(),
        ));
        (callback.0)(&path, unsafe { *flags.add(i) });
    }
}
unsafe extern "C" fn noop(_: *mut c_void) {}
impl Drop for Stream {
    fn drop(&mut self) {
        unsafe {
            FSEventStreamStop(self.stream);
            FSEventStreamInvalidate(self.stream);
            dispatch_sync_f(self.queue, ptr::null_mut(), noop);
            FSEventStreamRelease(self.stream);
            dispatch_release(self.queue);
            drop(Box::from_raw(self.context));
        }
    }
}
