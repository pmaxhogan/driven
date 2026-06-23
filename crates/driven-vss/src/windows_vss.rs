//! Windows VSS COM backend (ROADMAP M3.5, DESIGN s5.3).
//!
//! Implements [`VssSnapshot`] - an RAII handle around one Volume Shadow Copy -
//! plus [`is_elevated`]. This is the only `unsafe` COM code in Driven.
//!
//! # Hand-declared `IVssBackupComponents`
//!
//! `IVssBackupComponents` and its `CreateVssBackupComponents` factory are NOT
//! in the `windows` 0.62 bindings (microsoft/win32metadata#2095, open: the
//! interface and the factory were never projected from the SDK metadata). The
//! supporting types - `IVssAsync`, `VSS_SNAPSHOT_PROP`, the `VSS_CTX_*` /
//! `VSS_SS_*` constants, `VSS_BACKUP_TYPE` - DO exist. So we hand-declare the
//! `IVssBackupComponents` vtable with [`windows::core::interface`] using its
//! real IID (`665c1d5f-c218-414d-a05d-7fef5f9d5c86`, from `vsbackup.h`), and
//! load the factory (`CreateVssBackupComponentsInternal` in `vssapi.dll`,
//! which is what `CreateVssBackupComponents` resolves to) via
//! `GetProcAddress` at runtime.
//!
//! COM dispatches by vtable SLOT INDEX, so the interface declares ALL 48
//! methods in their exact `vsbackup.h` order. The ~38 methods Driven never
//! calls are declared as placeholder stubs (`_slotNN`) that only hold their
//! slot - their parameter types are irrelevant because we never invoke them.
//! Only the ~11 methods we actually call carry real signatures. The method
//! order and IID were lifted from `vsbackup.h` (via the winapi crate's RIDL
//! declaration) and cross-checked against MS Learn - NOT reconstructed from
//! memory: a wrong slot or IID compiles green and fails only at runtime.
//!
//! # The snapshot sequence (DESIGN s5.3)
//!
//! `CoInitializeEx` -> `CreateVssBackupComponents` -> `InitializeForBackup`
//! -> `SetContext(VSS_CTX_BACKUP)` -> `SetBackupState` ->
//! `GatherWriterMetadata` (async; `Wait` + `QueryStatus`) ->
//! `StartSnapshotSet` -> `AddToSnapshotSet(volume)` -> `PrepareForBackup`
//! (async) -> `DoSnapshotSet` (async) -> `GetSnapshotProperties` for the
//! `\\?\GLOBALROOT\Device\...` device path. [`Drop`] runs `BackupComplete`
//! (async) and `Release`s the components; the shadow copy is released because
//! we use the `VSS_CTX_BACKUP` context (a non-persistent, auto-release
//! context), and the recorded GUID lets a later run delete a leak explicitly
//! via [`VssSnapshot::delete_by_id`].

use std::ffi::c_void;
use std::path::{Path, PathBuf};

use windows::core::{Interface, GUID, HRESULT, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, E_ACCESSDENIED, HANDLE, S_OK};
use windows::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
use windows::Win32::Storage::Vss::{IVssAsync, VSS_BT_FULL, VSS_CTX_BACKUP, VSS_SNAPSHOT_PROP};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use crate::{SnapshotHandle, VssError};

/// Wait timeout for each async VSS step. `INFINITE` (`u32::MAX`): VSS async
/// ops are bounded by the OS, and a hung writer is rare enough that blocking
/// the per-op task (already a blocking-friendly path) is acceptable for V1.
const WAIT_INFINITE: u32 = u32::MAX;

// The hand-declared `IVssBackupComponents` vtable lives in its own module so a
// module-level `#![allow(non_snake_case)]` can keep the method names matching
// the Win32 header (the `#[interface]` macro rejects any sibling attribute on
// the trait, so a per-item `#[allow]` is impossible). The method names mirror
// the `vsbackup.h` vtable (PascalCase) so the slot mapping is auditable.
mod ffi {
    #![allow(non_snake_case)]

    use std::ffi::c_void;

    use windows::core::{interface, IUnknown, IUnknown_Vtbl, GUID, HRESULT, PCWSTR};
    use windows::Win32::Storage::Vss::{VSS_BACKUP_TYPE, VSS_SNAPSHOT_PROP};

