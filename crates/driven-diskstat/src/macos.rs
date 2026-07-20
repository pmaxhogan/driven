//! macOS [`DiskBusyProbe`] backend (DESIGN s18.2): IOKit `IOBlockStorageDriver`
//! `Statistics`.
//!
//! # Best-effort caveat (READ THIS)
//!
//! Unlike Linux (`/proc/diskstats` "busy ms") and Windows (PDH `% Disk Time`),
//! macOS exposes no first-class device-busy percentage. The closest signal is
//! `IOBlockStorageDriver`'s `Statistics` dict, whose `Total Time (Read)` /
//! `Total Time (Write)` counters are cumulative NANOSECONDS spent servicing I/O.
//! We sum those across every block-storage driver and treat
//! `total_time_ns_delta / interval_ns` as the busy fraction. Because I/O overlaps
//! (a delta can exceed wall-clock), this fraction can be over-unity; that reads
//! as "saturated", consistent with the other backends.
//!
//! This approximation can OVER-report busy. That direction is SAFE here: the
//! adaptive controller (DESIGN s11.4.7) requires `disk NOT saturated` for BOTH a
//! grow AND a shrink, so a falsely-saturated reading merely holds the pool at its
//! configured start size - i.e. it degrades to today's fixed-pool behaviour, it
//! never strangles the pool below the default. Any uncertainty (no matching
//! service, a missing key, an FFI error) returns [`DiskBusy::Unknown`]
//! (fail-open). This module is compiled but NOT executed on the CI hosts (no
//! Mac); it is validated by `cargo check --target aarch64-apple-darwin`.

use std::ffi::c_void;
use std::os::raw::c_char;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Instant;

use core_foundation::base::TCFType;
use core_foundation::string::CFString;
use core_foundation_sys::base::CFTypeRef;
use core_foundation_sys::dictionary::{CFDictionaryGetValue, CFDictionaryRef};
use core_foundation_sys::number::{kCFNumberSInt64Type, CFNumberGetValue, CFNumberRef};
use core_foundation_sys::string::CFStringRef;

use crate::{DiskBusy, DiskBusyProbe};

// IOKit / Mach scalar types (mach_port.h / IOKitLib.h). All `mach_port_t`
// derivatives are `u32`; `kern_return_t` is `i32`.
#[allow(non_camel_case_types)]
type mach_port_t = u32;
#[allow(non_camel_case_types)]
type io_object_t = mach_port_t;
#[allow(non_camel_case_types)]
type io_iterator_t = mach_port_t;
#[allow(non_camel_case_types)]
type io_registry_entry_t = mach_port_t;
#[allow(non_camel_case_types)]
type kern_return_t = i32;

/// `kIOMainPortDefault` (a.k.a. the legacy `kIOMasterPortDefault`) is the null
/// mach port, value 0.
const K_IO_MAIN_PORT_DEFAULT: mach_port_t = 0;
/// `KERN_SUCCESS`.
const KERN_SUCCESS: kern_return_t = 0;

// IOKit C API (IOKit.framework). Declared directly (matching the precedent in
// driven-power/src/macos.rs) rather than pulling a heavier IOKit binding crate.
#[link(name = "IOKit", kind = "framework")]
extern "C" {
    fn IOServiceMatching(name: *const c_char) -> CFDictionaryRef;
    fn IOServiceGetMatchingServices(
        main_port: mach_port_t,
        matching: CFDictionaryRef,
        existing: *mut io_iterator_t,
    ) -> kern_return_t;
    fn IOIteratorNext(iterator: io_iterator_t) -> io_object_t;
    fn IORegistryEntryCreateCFProperty(
        entry: io_registry_entry_t,
        key: CFStringRef,
        allocator: *const c_void,
        options: u32,
    ) -> CFTypeRef;
    fn IOObjectRelease(object: io_object_t) -> kern_return_t;
}

/// A prior sample: cumulative total-I/O-time (ns) and when it was read.
#[derive(Clone, Copy)]
struct Baseline {
    total_time_ns: u64,
    at: Instant,
}

/// macOS disk-busy reader over IOKit `IOBlockStorageDriver` `Statistics`
/// (DESIGN s18.2, best-effort - see the module docs).
pub struct RealDiskBusyProbe {
    baseline: Mutex<Option<Baseline>>,
}

