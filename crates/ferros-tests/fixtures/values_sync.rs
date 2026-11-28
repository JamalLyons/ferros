//! Locks, atomics, lazily set cells, weak references and pins for the formatter
//! integration test.

use std::cell::{OnceCell, UnsafeCell};
use std::ffi::{CStr, OsStr};
use std::fmt::Debug;
use std::hint::black_box;
use std::path::Path;
use std::pin::Pin;
use std::rc::{self, Rc};
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, RwLock, mpsc};
use std::thread;

fn main()
{
    let mutex = Mutex::new(9_u32);
    // Taking a lock once allocates the system mutex a free lock is then read from.
    let released = Mutex::new(10_u32);
    drop(released.lock());
    let rwlock = RwLock::new(8_u32);
    let read_locked = RwLock::new(11_u32);
    let reader = read_locked.read().unwrap();
    let write_locked = RwLock::new(0_u32);
    let mut writer = write_locked.write().unwrap();
    *writer = 15;
    let guarded = Mutex::new(16_u32);
    let guard = guarded.lock().unwrap();
    let atomic_u32 = AtomicU32::new(7);
    let atomic_bool = AtomicBool::new(true);
    let mut pointee = 12_u32;
    let atomic_ptr = AtomicPtr::new(&raw mut pointee);
    let null_ptr: AtomicPtr<u32> = AtomicPtr::new(std::ptr::null_mut());
    let once_set = OnceCell::new();
    let _ = once_set.set(4_u32);
    let once_unset: OnceCell<u32> = OnceCell::new();
    let once_lock = OnceLock::new();
    let _ = once_lock.set(6_u32);
    let once_lock_unset: OnceLock<u32> = OnceLock::new();
    let unsafe_cell = UnsafeCell::new(3_u32);

    let rc = Rc::new(20_u32);
    let rc_weak = Rc::downgrade(&rc);
    let arc = Arc::new(21_u32);
    let arc_weak = Arc::downgrade(&arc);
    let dropped = Arc::new(22_u32);
    let arc_dangling = Arc::downgrade(&dropped);
    drop(dropped);
    let rc_new: rc::Weak<u32> = rc::Weak::new();
    let pinned = Box::pin(5_u32);
    let pinned_ref: Pin<&u32> = Pin::new(&pointee);

    let poisoned = Arc::new(Mutex::new(13_u32));
    let panicking = Arc::clone(&poisoned);
    let _ = thread::spawn(move || {
        let _guard = panicking.lock();
        panic!("poison the mutex");
    })
    .join();

    // Reacquire both locks from their poison errors and keep the guards alive.
    let poisoned_again = Arc::new(Mutex::new(17_u32));
    let poisoned_rwlock = Arc::new(RwLock::new(18_u32));
    let panicking = Arc::clone(&poisoned_again);
    let panicking_writer = Arc::clone(&poisoned_rwlock);
    let _ = thread::spawn(move || {
        let _guard = panicking.lock();
        let _writer = panicking_writer.write();
        panic!("poison both locks");
    })
    .join();
    let poisoned_guard = poisoned_again.lock().unwrap_or_else(PoisonError::into_inner);
    let mut poisoned_writer = poisoned_rwlock.write().unwrap_or_else(PoisonError::into_inner);
    *poisoned_writer = 19;

    // Another thread holds `held` until the process exits.
    let held = Arc::new(Mutex::new(14_u32));
    let holder = Arc::clone(&held);
    let (locked, wait) = mpsc::channel();
    thread::spawn(move || {
        let _guard = holder.lock();
        let _ = locked.send(());
        loop {
            thread::park();
        }
    });
    let _ = wait.recv();

    let rc_path: Rc<Path> = Rc::from(Path::new("/tmp"));
    let arc_os_str: Arc<OsStr> = Arc::from(OsStr::new("os"));
    let rc_c_str: Rc<CStr> = Rc::from(c"cs");

    black_box(&mutex); // BREAK: inspect
    black_box((
        &released,
        &rwlock,
        &read_locked,
        &reader,
        &write_locked,
        &writer,
        &atomic_u32,
        &atomic_bool,
        &atomic_ptr,
        &null_ptr,
    ));
    black_box((&once_set, &once_unset, &once_lock, &once_lock_unset, &unsafe_cell));
    black_box((&rc, &rc_weak, &arc, &arc_weak, &arc_dangling, &rc_new, &pinned, &pinned_ref));
    black_box((&poisoned, &held, &rc_path, &arc_os_str, &rc_c_str));
    black_box((&guarded, &guard, &poisoned_guard, &poisoned_writer));
    inspect_guard_payloads();
}

const _: () = assert!(size_of::<Reading>() == 96 && align_of::<Reading>() == 32);

// Size 96 and alignment 32 differ, so reading the vtable's size slot instead of its
// alignment slot puts the payload at the wrong offset.
#[derive(Debug)]
#[repr(align(32))]
struct Reading
{
    count: u32,
    spare: (u64, u64, u64, u64, u64, u64, u64, u64),
}

fn inspect_guard_payloads()
{
    static LARGE: Mutex<[u8; 2_097_152]> = Mutex::new([0; 2_097_152]);

    let slice: Box<Mutex<[u8]>> = Box::new(Mutex::new([1, 2]));
    let read_slice: Box<RwLock<[u8]>> = Box::new(RwLock::new([3, 4]));
    let write_slice: Box<RwLock<[u8]>> = Box::new(RwLock::new([5, 6]));
    let reading = Reading {
        count: 7,
        spare: (0, 0, 0, 0, 0, 0, 0, 0),
    };
    black_box((reading.count, reading.spare));
    let object: Box<Mutex<dyn Debug>> = Box::new(Mutex::new(reading));
    let read_object: Box<RwLock<dyn Debug>> = Box::new(RwLock::new(Reading {
        count: 8,
        spare: (0, 0, 0, 0, 0, 0, 0, 0),
    }));
    let write_object: Box<RwLock<dyn Debug>> = Box::new(RwLock::new(Reading {
        count: 9,
        spare: (0, 0, 0, 0, 0, 0, 0, 0),
    }));

    let slice_guard = slice.lock().unwrap();
    let slice_reader = read_slice.read().unwrap();
    let slice_writer = write_slice.write().unwrap();
    let object_guard = object.lock().unwrap();
    let object_reader = read_object.read().unwrap();
    let object_writer = write_object.write().unwrap();
    let mut large_guard = LARGE.lock().unwrap();
    large_guard[0] = 7;

    black_box(&slice_guard); // BREAK: guard_payloads
    black_box((
        &slice_reader,
        &slice_writer,
        &object_guard,
        &object_reader,
        &object_writer,
        &large_guard,
    ));
}