    /// The `IVssBackupComponents` IID, from `vsbackup.h`
    /// (`665c1d5f-c218-414d-a05d-7fef5f9d5c86`). MUST be exact - a wrong IID
    /// makes the factory's returned pointer mis-cast at runtime.
    #[interface("665c1d5f-c218-414d-a05d-7fef5f9d5c86")]
    pub(super) unsafe trait IVssBackupComponents: IUnknown {
        // The methods are declared in EXACT vsbackup.h vtable order. Slots
        // Driven never calls are `_slotNN(&self) -> HRESULT` placeholders that
        // only hold their offset (params irrelevant - never dispatched). Slots
        // Driven calls carry real signatures. Slot numbers below are 1-based
        // AFTER IUnknown.
        unsafe fn _slot01_GetWriterComponentsCount(&self) -> HRESULT; // GetWriterComponentsCount
        unsafe fn _slot02_GetWriterComponents(&self) -> HRESULT; // GetWriterComponents
        /// 3: InitializeForBackup(bstrXML: optional). We pass null.
        pub(super) unsafe fn InitializeForBackup(&self, bstrxml: PCWSTR) -> HRESULT;
        /// 4: SetBackupState(bSelectComponents, bBackupBootableSystemState, type, bPartialFileSupport).
        pub(super) unsafe fn SetBackupState(
            &self,
            bselectcomponents: bool,
            bbackupbootablesystemstate: bool,
            backuptype: VSS_BACKUP_TYPE,
            bpartialfilesupport: bool,
        ) -> HRESULT;
        unsafe fn _slot05_InitializeForRestore(&self) -> HRESULT; // InitializeForRestore
        unsafe fn _slot06_SetRestoreState(&self) -> HRESULT; // SetRestoreState
        /// 7: GatherWriterMetadata(ppAsync). Async: Wait + QueryStatus.
        pub(super) unsafe fn GatherWriterMetadata(&self, ppasync: *mut *mut c_void) -> HRESULT;
        unsafe fn _slot08_GetWriterMetadataCount(&self) -> HRESULT; // GetWriterMetadataCount
        unsafe fn _slot09_GetWriterMetadata(&self) -> HRESULT; // GetWriterMetadata
        unsafe fn _slot10_FreeWriterMetadata(&self) -> HRESULT; // FreeWriterMetadata
        unsafe fn _slot11_AddComponent(&self) -> HRESULT; // AddComponent
        /// 12: PrepareForBackup(ppAsync). Async.
        pub(super) unsafe fn PrepareForBackup(&self, ppasync: *mut *mut c_void) -> HRESULT;
        unsafe fn _slot13_AbortBackup(&self) -> HRESULT; // AbortBackup
        unsafe fn _slot14_GatherWriterStatus(&self) -> HRESULT; // GatherWriterStatus
        unsafe fn _slot15_GetWriterStatusCount(&self) -> HRESULT; // GetWriterStatusCount
        unsafe fn _slot16_FreeWriterStatus(&self) -> HRESULT; // FreeWriterStatus
        unsafe fn _slot17_GetWriterStatus(&self) -> HRESULT; // GetWriterStatus
        unsafe fn _slot18_SetBackupSucceeded(&self) -> HRESULT; // SetBackupSucceeded
        unsafe fn _slot19_SetBackupOptions(&self) -> HRESULT; // SetBackupOptions
        unsafe fn _slot20_SetSelectedForRestore(&self) -> HRESULT; // SetSelectedForRestore
        unsafe fn _slot21_SetRestoreOptions(&self) -> HRESULT; // SetRestoreOptions
        unsafe fn _slot22_SetAdditionalRestores(&self) -> HRESULT; // SetAdditionalRestores
        unsafe fn _slot23_SetPreviousBackupStamp(&self) -> HRESULT; // SetPreviousBackupStamp
        unsafe fn _slot24_SaveAsXML(&self) -> HRESULT; // SaveAsXML
        /// 25: BackupComplete(ppAsync). Async; run in Drop.
        pub(super) unsafe fn BackupComplete(&self, ppasync: *mut *mut c_void) -> HRESULT;
        unsafe fn _slot26_AddAlternativeLocationMapping(&self) -> HRESULT; // AddAlternativeLocationMapping
        unsafe fn _slot27_AddRestoreSubcomponent(&self) -> HRESULT; // AddRestoreSubcomponent
        unsafe fn _slot28_SetFileRestoreStatus(&self) -> HRESULT; // SetFileRestoreStatus
        unsafe fn _slot29_AddNewTarget(&self) -> HRESULT; // AddNewTarget
        unsafe fn _slot30_SetRangesFilePath(&self) -> HRESULT; // SetRangesFilePath
        unsafe fn _slot31_PreRestore(&self) -> HRESULT; // PreRestore
        unsafe fn _slot32_PostRestore(&self) -> HRESULT; // PostRestore
        /// 33: SetContext(lContext: LONG).
        pub(super) unsafe fn SetContext(&self, lcontext: i32) -> HRESULT;
        /// 34: StartSnapshotSet(pSnapshotSetId: out GUID).
        pub(super) unsafe fn StartSnapshotSet(&self, psnapshotsetid: *mut GUID) -> HRESULT;
        /// 35: AddToSnapshotSet(pwszVolumeName, ProviderId, pidSnapshot: out GUID).
        pub(super) unsafe fn AddToSnapshotSet(
            &self,
            pwszvolumename: PCWSTR,
            providerid: GUID,
            pidsnapshot: *mut GUID,
        ) -> HRESULT;
        /// 36: DoSnapshotSet(ppAsync). Async; commits the set.
        pub(super) unsafe fn DoSnapshotSet(&self, ppasync: *mut *mut c_void) -> HRESULT;
        /// 37: DeleteSnapshots(SourceObjectId, eSourceObjectType, bForceDelete,
        /// plDeletedSnapshots: out, pNondeletedSnapshotID: out).
        pub(super) unsafe fn DeleteSnapshots(
            &self,
            sourceobjectid: GUID,
            esourceobjecttype: i32,
            bforcedelete: bool,
            pldeletedsnapshots: *mut i32,
            pnondeletedsnapshotid: *mut GUID,
        ) -> HRESULT;
        unsafe fn _slot38_ImportSnapshots(&self) -> HRESULT; // ImportSnapshots
        unsafe fn _slot39_BreakSnapshotSet(&self) -> HRESULT; // BreakSnapshotSet
        /// 40: GetSnapshotProperties(SnapshotId, pProp: out VSS_SNAPSHOT_PROP).
        pub(super) unsafe fn GetSnapshotProperties(
            &self,
            snapshotid: GUID,
            pprop: *mut VSS_SNAPSHOT_PROP,
        ) -> HRESULT;
        // 41..=48 are never called; declared as stubs to keep the (truncated)
        // vtable honest in case a future method is added below them.
        unsafe fn _slot41_Query(&self) -> HRESULT; // Query
        unsafe fn _slot42_IsVolumeSupported(&self) -> HRESULT; // IsVolumeSupported
        unsafe fn _slot43_DisableWriterClasses(&self) -> HRESULT; // DisableWriterClasses
        unsafe fn _slot44_EnableWriterClasses(&self) -> HRESULT; // EnableWriterClasses
        unsafe fn _slot45_DisableWriterInstances(&self) -> HRESULT; // DisableWriterInstances
        unsafe fn _slot46_ExposeSnapshot(&self) -> HRESULT; // ExposeSnapshot
        unsafe fn _slot47_RevertToSnapshot(&self) -> HRESULT; // RevertToSnapshot
        unsafe fn _slot48_QueryRevertStatus(&self) -> HRESULT; // QueryRevertStatus
    }
}