impl RealDiskBusyProbe {
    /// Build the reader. The `root` is unused on macOS (we aggregate every block
    /// driver rather than resolving the backing device, which IOKit does not
    /// expose as cheaply as Linux `st_dev`) but accepted for signature parity.
    #[must_use]
    pub fn new(_root: PathBuf) -> Self {
        Self {
            baseline: Mutex::new(None),
        }
    }
}

impl DiskBusyProbe for RealDiskBusyProbe {
    fn sample(&self) -> DiskBusy {
        let Some(total_time_ns) = read_total_io_time_ns() else {
            return DiskBusy::Unknown;
        };
        let now = Instant::now();
        let mut guard = self.baseline.lock().unwrap_or_else(|e| e.into_inner());
        let prev = *guard;
        *guard = Some(Baseline {
            total_time_ns,
            at: now,
        });
        drop(guard);

        match prev {
            None => DiskBusy::Unknown,
            Some(prev) => {
                let interval_ns = now.duration_since(prev.at).as_nanos();
                let interval_ns = u64::try_from(interval_ns).unwrap_or(u64::MAX);
                let delta = total_time_ns.saturating_sub(prev.total_time_ns);
                crate::busy_fraction_from_delta(delta, interval_ns)
            }
        }
    }
}

/// Sum `Total Time (Read)` + `Total Time (Write)` (ns) across every
/// `IOBlockStorageDriver`. Returns `None` on any IOKit failure or if no driver
/// reported a usable counter (fail-open).
fn read_total_io_time_ns() -> Option<u64> {
    // "IOBlockStorageDriver" as a NUL-terminated C string.
    const CLASS: &[u8] = b"IOBlockStorageDriver\0";
    let read_key = CFString::from_static_string("Total Time (Read)");
    let write_key = CFString::from_static_string("Total Time (Write)");
    let stats_key = CFString::from_static_string("Statistics");

    unsafe {
        let matching = IOServiceMatching(CLASS.as_ptr() as *const c_char);
        if matching.is_null() {
            return None;
        }
        // IOServiceGetMatchingServices CONSUMES the matching dict's +1 ref, so we
        // must not release `matching` ourselves.
        let mut iter: io_iterator_t = 0;
        if IOServiceGetMatchingServices(K_IO_MAIN_PORT_DEFAULT, matching, &mut iter) != KERN_SUCCESS
        {
            return None;
        }

        let mut total: u64 = 0;
        let mut saw_any = false;
        loop {
            let entry = IOIteratorNext(iter);
            if entry == 0 {
                break;
            }
            let stats = IORegistryEntryCreateCFProperty(
                entry,
                stats_key.as_concrete_TypeRef(),
                std::ptr::null(),
                0,
            );
            if !stats.is_null() {
                let dict = stats as CFDictionaryRef;
                if let Some(r) = dict_i64(dict, read_key.as_concrete_TypeRef()) {
                    total = total.saturating_add(r.max(0) as u64);
                    saw_any = true;
                }
                if let Some(w) = dict_i64(dict, write_key.as_concrete_TypeRef()) {
                    total = total.saturating_add(w.max(0) as u64);
                    saw_any = true;
                }
                // IORegistryEntryCreateCFProperty follows the CREATE rule (+1).
                core_foundation_sys::base::CFRelease(stats);
            }
            IOObjectRelease(entry);
        }
        IOObjectRelease(iter);

        saw_any.then_some(total)
    }
}

/// Read a signed-64-bit CFNumber value for `key` out of a CFDictionary, or
/// `None` if the key is absent or not a number.
///
/// # Safety
/// `dict` must be a valid `CFDictionaryRef` and `key` a valid `CFStringRef`.
unsafe fn dict_i64(dict: CFDictionaryRef, key: CFStringRef) -> Option<i64> {
    let value = CFDictionaryGetValue(dict, key as *const c_void);
    if value.is_null() {
        return None;
    }
    let mut out: i64 = 0;
    let ok = CFNumberGetValue(
        value as CFNumberRef,
        kCFNumberSInt64Type,
        (&mut out as *mut i64).cast(),
    );
    ok.then_some(out)
}