use ffi::IVssBackupComponents;

/// `VSS_OBJECT_SNAPSHOT` discriminant for `DeleteSnapshots`
/// (`VSS_OBJECT_TYPE::VSS_OBJECT_SNAPSHOT == 3`). We delete by individual
/// snapshot GUID, so the source-object type is the snapshot type.
const VSS_OBJECT_SNAPSHOT: i32 = 3;

/// Factory signature: `CreateVssBackupComponentsInternal(ppBackup) -> HRESULT`.
type CreateFn = unsafe extern "system" fn(*mut *mut c_void) -> HRESULT;

/// An RAII guard that pairs a successful `CoInitializeEx` with its
/// `CoUninitialize`. COM is initialised per snapshot creation on the calling
/// thread (the executor's per-op blocking task) and torn down when the guard
/// drops.
struct ComApartment {
    /// `true` when WE initialised COM (so we own the `CoUninitialize`). If the
    /// thread already had COM up (`RPC_E_CHANGED_MODE` / `S_FALSE`), we leave
    /// it alone.
    owned: bool,
}

impl ComApartment {
    fn enter() -> Self {
        // SAFETY: CoInitializeEx is always safe to call; we record whether we
        // were the initialiser so Drop only uninitialises our own init.
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        // S_OK => we initialised. S_FALSE => already initialised on this thread
        // (still balanced by an uninit). RPC_E_CHANGED_MODE => someone set a
        // different apartment; do NOT uninitialise.
        let owned = hr == S_OK || hr.0 == 1 /* S_FALSE */;
        Self { owned }
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        if self.owned {
            // SAFETY: balanced against our own successful CoInitializeEx.
            unsafe { CoUninitialize() };
        }
    }
}

/// An RAII handle around one Volume Shadow Copy (DESIGN s5.3).
///
/// Created by [`VssSnapshot::create`]; reading a locked file goes through
/// [`VssSnapshot::map_path`]. [`Drop`] runs `BackupComplete` and releases the
/// `IVssBackupComponents`; under the `VSS_CTX_BACKUP` context the shadow copy
/// auto-releases, and the recorded GUID lets a later run delete a leak.
pub struct VssSnapshot {
    /// The live backup-components COM object. `Some` until dropped.
    components: Option<IVssBackupComponents>,
    /// The shadow-copy GUID (`VSS_SNAPSHOT_PROP::m_SnapshotId`).
    snapshot_id: GUID,
    /// `\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopyN` device root.
    device_root: String,
    /// The original volume label (`C:`) this snapshot covers, used to map a
    /// live path to its snapshot-relative remainder.
    volume_label: String,
    /// Keeps COM initialised for the lifetime of the components object.
    _com: ComApartment,
}

// The COM object is single-threaded-affine in general, but our usage pins one
// VssSnapshot to one volume and access is serialised behind the provider's
// Mutex. We mark it Send so the provider cache (Mutex<HashMap<.., VssSnapshot>>)
// can live behind an Arc shared across the executor's tasks; all real method
// calls happen under the provider lock, never concurrently.
unsafe impl Send for VssSnapshot {}

impl VssSnapshot {
    /// Create a shadow copy of the volume named by `volume_letter` (`"C:"` or
    /// `"C:\\"`) and return a handle exposing its `\\?\GLOBALROOT` device root.
    ///
    /// Runs the full DESIGN s5.3 COM sequence. Any failure maps to a
    /// [`VssError`] the caller degrades on (skip-the-locked-file); this never
    /// panics.
    pub fn create(volume_letter: &str) -> Result<Self, VssError> {
        let volume_label = normalise_volume_label(volume_letter)?;
        // VSS wants the volume as a mount point with a trailing backslash.
        let volume_mount = format!("{volume_label}\\");

        let com = ComApartment::enter();
        let components = create_backup_components()?;

        // SAFETY: every call below dispatches a real vtable slot on a live COM
        // object we just created; pointers are stack locals valid for the call.
        unsafe {
            components
                .InitializeForBackup(PCWSTR::null())
                .ok()
                .map_err(|e| com_err("InitializeForBackup", e))?;

            components
                .SetContext(VSS_CTX_BACKUP.0)
                .ok()
                .map_err(|e| com_err("SetContext", e))?;

            components
                .SetBackupState(false, false, VSS_BT_FULL, false)
                .ok()
                .map_err(|e| com_err("SetBackupState", e))?;

            // GatherWriterMetadata is required before PrepareForBackup even for
            // a writer-less volume snapshot.
            gather_async(&components, "GatherWriterMetadata", |c, p| {
                c.GatherWriterMetadata(p)
            })?;

            // StartSnapshotSet -> AddToSnapshotSet(volume) -> PrepareForBackup
            // -> DoSnapshotSet.
            let mut snapshot_set_id = GUID::zeroed();
            components
                .StartSnapshotSet(&mut snapshot_set_id)
                .ok()
                .map_err(|e| com_err("StartSnapshotSet", e))?;

            let mut snapshot_id = GUID::zeroed();
            let vol_wide = to_wide(&volume_mount);
            components
                .AddToSnapshotSet(
                    PCWSTR(vol_wide.as_ptr()),
                    // GUID_NULL = "let VSS pick the default provider".
                    GUID::zeroed(),
                    &mut snapshot_id,
                )
                .ok()
                .map_err(|e| com_err("AddToSnapshotSet", e))?;

            gather_async(&components, "PrepareForBackup", |c, p| {
                c.PrepareForBackup(p)
            })?;
            gather_async(&components, "DoSnapshotSet", |c, p| c.DoSnapshotSet(p))?;

            // Pull the device object path for the committed snapshot.
            let mut prop = VSS_SNAPSHOT_PROP::default();
            components
                .GetSnapshotProperties(snapshot_id, &mut prop)
                .ok()
                .map_err(|e| com_err("GetSnapshotProperties", e))?;

            let device_root = pwstr_to_string(prop.m_pwszSnapshotDeviceObject);
            // Free the allocated string members. There is no projected
            // `VssFreeSnapshotProperties`, so free the individual `CoTaskMem`
            // strings VSS allocated (it uses the COM task allocator).
            free_snapshot_prop_strings(&prop);

            if device_root.is_empty() {
                // No device path => unusable snapshot; release and degrade.
                let snap = Self {
                    components: Some(components),
                    snapshot_id,
                    device_root: String::new(),
                    volume_label: volume_label.clone(),
                    _com: com,
                };
                drop(snap);
                return Err(VssError::Com {
                    step: "GetSnapshotProperties",
                    detail: "empty device object path".to_string(),
                });
            }

            Ok(Self {
                components: Some(components),
                snapshot_id,
                device_root,
                volume_label,
                _com: com,
            })
        }
    }

    /// The `\\?\GLOBALROOT\Device\...` device root of this snapshot.
    pub fn root_path(&self) -> PathBuf {
        PathBuf::from(&self.device_root)
    }

    /// The recorded ownership handle (GUID + device root) for the orphan
    /// ledger.
    pub fn handle(&self) -> SnapshotHandle {
        SnapshotHandle {
            snapshot_id: guid_to_braced(&self.snapshot_id),
            device_root: self.device_root.clone(),
        }
    }

    /// The shadow-copy GUID as a braced `{...}` string (for the orphan ledger).
    pub fn snapshot_id_string(&self) -> String {
        guid_to_braced(&self.snapshot_id)
    }

    /// Map a live path on this snapshot's volume to its path under the shadow
    /// copy device root. `C:\Users\me\f.pst` ->
    /// `\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopyN\Users\me\f.pst`.
    ///
    /// Errors when `live_path` is not on the snapshotted volume.
    pub fn map_path(&self, live_path: &Path) -> Result<PathBuf, VssError> {
        let live = live_path.to_string_lossy();
        // Strip a leading `\\?\` extended prefix if present.
        let live_stripped = live
            .strip_prefix(r"\\?\")
            .or_else(|| live.strip_prefix(r"\\.\"))
            .unwrap_or(&live);

        // The remainder after the `C:` volume label, e.g. `\Users\me\f.pst`.
        // `volume_label` is already stored uppercased by `normalise_volume_label`,
        // so compare against the uppercased live path.
        let upper = live_stripped.to_ascii_uppercase();
        let prefix = self.volume_label.as_str(); // "C:"
        let remainder = upper
            .strip_prefix(prefix)
            .ok_or_else(|| VssError::PathNotOnVolume(live.to_string()))?;
        // Use the ORIGINAL-cased remainder (NTFS is case-insensitive for open,
        // but keep the user's casing for readability/logging).
        let orig_remainder = &live_stripped[prefix.len()..];
        debug_assert_eq!(orig_remainder.len(), remainder.len());

        // Join: device_root + remainder (remainder starts with a separator).
        let mut mapped = self.device_root.clone();
        // device_root has no trailing separator; remainder begins with `\`.
        if !orig_remainder.starts_with('\\') && !orig_remainder.starts_with('/') {
            mapped.push('\\');
        }
        mapped.push_str(orig_remainder);
        Ok(PathBuf::from(mapped))
    }

    /// Delete a shadow copy by its recorded GUID string (orphan cleanup).
    /// Creates a throwaway `IVssBackupComponents` to issue `DeleteSnapshots`.
    /// A not-found snapshot is treated as already-gone (`Ok`).
    pub fn delete_by_id(snapshot_id: &str) -> Result<(), VssError> {
        let guid = parse_braced_guid(snapshot_id)
            .ok_or_else(|| VssError::InvalidVolume(snapshot_id.to_string()))?;
        let _com = ComApartment::enter();
        let components = create_backup_components()?;
        // SAFETY: live COM object; out-params are stack locals.
        unsafe {
            components
                .InitializeForBackup(PCWSTR::null())
                .ok()
                .map_err(|e| com_err("InitializeForBackup", e))?;
            components
                .SetContext(VSS_CTX_BACKUP.0)
                .ok()
                .map_err(|e| com_err("SetContext", e))?;
            let mut deleted: i32 = 0;
            let mut nondeleted = GUID::zeroed();
            let hr = components.DeleteSnapshots(
                guid,
                VSS_OBJECT_SNAPSHOT,
                true,
                &mut deleted,
                &mut nondeleted,
            );
            // VSS_E_OBJECT_NOT_FOUND => already gone; treat as success.
            if hr == S_OK || is_object_not_found(hr) {
                Ok(())
            } else {
                Err(VssError::Com {
                    step: "DeleteSnapshots",
                    detail: format!("hr=0x{:08x}", hr.0),
                })
            }
        }
    }
}

impl Drop for VssSnapshot {
    fn drop(&mut self) {
        if let Some(components) = self.components.take() {
            // SAFETY: live components object; BackupComplete is an async op we
            // wait on. Errors here are logged, never propagated (Drop).
            unsafe {
                if let Err(err) =
                    gather_async(&components, "BackupComplete", |c, p| c.BackupComplete(p))
                {
                    tracing::warn!(%err, "VSS: BackupComplete during release failed");
                }
            }
            // Releasing the components object (and the VSS_CTX_BACKUP context's
            // auto-release) drops the shadow copy. The explicit `drop` makes the
            // ordering clear; `_com` (CoUninitialize) drops after.
            drop(components);
            tracing::debug!(snapshot = %guid_to_braced(&self.snapshot_id), "VSS: snapshot released");
        }
    }
}

// -----------------------------------------------------------------------------
// Elevation detection
// -----------------------------------------------------------------------------

/// `true` when the current process token reports elevation
/// (`TokenElevation`). Any failure reads as NOT elevated (conservative
/// degrade).
pub fn is_elevated() -> bool {
    // SAFETY: standard OpenProcessToken + GetTokenInformation dance; the token
    // handle is closed before return on every path.
    unsafe {
        let process = GetCurrentProcess();
        let mut token = HANDLE::default();
        if OpenProcessToken(process, TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION::default();
        let mut ret_len = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elevation as *mut _ as *mut c_void),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut ret_len,
        )
        .is_ok();
        let _ = CloseHandle(token);
        ok && elevation.TokenIsElevated != 0
    }
}

// -----------------------------------------------------------------------------
// COM helpers
// -----------------------------------------------------------------------------

/// Load `CreateVssBackupComponentsInternal` from `vssapi.dll` and call it,
/// returning the live `IVssBackupComponents`. Maps `E_ACCESSDENIED` (the
/// not-elevated / no-backup-privilege case) to [`VssError::Unavailable`] so the
/// caller degrades rather than erroring.
fn create_backup_components() -> Result<IVssBackupComponents, VssError> {
    // SAFETY: LoadLibraryW + GetProcAddress on a fixed, OS-provided DLL; the
    // returned function pointer matches CreateFn's documented signature.
    unsafe {
        let dll = LoadLibraryW(windows::core::w!("vssapi.dll"))
            .map_err(|e| VssError::Init(format!("LoadLibrary(vssapi.dll): {e}")))?;
        let proc = GetProcAddress(dll, windows::core::s!("CreateVssBackupComponentsInternal"));
        let Some(proc) = proc else {
            return Err(VssError::Init(
                "CreateVssBackupComponentsInternal not found in vssapi.dll".to_string(),
            ));
        };
        let create: CreateFn = std::mem::transmute(proc);
        let mut raw: *mut c_void = std::ptr::null_mut();
        let hr = create(&mut raw);
        if hr == E_ACCESSDENIED {
            return Err(VssError::Unavailable(
                "VSS access denied (process lacks backup privilege)",
            ));
        }
        if hr != S_OK || raw.is_null() {
            return Err(VssError::Init(format!(
                "CreateVssBackupComponents hr=0x{:08x}",
                hr.0
            )));
        }
        // Adopt the raw IUnknown-derived pointer as our interface.
        Ok(IVssBackupComponents::from_raw(raw))
    }
}

/// Run a VSS async op: call the producer, then `Wait(INFINITE)` and
/// `QueryStatus`, mapping a non-`S_OK` (and non-`VSS_S_ASYNC_FINISHED`) status
/// to a [`VssError::Com`].
///
/// # Safety
/// `components` must be a live `IVssBackupComponents`; `op` must dispatch a
/// real async-producing vtable slot.
unsafe fn gather_async(
    components: &IVssBackupComponents,
    step: &'static str,
    op: impl Fn(&IVssBackupComponents, *mut *mut c_void) -> HRESULT,
) -> Result<(), VssError> {
    let mut raw: *mut c_void = std::ptr::null_mut();
    // SAFETY: caller guarantees a live object + valid async-producing slot.
    let hr = op(components, &mut raw);
    hr.ok().map_err(|e| com_err(step, e))?;
    if raw.is_null() {
        // Some steps may legitimately return no async object (already done).
        return Ok(());
    }
    // SAFETY: a non-null async pointer is an IVssAsync we now own.
    let async_obj = unsafe { IVssAsync::from_raw(raw) };
    // SAFETY: live IVssAsync; Wait/QueryStatus take stack-local out-params.
    unsafe {
        async_obj
            .Wait(WAIT_INFINITE)
            .map_err(|e| com_err(step, e))?;
    }
    let mut hr_result = HRESULT(0);
    let mut reserved = 0i32;
    // SAFETY: live IVssAsync; out-params are stack locals valid for the call.
    unsafe {
        async_obj
            .QueryStatus(&mut hr_result, &mut reserved)
            .map_err(|e| com_err(step, e))?;
    }
    // VSS_S_ASYNC_FINISHED == 0x4230A; S_OK also acceptable.
    const VSS_S_ASYNC_FINISHED: i32 = 0x0004_230A;
    if hr_result == S_OK || hr_result.0 == VSS_S_ASYNC_FINISHED {
        Ok(())
    } else {
        Err(VssError::Com {
            step,
            detail: format!("async status hr=0x{:08x}", hr_result.0),
        })
    }
}

/// Build a `VssError::Com` from a windows-core error.
fn com_err(step: &'static str, e: windows_core::Error) -> VssError {
    VssError::Com {
        step,
        detail: e.to_string(),
    }
}

/// `true` when an HRESULT is `VSS_E_OBJECT_NOT_FOUND` (0x80042308) - treated as
/// already-deleted during orphan cleanup.
fn is_object_not_found(hr: HRESULT) -> bool {
    hr.0 as u32 == 0x8004_2308
}

// -----------------------------------------------------------------------------
// String / GUID helpers
// -----------------------------------------------------------------------------

/// Normalise a `"C:"` / `"C:\\"` / `"c"` volume spec to the canonical `"C:"`.
fn normalise_volume_label(input: &str) -> Result<String, VssError> {
    let trimmed = input.trim().trim_end_matches(['\\', '/']);
    let mut chars = trimmed.chars();
    match (chars.next(), chars.next(), chars.next()) {
        // "C" -> "C:"
        (Some(c), None, None) if c.is_ascii_alphabetic() => {
            Ok(format!("{}:", c.to_ascii_uppercase()))
        }
        // "C:" -> "C:"
        (Some(c), Some(':'), None) if c.is_ascii_alphabetic() => {
            Ok(format!("{}:", c.to_ascii_uppercase()))
        }
        _ => Err(VssError::InvalidVolume(input.to_string())),
    }
}

/// UTF-16, null-terminated, for a Win32 wide-string argument.
fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Read a null-terminated wide string VSS allocated; empty for a null pointer.
fn pwstr_to_string(p: *const u16) -> String {
    if p.is_null() {
        return String::new();
    }
    // SAFETY: VSS guarantees a null-terminated string; we read up to the NUL.
    unsafe {
        let mut len = 0usize;
        while *p.add(len) != 0 {
            len += 1;
        }
        let slice = std::slice::from_raw_parts(p, len);
        String::from_utf16_lossy(slice)
    }
}

/// Free the `CoTaskMem`-allocated string members of a `VSS_SNAPSHOT_PROP`.
/// There is no projected `VssFreeSnapshotProperties`; VSS uses the COM task
/// allocator for these strings, so `CoTaskMemFree` each non-null pointer.
fn free_snapshot_prop_strings(prop: &VSS_SNAPSHOT_PROP) {
    use windows::Win32::System::Com::CoTaskMemFree;
    let ptrs = [
        prop.m_pwszSnapshotDeviceObject,
        prop.m_pwszOriginalVolumeName,
        prop.m_pwszOriginatingMachine,
        prop.m_pwszServiceMachine,
        prop.m_pwszExposedName,
        prop.m_pwszExposedPath,
    ];
    for p in ptrs {
        if !p.is_null() {
            // SAFETY: each is either null (skipped) or a CoTaskMem allocation
            // VSS handed us in GetSnapshotProperties; freed exactly once.
            unsafe { CoTaskMemFree(Some(p as *const c_void)) };
        }
    }
}

/// Format a GUID as a braced `{XXXXXXXX-XXXX-...}` string.
fn guid_to_braced(g: &GUID) -> String {
    format!("{{{:?}}}", g)
}

/// Parse a braced `{...}` (or bare) GUID string back to a [`GUID`].
fn parse_braced_guid(s: &str) -> Option<GUID> {
    let trimmed = s.trim().trim_start_matches('{').trim_end_matches('}');
    GUID::try_from(trimmed).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalise_volume_label_variants() {
        assert_eq!(normalise_volume_label("C:").unwrap(), "C:");
        assert_eq!(normalise_volume_label("c:\\").unwrap(), "C:");
        assert_eq!(normalise_volume_label("d").unwrap(), "D:");
        assert_eq!(normalise_volume_label(" E:\\ ").unwrap(), "E:");
        assert!(normalise_volume_label("not a volume").is_err());
        assert!(normalise_volume_label("").is_err());
    }

    #[test]
    fn to_wide_is_null_terminated() {
        let w = to_wide("C:");
        assert_eq!(w, vec![b'C' as u16, b':' as u16, 0]);
    }

    #[test]
    fn map_path_joins_remainder_under_device_root() {
        // Build a snapshot by hand (no COM) to test pure path mapping.
        let snap = VssSnapshot {
            components: None,
            snapshot_id: GUID::zeroed(),
            device_root: r"\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopy1".to_string(),
            volume_label: "C:".to_string(),
            _com: ComApartment { owned: false },
        };
        let mapped = snap
            .map_path(Path::new(r"C:\Users\me\Outlook.pst"))
            .unwrap();
        assert_eq!(
            mapped,
            PathBuf::from(r"\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopy1\Users\me\Outlook.pst")
        );
        // A path on a different volume errors.
        assert!(snap.map_path(Path::new(r"D:\other\f.txt")).is_err());
        // Don't run Drop's BackupComplete on this synthetic (no components).
        std::mem::forget(snap);
    }

    #[test]
    fn guid_braced_round_trip() {
        let g = GUID::from_u128(0x665c1d5f_c218_414d_a05d_7fef5f9d5c86);
        let braced = guid_to_braced(&g);
        assert!(braced.starts_with('{') && braced.ends_with('}'));
        let parsed = parse_braced_guid(&braced).unwrap();
        assert_eq!(parsed, g);
    }
}
