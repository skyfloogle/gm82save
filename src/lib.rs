#![feature(asm_sym, naked_functions)]

#[cfg(not(all(windows, target_arch = "x86")))]
compile_error!("this tool only works on windows 32-bit");

mod asset;
#[macro_use]
mod delphi;
mod compiler;
mod events;
mod ide;
mod list;
mod load;
mod project_watcher;
mod save;
mod save_exe;
mod stub;

use crate::delphi::{TMenuItem, TTreeNode, UStr};
use ide::AssetListTrait;
use lazy_static::lazy_static;
use regex::Regex;
use std::{
    arch::asm,
    collections::{HashMap, HashSet},
    ffi::{c_void, OsStr},
    io::Write,
    os::windows::process::CommandExt,
    path::PathBuf,
    ptr,
    time::SystemTime,
};

#[derive(Debug)]
pub enum Error {
    IoError(std::io::Error),
    FileIoError(std::io::Error, PathBuf),
    DirIoError(std::io::Error, PathBuf),
    PngDecodeError(PathBuf, png::DecodingError),
    UnicodeError(String),
    AssetNotFound(String, &'static str, String),
    SyntaxError(PathBuf),
    UnknownKey(PathBuf, String),
    UnknownAction(u32, u32),
    ParseIntError(std::num::ParseIntError),
    ParseFloatError(std::num::ParseFloatError),
    InvalidVersion(String),
    DuplicateAsset(String),
    DuplicateIncludedFile(String),
    DuplicateTrigger(String),
    BadAssetName(String, char),
    BadIncludedFileName(String, char),
    BadTriggerName(String, char),
    OldGM82,
    Other(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::IoError(e) => write!(f, "io error: {}", e),
            Self::FileIoError(e, p) => write!(f, "io error in file {}: {}", p.to_string_lossy(), e),
            Self::DirIoError(e, p) => write!(f, "io error in directory {}: {}", p.to_string_lossy(), e),
            Self::PngDecodeError(p, e) => write!(f, "couldn't decode image {}: {}", p.to_string_lossy(), e),
            Self::UnicodeError(s) => write!(f, "couldn't encode {}", s),
            Self::AssetNotFound(s, t, src) => write!(f, "couldn't find {} {} (from {})", t, s, src),
            Self::SyntaxError(p) => write!(f, "syntax error in file {}", p.to_string_lossy()),
            Self::UnknownKey(p, k) => write!(f, "unknown key in {}: {}", p.to_string_lossy(), k),
            Self::UnknownAction(lib_id, act_id) => write!(f, "unknown action {} in lib with id {}", act_id, lib_id),
            Self::ParseIntError(e) => write!(f, "integer parse error: {}", e),
            Self::ParseFloatError(e) => write!(f, "float parse error: {}", e),
            Self::InvalidVersion(v) => write!(f, "invalid exe_version {}", v),
            Self::DuplicateAsset(n) => write!(f, "multiple assets named {}", n),
            Self::DuplicateIncludedFile(n) => write!(f, "multiple included files named {}", n),
            Self::DuplicateTrigger(n) => write!(f, "multiple triggers named {}", n),
            Self::BadAssetName(n, c) => write!(f, "asset name {n} may not contain character {c}"),
            Self::BadIncludedFileName(n, c) => write!(f, "included file name {n} may not contain character {c}"),
            Self::BadTriggerName(n, c) => write!(f, "trigger file name {n} may not contain character {c}"),
            Self::OldGM82 => write!(f, "this project was made with a newer version of gm82save, please update"),
            Self::Other(s) => write!(f, "other error: {}", s),
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(err: std::io::Error) -> Self {
        Error::IoError(err)
    }
}

impl From<std::num::ParseIntError> for Error {
    fn from(err: std::num::ParseIntError) -> Self {
        Error::ParseIntError(err)
    }
}

impl From<std::num::ParseFloatError> for Error {
    fn from(err: std::num::ParseFloatError) -> Self {
        Error::ParseFloatError(err)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

const ACTION_TOKEN: &str = "/*\"/*'/**//* YYD ACTION";

fn show_message(msg: impl AsRef<OsStr>) {
    unsafe {
        delphi::ShowMessage(&UStr::new(msg));
    }
}

#[cfg(not(feature = "smooth_progress_bar"))]
struct FakeSender;
#[cfg(not(feature = "smooth_progress_bar"))]
impl FakeSender {
    fn send(&self, _: ()) {}
}
#[cfg(not(feature = "smooth_progress_bar"))]
fn run_while_updating_bar<OP>(_bar_start: u32, _bar_end: u32, _count: u32, op: OP) -> Result<()>
where
    OP: Fn(FakeSender) -> Result<()> + Sync + Send,
{
    op(FakeSender)
}

#[cfg(feature = "smooth_progress_bar")]
fn run_while_updating_bar<OP>(bar_start: u32, bar_end: u32, count: u32, op: OP) -> Result<()>
where
    OP: Fn(crossbeam_channel::Sender<()>) -> Result<()> + Send + Sync,
{
    if count > 0 {
        let (tx, rx) = crossbeam_channel::unbounded();
        let handle = {
            // this is basically what crossbeam does
            // note that the handle is joined at the end of the function
            // but it should be done executing by the time we get there anyway
            let f: Box<dyn FnOnce() -> Result<()> + Send> = Box::new(|| op(tx));
            let f: Box<dyn FnOnce() -> Result<()> + Send + 'static> = unsafe { std::mem::transmute(f) };
            std::thread::spawn(f)
        };
        let mut progress = 0;
        'outer: loop {
            'inner: loop {
                match rx.try_recv() {
                    Ok(()) => progress += 1,
                    Err(crossbeam_channel::TryRecvError::Empty) => break 'inner,
                    Err(_) => break 'outer,
                }
            }
            delphi::advance_progress_form(progress * (bar_end - bar_start) / count + bar_start);
            // if this errors, it'll error next time too so no need to check
            if let Ok(()) = rx.recv_timeout(std::time::Duration::from_millis(20)) {
                progress += 1;
            }
        }
        handle.join().unwrap()
    } else {
        Ok(())
    }
}

static mut SAVE_END: SystemTime = SystemTime::UNIX_EPOCH;
static mut LAST_SAVE: f64 = 0.0;

fn update_timestamp() {
    unsafe {
        SAVE_END = SystemTime::now();
        delphi::Now(&mut LAST_SAVE);
    }
}

#[naked]
unsafe extern "C" fn reset_if_time_went_backwards() {
    asm! {
        "movsd xmm0, qword ptr [{last_save}]", // load last save
        "ucomisd xmm0, qword ptr [esp]",       // compare to now
        "jb 2f", // jump if now > last save (i.e. no change needed)
        "mov dword ptr [{last_save}], 0",      // null out last_save
        "mov dword ptr [{last_save}+4], 0",
        "2: add esp, 0x20", // return
        "ret",
        last_save = sym LAST_SAVE,
        options(noreturn),
    }
}

#[derive(Clone)]
struct InstanceExtra {
    pub name: u32,
    pub xscale: f64,
    pub yscale: f64,
    pub blend: u32,
    pub angle: f64,
}

impl InstanceExtra {
    pub const DEFAULT: Self = Self { name: 0, xscale: 1.0, yscale: 1.0, blend: u32::MAX, angle: 0.0 };
}

impl Default for InstanceExtra {
    fn default() -> Self {
        Self::DEFAULT
    }
}

#[derive(Clone)]
struct TileExtra {
    pub xscale: f64,
    pub yscale: f64,
    pub blend: u32,
}

impl TileExtra {
    pub const DEFAULT: Self = Self { xscale: 1.0, yscale: 1.0, blend: u32::MAX };
}

impl Default for TileExtra {
    fn default() -> Self {
        Self::DEFAULT
    }
}

unsafe extern "fastcall" fn stuff_to_do_on_project_init() {
    EXTRA_DATA = None;
    project_watcher::unwatch();
    let _: u32 = delphi_call!(0x62c554); // reset objects (what this overwrote)
    // insert a blank object
    ide::OBJECTS.alloc(1);
}

static mut EXTRA_DATA: Option<(HashMap<usize, InstanceExtra>, HashMap<usize, TileExtra>)> = None;

unsafe extern "fastcall" fn about_inj(about_dialog: *const *const usize) {
    let info = UStr::new(concat!("gm82save: ", env!("BUILD_DATE")));
    let edition_label = *about_dialog.add(0xe5);
    asm! {
        "call {}",
        in(reg) 0x4ee6d8, // TControl.SetText
        inlateout("eax") edition_label => _,
        inlateout("edx") info.0 => _,
        lateout("ecx") _,
    }
}

#[naked]
unsafe extern "C" fn save_inj() {
    asm! {
        "mov ecx, ebp",
        "sub ecx, 4",
        "mov edx, ebp",
        "sub edx, 20",
        "jmp {}",
        sym save,
        options(noreturn),
    }
}

// set the high byte to nonzero if YYD save code was used
// set the low byte to nonzero on success
unsafe extern "fastcall" fn save(proj_path: &UStr, stream_ptr: *mut u32) -> u16 {
    SEEN_ERROR = false;
    const IS_YYD: u16 = 0x100;
    let mut path: PathBuf = proj_path.to_os_string().into();
    // filename ".gm82" works in the ui but rust doesn't get it so check for that specifically
    let is_gm82 = path.extension() == Some("gm82".as_ref()) || path.file_name() == Some(".gm82".as_ref());
    if !is_gm82 {
        project_watcher::unwatch();
        // CStream.Create
        let buf = delphi_call!(0x405a4c, 0x52e8fc, 1);
        stream_ptr.write(buf);
        // save gmk
        let success: u32 = delphi_call!(0x705798, buf);
        return success as u16
    }

    if let Err(e) = save::save_gmk(&mut path) {
        // display the error
        project_watcher::unwatch();
        delphi::close_progress_form();
        show_message(format!("Failed to save: {}", e));
        0 | IS_YYD
    } else {
        if !SAVING_FOR_ROOM_EDITOR {
            project_watcher::setup_watcher(&mut path);
        }
        delphi::close_progress_form();
        1 | IS_YYD
    }
}

#[naked]
unsafe extern "C" fn load_inj() {
    asm! {
        "mov ecx, ebp",
        "sub ecx, 4",
        "mov edx, ebp",
        "sub edx, 12",
        "mov eax,ebp",
        "sub eax, 5",
        "push eax",
        "call {}",
        "ret",
        sym load,
        options(noreturn),
    };
}

unsafe extern "fastcall" fn load(proj_path: &UStr, stream_ptr: *mut u32, result_ptr: *mut bool) -> bool {
    SEEN_ERROR = false;
    SAW_APPLIES_TO_WARNING = false;
    project_watcher::unwatch();
    let path: PathBuf = proj_path.to_os_string().into();
    // .gm82 works in the ui but rust doesn't get it so check for that specifically
    let is_gm82 = path.extension() == Some("gm82".as_ref()) || path.file_name() == Some(".gm82".as_ref());
    if !is_gm82 {
        let stream = delphi_call!(0x405a4c, 0x52e8fc, 1);
        stream_ptr.write(stream);
        return false
    }

    if let Err(e) = load::load_gmk(path) {
        // display the error and reload
        delphi::close_progress_form();
        show_message(format!("Failed to load: {}", e));
        ide::initialize_project();
    } else {
        delphi::close_progress_form();
        result_ptr.write(true);
    }
    true
}

#[naked]
unsafe extern "C" fn gm81_or_gm82_inj() {
    asm! {
        "mov ecx, eax",
        "jmp {}",
        sym gm81_or_gm82,
        options(noreturn),
    }
}

unsafe extern "fastcall" fn gm81_or_gm82(s: *const u16) -> i32 {
    let s = UStr::from_ptr(&s);
    // test .gm81
    let out = delphi::CompareText(s, 0x6e0534 as _);
    // test .gm82
    if out != 0 { delphi::CompareText(s, 0x6dfbe4 as _) } else { out }
}

unsafe extern "fastcall" fn make_new_folder(_: u32, path_ptr: *const u16) {
    use load::UStrPtr;
    let path_delphi = UStr::from_ptr(&path_ptr);
    let mut path: PathBuf = path_delphi.to_os_string().into();
    // .gm82 works in the ui but rust doesn't get it so check for that specifically
    let is_gm82 = path.extension() == Some("gm82".as_ref()) || path.file_name() == Some(".gm82".as_ref());
    if is_gm82 && !path.is_file() {
        path.push(path.file_name().unwrap().to_owned());
    }
    ide::PROJECT_PATH.asg(path);
    // throw this in here as well
    project_watcher::unwatch();
}

#[naked]
unsafe extern "C" fn fix_tile_null_pointer() {
    asm! {
        "mov edx, 0x64e048",
        "call edx",
        "mov edx, 0x68ef07",
        "mov ecx, 0x68ef6c",
        "test eax, eax",
        "cmovz edx, ecx",
        "jmp edx",
        options(noreturn),
    }
}

#[naked]
unsafe extern "C" fn inflate_inj() {
    asm! {
        "mov ecx, eax",
        "jmp {}",
        sym inflate,
        options(noreturn),
    }
}

unsafe extern "fastcall" fn inflate(src: &delphi::TMemoryStream) -> &mut delphi::TMemoryStream {
    let dst = &mut *delphi::TMemoryStream::new();
    let mut size: usize = 0;
    src.read(&mut size as *mut usize as *mut u8, 4);
    let mut data = Vec::with_capacity(size);
    data.set_len(size);
    src.read(data.as_mut_ptr(), size as u32);
    let mut decoder = flate2::write::ZlibDecoder::new(dst);
    decoder.write_all(&data).unwrap();
    let dst = decoder.finish().unwrap();
    dst.set_pos(0);
    return dst
}

#[naked]
unsafe extern "C" fn deflate_inj() {
    asm! {
        "mov ecx, eax",
        "jmp {}",
        sym deflate,
        options(noreturn),
    }
}

unsafe extern "fastcall" fn deflate(dst: &mut delphi::TMemoryStream, src: &delphi::TMemoryStream) {
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::new(DEFLATE_LEVEL));
    encoder.write_all(src.get_slice()).unwrap();
    let data = encoder.finish().unwrap();
    let _ = dst.write(&data.len().to_le_bytes());
    let _ = dst.write(&data);
    src.set_pos(data.len() as _);
}

static mut DEFLATE_LEVEL: u32 = 6; // default

#[naked]
unsafe extern "C" fn build_small() {
    asm! {
        "mov ecx, 9",
        "mov {}, ecx",
        "mov ecx, 0x4cf2f4",
        "jmp ecx",
        sym DEFLATE_LEVEL,
        options(noreturn),
    }
}

#[naked]
unsafe extern "C" fn build_fast() {
    asm! {
        "mov ecx, 1",
        "mov {}, ecx",
        "mov ecx, 0x41735c",
        "jmp ecx",
        sym DEFLATE_LEVEL,
        options(noreturn),
    }
}

#[naked]
unsafe extern "C" fn reset_compression() {
    asm! {
        "mov ecx, 6",
        "mov {}, ecx",
        "mov ecx, 0x51cc64",
        "jmp ecx",
        sym DEFLATE_LEVEL,
        options(noreturn),
    }
}

unsafe extern "stdcall" fn duplicate_room(room: &mut asset::Room, old_id: usize, new_id: usize) {
    let room_names = ide::ROOMS.names();
    fix_instances_when_renaming_room(
        room,
        room_names[old_id].to_os_string().to_str().unwrap(),
        room_names[new_id].to_os_string().to_str().unwrap(),
    );
    freshen_room_ids(room);
}

unsafe fn freshen_room_ids(room: &mut asset::Room) {
    if let Some((instance_map, tile_map)) = EXTRA_DATA.as_mut() {
        let last_instance_id = *ide::LAST_INSTANCE_ID + 1;
        let instances = room.get_instances_mut();
        *ide::LAST_INSTANCE_ID += instances.len();
        for (i, instance) in instances.into_iter().enumerate() {
            let old_id = instance.id;
            instance.id = last_instance_id + i;
            if let Some(data) = instance_map.get(&old_id).cloned() {
                instance_map.insert(instance.id, data);
            }
        }
        let last_tile_id = *ide::LAST_TILE_ID + 1;
        let tiles = room.get_tiles_mut();
        *ide::LAST_TILE_ID += tiles.len();
        for (i, tile) in tiles.into_iter().enumerate() {
            let old_id = tile.id;
            tile.id = last_tile_id + i;
            if let Some(data) = tile_map.get(&old_id).cloned() {
                tile_map.insert(tile.id, data);
            }
        }
    }
}

#[naked]
unsafe extern "C" fn setup_unicode_parse_inj() {
    asm! {
        "mov ecx, edi",
        "call {}",
        "mov eax, 5",
        "ret",
        sym setup_unicode_parse,
        options(noreturn),
    }
}

#[naked]
unsafe extern "C" fn teardown_unicode_parse_inj() {
    asm! {
        "mov ecx, 810",
        "call {}",
        "mov eax, 0x6ca2cc",
        "jmp eax",
        sym setup_unicode_parse,
        options(noreturn),
    }
}

unsafe extern "fastcall" fn setup_unicode_parse(version: i32) {
    // this just patches CStream.ReadString to read with the active code page instead of UTF-8
    // (and reverts that change after loading so nothing else breaks)
    let cp = if version < 810 { [0, 0] } else { [0xe9, 0xfd] };
    patch(0x52f0a2 as _, &cp);
    patch(0x52f0c5 as _, &cp);
}

#[naked]
unsafe extern "C" fn properly_update_object_timestamp_drag_drop() {
    asm! {
        "mov eax, [esi + 0x46c]",
        "mov ecx, 0x62cd2c",
        "jmp ecx",
        options(noreturn),
    }
}

#[naked]
unsafe extern "C" fn properly_update_timeline_timestamp_drag_drop() {
    asm! {
        "mov eax, [esi + 0x430]",
        "mov ecx, 0x6fa7b0",
        "jmp ecx",
        options(noreturn),
    }
}

#[naked]
unsafe extern "C" fn properly_update_object_timestamp_right_click() {
    asm! {
        // show action modal
        "mov ecx, 0x6ff4dc",
        "call ecx",
        // if it returned false, return false
        "test al, al",
        "jz 2f",
        // update timestamp and return 1
        "mov eax, [esi + 0x46c]",
        "mov ecx, 0x62cd2c",
        "call ecx",
        "mov al, 1",
        "2: ret",
        options(noreturn),
    }
}

#[naked]
unsafe extern "C" fn properly_update_timeline_timestamp_right_click() {
    asm! {
        // show action modal
        "mov ecx, 0x6ff4dc",
        "call ecx",
        // if it returned false, return false
        "test al, al",
        "jz 2f",
        // update timestamp and return 1
        "mov eax, [esi + 0x430]",
        "mov ecx, 0x6fa7b0",
        "call ecx",
        "mov al, 1",
        "2: ret",
        options(noreturn),
    }
}

#[naked]
unsafe extern "C" fn update_sprite_mask_timestamp() {
    asm! {
        "mov eax, [ebx+0x42c]",
        "mov ecx, 0x6f5ac8",
        "jmp ecx",
        options(noreturn),
    }
}

#[naked]
unsafe extern "C" fn gm82_file_association_inj() {
    asm! {
        "mov ecx, eax",
        "jmp {}",
        sym gm82_file_association,
        options(noreturn),
    }
}

unsafe extern "fastcall" fn gm82_file_association(reg: u32) {
    let ext = UStr::new(r"\.gm82");
    let _: u32 = delphi_call!(0x6dd850, reg, ext.0, 0, UStr::new("gm82file").0);
    let _: u32 = delphi_call!(0x452568, reg, 0x80000001u32);
    let _: u32 = delphi_call!(0x6dd850, reg, ext.0, UStr::new(r"\Software\Classes").0, UStr::new("gm82file").0);
}

unsafe extern "fastcall" fn check_gm_processes(name: usize, value: u32) {
    use sysinfo::{ProcessExt, SystemExt};
    let system = sysinfo::System::new_with_specifics(
        sysinfo::RefreshKind::new().with_processes(sysinfo::ProcessRefreshKind::new()),
    );
    let path = std::env::current_exe().unwrap();
    if system.processes().iter().filter(|(_, p)| p.exe() == path).count() <= 1 {
        let _: u32 = delphi_call!(0x716b78, name, value);
    }
}

#[naked]
unsafe extern "C" fn free_image_editor_bitmap() {
    asm! {
        // call free on TheBitmap
        "mov edx, 0x405a7c",
        "call edx",
        // get TransBitmap
        "mov eax, dword ptr [esi]",
        "mov eax, dword ptr [eax + 0x708]",
        // check if it exists
        "test eax, eax",
        "jz 1f",
        // free it
        "mov edx, 0x405a7c",
        "call edx",
        "1: ret",
        options(noreturn),
    }
}

#[naked]
unsafe extern "C" fn copy_origin_on_new() {
    asm! {
        "mov ecx, [eax+0xc]",
        "mov [esi+0xc], ecx",
        "mov ecx, [eax+0x10]",
        "mov [esi+0x10], ecx",
        "mov ecx, 0x405a7c",
        "jmp ecx",
        options(noreturn),
    }
}

#[naked]
unsafe extern "C" fn floor_st0() {
    asm! {
        // move the return address and put st0 on the stack before it so it's like an argument
        "mov eax, [esp]",
        "sub esp, 8",
        // can't fucking say "fstp tword ptr [esp]" apparently so i guess i'm doing this
        ".byte 0xdb, 0x3c, 0x24",
        "push eax",
        "mov eax, 0x410538",
        "jmp eax",
        options(noreturn),
    }
}

#[naked]
unsafe extern "C" fn dont_show_action_tooltip_if_event_is_null() {
    asm! {
        // if eax is not null, call CEvent.GetAction
        "mov ecx, 0x5a502c",
        "test eax, eax",
        "jz 2f",
        "jmp ecx",
        // otherwise, skip to the end of that code block
        // there's a function that updates the tooltip or something in there
        // so skip it, so that the tooltip doesn't just stick around
        "2: add dword ptr [esp], 0x25",
        "ret",
        options(noreturn),
    }
}

#[naked]
unsafe extern "C" fn path_form_mouse_wheel_inj() {
    asm! {
        // call TPathForm.Create
        "mov ebx, 0x514e78",
        "call ebx",
        // set OnMouseWheel
        "mov dword ptr [eax + 0x144], eax",
        "lea edx, {}",
        "mov dword ptr [eax + 0x140], edx",
        "ret",
        sym path_form_mouse_wheel,
        options(noreturn),
    }
}

#[naked]
unsafe extern "C" fn path_form_mouse_wheel() {
    asm! {
        // check handled flag
        "mov edx, [esp + 0x4]",
        "cmp byte ptr [edx], 0",
        "jnz 4f",
        // set handled flag
        "mov byte ptr [edx], 1",
        "push eax",
        // check if shift is being held
        "mov edx, dword ptr [esp + 0x10]",
        "test cx, 1",
        "jnz 2f",
        // no shift, so scroll vertically
        "sub dword ptr [eax + 0x464], edx",
        "jmp 3f",
        // yes shift, so scroll horizontally
        "2: sub dword ptr [eax + 0x460], edx",
        "3:",
        // update room background
        "mov ecx, 0x7203ec",
        "call ecx",
        // draw path image
        "mov eax, dword ptr [esp]",
        "mov ecx, 0x720560",
        "call ecx",
        // update status bar
        "pop eax",
        "mov ecx, 0x720a68",
        "call ecx",
        "4:",
        "ret 0xc",
        options(noreturn),
    }
}

#[naked]
unsafe extern "C" fn path_room_change_forces_room_editor_save() {
    asm! {
        "mov ecx, 0x720560", // draw form
        "call ecx",
        "mov byte ptr {}, 1",
        "ret",
        sym PATH_FORM_UPDATED,
        options(noreturn),
    }
}

static mut PATH_FORM_UPDATED: bool = false;

#[naked]
unsafe extern "C" fn code_editor_middle_click() {
    asm! {
        // push return address
        "mov ecx, 0x6b734e",
        "push ecx",
        // abort if not middle click
        "cmp byte ptr [ebp - 1], 2",
        "jnz 2f",
        // save cursor position
        "push dword ptr [ebx + 0x2ec]",
        "push dword ptr [ebx + 0x2f0]",
        // move cursor
        "mov dword ptr [ebx + 0x2ec], edi",
        "mov dword ptr [ebx + 0x2f0], esi",
        // show resource on cursor position
        "mov ecx, 0x6b2000",
        "mov eax, ebx",
        "call ecx",
        // reset cursor
        "pop dword ptr [ebx + 0x2f0]",
        "pop dword ptr [ebx + 0x2ec]",
        "2: ret",
        options(noreturn),
    }
}

unsafe extern "fastcall" fn code_editor_script_hint(name: *const u16, out: &mut UStr) {
    // get script from name
    let script_id: i32 = delphi_call!(0x655c2c, name);
    if script_id >= 0 {
        if let Some(Some(script)) = ide::SCRIPTS.assets().get(script_id as usize) {
            if script.source.as_slice().get(..3) == Some(&[b'/' as u16; 3]) {
                let count =
                    script.source.as_slice().iter().position(|&c| c == b'\r' as u16).unwrap_or(script.source.len()) - 3;
                let mut untrimmed = UStr::default();
                // @UStrCopy
                let _: u32 = delphi_call!(0x4086a8, script.source.0, 4, count, &mut untrimmed.0);
                // Trim
                let _: u32 = delphi_call!(0x415dd0, untrimmed.0, &mut out.0);
            }
        }
    }
}

#[naked]
unsafe extern "C" fn completion_script_args_inj() {
    asm! {
        "mov ecx, eax",
        "call {}",
        // copy output of this to the argument on the stack
        "mov eax, dword ptr [ebp-0x5c]",
        "mov dword ptr [esp+8], eax",
        "ret",
        sym completion_script_args,
        options(noreturn),
    }
}

unsafe extern "fastcall" fn completion_script_args(script_id: usize, out: &mut UStr) {
    let script = ide::SCRIPTS.assets().get_unchecked(script_id).unwrap_unchecked();
    if script.source.as_slice().get(..3) == Some(&[b'/' as u16; 3]) {
        if let Some(paren_pos) = script.source.as_slice().iter().position(|&c| c == b'(' as u16) {
            let count = script.source.as_slice().iter().position(|&c| c == b'\r' as u16).unwrap_or(script.source.len())
                - paren_pos;
            let mut untrimmed = UStr::default();
            // @UStrCopy
            let _: u32 = delphi_call!(0x4086a8, script.source.0, paren_pos + 1, count, &mut untrimmed.0);
            // Trim
            let _: u32 = delphi_call!(0x415dd0, untrimmed.0, &mut out.0);
            return
        }
    }
    *out = UStr(0x6baf10 as _);
}

#[naked]
unsafe extern "C" fn write_number_on_actions() {
    asm! {
        // call original function
        "mov ecx, 0x45b498",
        "call ecx",
        // move existing string to top of stack
        "mov eax, [ebp-4]",
        "push eax",
        "mov dword ptr [ebp-4], 0",
        // get action index from outer function (same for object and form)
        "mov eax, [ebp]",
        "mov eax, [eax-4]",
        "inc eax",
        // convert to int and put in original string
        "lea edx, [ebp-4]",
        "mov ecx, 0x41666c",
        "call ecx",
        // append ". "
        "lea eax, [ebp-4]",
        "lea edx, 2f",
        "mov ecx, 0x4082dc",
        "call ecx",
        // append original string
        "lea eax, [ebp-4]",
        "mov edx, [esp]",
        "mov ecx, 0x4082dc",
        "call ecx",
        // free original string
        "lea eax, [esp]",
        "mov ecx, 0x407ea8",
        "call ecx",
        // cleanup and return
        "add esp, 4",
        "ret",
        // define the ". "
        ".align 4",
        ".short 1200, 2",
        ".long -1, 2",
        "2:",
        ".short '.', ' ', 0",
        options(noreturn),
    }
}

unsafe extern "C" fn regen_temp_folder_when_making_file() {
    asm! {
        "mov ecx, 0x407660",
        "call ecx",
        // ForceDirectories the temp directory
        "mov eax, [0x788974]",
        "mov ecx, 0x416eac",
        "jmp ecx",
        options(noreturn),
    }
}

unsafe extern "C" fn get_temp_folder_but_also_regen_it() {
    asm! {
        //UStrAsg temp_diretory to the output
        "mov edx, [0x788974]",
        "mov ecx, 0x407eb8",
        "call ecx",
        // ForceDirectories the temp directory
        "mov eax, [0x788974]",
        "mov ecx, 0x416eac",
        "jmp ecx",
    }
}

static mut SEEN_ERROR: bool = false;

unsafe extern "fastcall" fn patch_error_box(caption: *const u16, text: *const u16, _flags: u32) {
    if SEEN_ERROR {
        return
    }
    SEEN_ERROR = true;
    let mut message = UStr::from_ptr(&text).clone();
    let extra_text = UStr::new(
        "\r\nGame Maker will continue to run, but may be unstable. Enjoy!\r\n\r\n\
        In the meantime, please send Floogle your TraceIDE.log file. It should be next to \
        GameMaker.exe. Would you like to open the relevant folder now?",
    );
    // UStrCat
    let _: u32 = delphi_call!(0x4082dc, &mut message, extra_text.0);
    // TApplication.MessageBox
    let answer: u32 = delphi_call!(0x51fbdc, *(0x7882ec as *const u32), message.0, caption, 0x14);
    if answer == 6 {
        let path = std::env::current_exe()
            .ok()
            .and_then(|mut p| {
                p.pop();
                p.push("TraceIDE.log");
                p.into_os_string().into_string().ok()
            })
            .unwrap_or_default();
        let _ = std::process::Command::new("explorer.exe").raw_arg(format!("/select,\"{path}\"")).spawn();
    }
}

#[naked]
unsafe extern "C" fn get_treenode_count_and_preserve_resource_type() {
    asm! {
        "mov ecx, 0x4ad490",
        "call ecx",
        "push edi",
        "mov ecx, 0x71c6a5",
        "jmp ecx",
        options(noreturn),
    }
}

#[naked]
unsafe extern "C" fn add_three_newest_inj() {
    asm! {
        // add a line
        "mov eax, [esi + 0x38]",
        "mov ecx, 0x4dd244",
        "call ecx",
        // add the three items
        "pop edx", // resource type
        "push ebp",
        "mov ecx, [esi + 0x38]", // menu items
        "call {}",
        // cleanup
        "xor eax, eax",
        "pop edx",
        "pop ecx",
        "pop ecx",
        "mov ecx, 0x71c6e2",
        "jmp ecx",
        sym add_three_newest,
        options(noreturn),
    }
}

unsafe extern "fastcall" fn add_three_newest(items: &TMenuItem, ty: u32, ebp: *const u8) {
    unsafe fn inner<T: 'static, AL: AssetListTrait<T>>(items: &TMenuItem, ty: u32, ebp: *const u8, asset_list: &AL) {
        let ids = asset_list
            .assets()
            .iter()
            .enumerate()
            .rev()
            .filter(|(_, o)| o.is_some())
            .map(|(i, _)| i)
            .take(3)
            .collect::<Vec<_>>();
        for &id in ids.iter().rev() {
            items.add_with_fake_tree_node(
                &asset_list.names()[id],
                3,
                ty,
                id,
                ebp.sub(5).cast::<bool>().read().then(|| &*ebp.cast::<[u32; 6]>()),
            );
        }
    }
    match ty {
        1 => inner(items, ty, ebp, &ide::OBJECTS),
        2 => inner(items, ty, ebp, &ide::SPRITES),
        3 => inner(items, ty, ebp, &ide::SOUNDS),
        4 => inner(items, ty, ebp, &ide::ROOMS),
        6 => inner(items, ty, ebp, &ide::BACKGROUNDS),
        7 => inner(items, ty, ebp, &ide::SCRIPTS),
        8 => inner(items, ty, ebp, &ide::PATHS),
        9 => inner(items, ty, ebp, &ide::FONTS),
        12 => inner(items, ty, ebp, &ide::TIMELINES),
        _ => return,
    };
}

#[naked]
unsafe extern "C" fn room_form_inj() {
    asm! {
        "mov ecx, eax",
        "jmp {}",
        sym room_form,
        options(noreturn),
    }
}

#[naked]
unsafe extern "C" fn room_size() {
    asm! {
        "mov eax, {width}",
        "mov dword ptr [ebx+0xc], eax",
        "mov eax, {height}",
        "mov dword ptr [ebx+0x10], eax",
        "mov eax, {speed}",
        "mov dword ptr [ebx+0x8], eax",
        // views
        "mov ecx, 7 * 0x38",
        "2:", // loop point
        "mov eax, dword ptr {width}", // width
        "mov dword ptr [ebx + 0x12c + ecx + 0xc], eax",  // view_w
        "mov dword ptr [ebx + 0x12c + ecx + 0x1c], eax", // port_w
        "mov eax, dword ptr {height}", // height
        "mov dword ptr [ebx + 0x12c + ecx + 0x10], eax", // view_h
        "mov dword ptr [ebx + 0x12c + ecx + 0x20], eax", // port_h
        "sub ecx, 0x38",
        "jge 2b",
        "ret",
        width = sym DEFAULT_ROOM_WIDTH,
        height = sym DEFAULT_ROOM_HEIGHT,
        speed = sym DEFAULT_ROOM_SPEED,
        options(noreturn),
    }
}

#[naked]
unsafe extern "C" fn fix_broken_room_size() {
    asm! {
        // we already have speed in eax so do that one first
        "cmp eax, 0",
        "cmovz eax, {speed}",
        "mov {speed}, eax",
        // check width
        "mov eax, 800",
        "cmp dword ptr {width}, 0",
        "cmovnz eax, {width}",
        "mov {width}, eax",
        // check height
        "mov eax, 608",
        "cmp dword ptr {height}, 0",
        "cmovnz eax, {height}",
        "mov {height}, eax",
        "ret",
        width = sym DEFAULT_ROOM_WIDTH,
        height = sym DEFAULT_ROOM_HEIGHT,
        speed = sym DEFAULT_ROOM_SPEED,
        options(noreturn),
    }
}

static mut DEFAULT_ROOM_WIDTH: u32 = 800;
static mut DEFAULT_ROOM_HEIGHT: u32 = 608;
static mut DEFAULT_ROOM_SPEED: u32 = 50;

#[naked]
unsafe extern "fastcall" fn rename_room_inj() {
    asm! {
        "mov ecx, ebx",
        "mov edx, [ebp - 4]",
        "call {}",
        "test eax, eax", // for the jump afterwards
        "ret",
        sym rename_room,
        options(noreturn),
    }
}

unsafe extern "fastcall" fn rename_room(room_id: usize, new_name: *const u16) -> *const UStr {
    let room_names = ide::ROOMS.names();
    if let Some(room) = ide::ROOMS.assets()[room_id] {
        let new_name = UStr::from_ptr(&new_name);
        let new_name_slice = new_name.as_slice();
        if new_name_slice.is_empty() {
            show_message("Can't give room an empty name.");
            return ptr::null()
        }
        if new_name_slice.contains(&(b'=' as u16)) {
            show_message("Can't use illegal character '=' in asset name.");
            return ptr::null()
        }
        let old_name = room_names[room_id].to_os_string().into_string().unwrap();
        let new_name = new_name.to_os_string().into_string().unwrap();
        fix_instances_when_renaming_room(&mut *(room as *const asset::Room as *mut asset::Room), &old_name, &new_name);
    }
    &room_names[room_id]
}

lazy_static! {
    static ref ROOM_RENAME_REGEX: Regex = Regex::new(r"=[ \t\r\n]*([^=]*?)_[0-9A-F]{8}").unwrap();
}

fn fix_instances_when_renaming_room(room: &mut asset::Room, old_name: &str, new_name: &str) {
    let re: &Regex = &ROOM_RENAME_REGEX;
    for inst in room.get_instances_mut() {
        let code = inst.creation_code.to_os_string().into_string().unwrap();
        let mut it = re.captures_iter(&code).filter_map(|c| c.get(1)).filter(|m| m.as_str() == old_name).peekable();
        if it.peek().is_none() {
            continue
        }
        let mut new_code = String::with_capacity(code.len());
        let mut last_match = 0;
        for m in it {
            new_code.push_str(&code[last_match..m.start()]);
            new_code.push_str(&new_name);
            last_match = m.end();
        }
        new_code.push_str(&code[last_match..]);
        inst.creation_code = UStr::new(new_code);
    }
}

#[naked]
unsafe extern "fastcall" fn dont_make_room_form_inj() {
    asm! {
        "mov ecx, eax",
        "jmp {}",
        sym dont_make_room_form,
        options(noreturn)
    }
}

unsafe extern "fastcall" fn dont_make_room_form(node: &TTreeNode) {
    // always open if gm82room is disabled, not opening a room, or not using gm82 format
    if *(0x79a982 as *const bool)
        || (*node.data).kind != 4
        || (*ide::PROJECT_PATH).as_slice().last().copied() != Some(b'2' as u16)
    {
        let _: u32 = delphi_call!(0x71d608, node);
    }
}

#[naked]
unsafe extern "fastcall" fn show_instance_id_inj() {
    asm! {
        "mov ecx, eax",
        "mov eax, [ebx + 0x630]",
        "push eax",
        "call {}",
        "ret",
        sym show_instance_id,
        options(noreturn),
    }
}

unsafe extern "fastcall" fn show_instance_id(id: usize, out: &mut UStr, room_id: usize) {
    if let Some((insts, _)) = EXTRA_DATA.as_mut() {
        let suffix = {
            let mut name = insts.entry(id).or_default().name;
            if name == 0 {
                loop {
                    name = delphi::Random();
                    if !insts.values().any(|ex| ex.name == name) {
                        insts.get_mut(&id).unwrap().name = name;
                        break
                    }
                }
            }
            UStr::new(format!("_{:08X}", name))
        };
        let _: u32 = delphi_call!(0x40839c, out, ide::ROOMS.names()[room_id].0, suffix.0);
    } else {
        let _: u32 = delphi_call!(0x41666c, id, out);
    }
}

static mut SAW_APPLIES_TO_WARNING: bool = false;

static mut SAVING_FOR_ROOM_EDITOR: bool = false;

unsafe extern "fastcall" fn room_form(room_id: usize) -> u32 {
    if *(0x79a982 as *const bool) {
        return delphi_call!(0x6884c8, room_id)
    }
    let editor_path = match std::env::current_exe() {
        Ok(mut path) => {
            path.set_file_name("gm82room.exe");
            path
        },
        Err(_) => return delphi_call!(0x6884c8, room_id),
    };
    if editor_path.exists() {
        let mut room_path = PathBuf::from((&*ide::PROJECT_PATH).to_os_string());
        if room_path.extension() == Some("gm82".as_ref()) {
            // if we haven't loaded as gm82 before, sanitize instance and tile ids, as there may be duplicates
            if EXTRA_DATA.is_none() {
                EXTRA_DATA = Some(Default::default());
                let mut instance_ids = HashSet::new();
                let mut tile_ids = HashSet::new();
                // making rooms mutable would lead to weirdness so i guess i'm not
                for room in ide::ROOMS
                    .assets_mut()
                    .iter_mut()
                    .map(|r| std::mem::transmute::<Option<&asset::Room>, Option<&mut asset::Room>>(*r))
                    .flatten()
                {
                    // check if all ids are unique, and if not, update *all* the ids for consistency
                    let instances_ok = room.get_instances().iter().all(|inst| instance_ids.insert(inst.id));
                    let tiles_ok = room.get_tiles().iter().all(|tile| tile_ids.insert(tile.id));
                    if !instances_ok || !tiles_ok {
                        freshen_room_ids(room);
                    }
                }
                // force-save all open rooms just in case
                for form in ide::ROOMS.forms().iter().map(|&f| f as *mut *const u8) {
                    if !form.is_null() {
                        let room = *form.add(0x61c / 4);
                        let saveroom = *form.add(0x620 / 4);
                        let _: u32 = delphi_call!(0x657994, saveroom, room); // copy room to saveroom
                        let undoroom_ptr = form.add(0x62c / 4);
                        if !(*undoroom_ptr).is_null() {
                            let _: u32 = delphi_call!(0x405a7c, *undoroom_ptr); // free undo room
                            *undoroom_ptr = ptr::null();
                        }
                        form.cast::<bool>().add(0x628).write(false); // clear ischanged flag
                    }
                }
            }
            // save game first, if needed
            let project_modified = PATH_FORM_UPDATED || {
                let out: u32 = delphi_call!(0x7060e8);
                out != 0
            };
            if project_modified {
                SAVING_FOR_ROOM_EDITOR = true;
                let _: u32 = delphi_call!(0x51cc64, *(0x7882f0 as *const u32), 0xfff5); // set cursor
                let success: u32 = delphi_call!(0x705c84, (*ide::PROJECT_PATH).0); // save
                let _: u32 = delphi_call!(0x51cc64, *(0x7882f0 as *const u32), 0); // reset cursor
                SAVING_FOR_ROOM_EDITOR = false;
                if success == 0 {
                    return 0
                }
            } else {
                project_watcher::unwatch();
            }
            // sort out running gm82room
            room_path.pop();
            let mut asset_maps_path = room_path.clone();
            room_path.push("rooms");
            room_path.push(ide::ROOMS.names()[room_id].to_os_string());
            let _: u32 = delphi_call!(0x51acd0, *(0x790100 as *const u32)); // hide main form
            let _ = std::process::Command::new(editor_path).arg(&room_path).spawn().and_then(|mut c| c.wait());
            let _: u32 = delphi_call!(0x51acd8, *(0x790100 as *const u32)); // show main form
            {
                let room = ide::ROOMS.assets()[room_id].unwrap();
                // remove this room's ids from the global thing
                if let Some((extra_inst, extra_tile)) = EXTRA_DATA.as_mut() {
                    for inst in room.get_instances() {
                        extra_inst.remove(&inst.id);
                    }
                    for tile in room.get_tiles() {
                        extra_tile.remove(&tile.id);
                    }
                }
                let _: u32 = delphi_call!(0x405a7c, room); // delete room
            }
            // reload room
            ide::ROOMS.assets_mut()[room_id] = load::load_asset_maps(&mut asset_maps_path)
                .and_then(|asset_maps| load::load_room(&mut room_path, &asset_maps))
                .map_err(|e| e.to_string())
                .expect("loading the updated room failed")
                .as_ref();
            room_path.pop();
            room_path.pop();
            update_timestamp();
            project_watcher::setup_watcher(&mut room_path);
            return 0
        }
    }
    delphi_call!(0x6884c8, room_id) // the default
}

unsafe fn patch(dest: *mut u8, source: &[u8]) {
    // the only winapi imports in the whole project, no need for crates
    #[allow(non_camel_case_types)]
    type PAGE_TYPE = u32;
    const PAGE_READWRITE: PAGE_TYPE = 0x04;
    type BOOL = i32;
    type HANDLE = isize;
    #[link(name = "kernel32")]
    extern "system" {
        fn VirtualProtect(
            lpaddress: *mut c_void,
            dwsize: usize,
            flnewprotect: PAGE_TYPE,
            lpfloldprotect: *mut PAGE_TYPE,
        ) -> BOOL;
        fn GetCurrentProcess() -> HANDLE;
        fn FlushInstructionCache<'a>(hprocess: HANDLE, lpbaseaddress: *const c_void, dwsize: usize) -> BOOL;
    }

    let mut old_protect = 0;
    VirtualProtect(dest.cast(), source.len(), PAGE_READWRITE, &mut old_protect);
    dest.copy_from(source.as_ptr(), source.len());
    VirtualProtect(dest.cast(), source.len(), old_protect, &mut old_protect);
    FlushInstructionCache(GetCurrentProcess(), dest.cast(), source.len());
}

unsafe fn patch_call(instr: *mut u8, proc: usize) {
    patch(instr.add(1), &(proc - (instr as usize + 5)).to_le_bytes());
}

#[cfg_attr(not(test), ctor::ctor)]
#[cfg_attr(test, allow(dead_code))]
unsafe fn injector() {
    std::panic::set_hook(Box::new(|info| {
        #[link(name = "user32")]
        extern "system" {
            fn MessageBoxW(hWnd: usize, lpText: *const u16, lpCaption: *const u16, uType: u32) -> i32;
        }
        let msg = (info.to_string() + "\r\n\r\nPlease send a screenshot of this error message to Floogle.")
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        MessageBoxW(0, msg.as_ptr(), std::ptr::null(), 0x10);

        std::process::exit(-1);
    }));

    // about dialog
    #[rustfmt::skip]
    patch(0x71be58 as _, &[
        0x8b, 0xc8, // mov ecx, eax
        0xe9, // jmp [nothing yet]
    ]);
    patch_call(0x71be5a as _, about_inj as _);

    // call save() instead of CStream.Create and the "save gmk" function
    let save_dest = 0x705cbd as *mut u8;
    #[rustfmt::skip]
    let mut save_patch = [
        0xe8, 0x00, 0x00, 0x00, 0x00, // call save (my save)
        0x84, 0xe4, // test ah,ah
        0x74, 0x0e, // je 0x705cd4 (after this patch)
        0x84, 0xc0, // test al,al
        0x74, 0x25, // je 0x705cef (after save fail)
        0xe9, 0x7e, 0x01, 0x00, 0x00, // jmp 0x705e4d (after save success)
    ];
    save_patch[1..5].copy_from_slice(&(save_inj as u32 - (save_dest as u32 + 5)).to_le_bytes());
    patch(save_dest, &save_patch);

    // call load() instead of CStream.Create
    // and insert a JZ to the post-load code (0x705af3)
    let load_dest = 0x705a42 as *mut u8;
    #[rustfmt::skip]
    let mut load_patch = [
        0xe8, 0x00, 0x00, 0x00, 0x00, // call load (my load)
        0x84, 0xc0, // test al,al
        0x0f, 0x85, 0xa4, 0x00, 0x00, 0x00, // jne 0x705af3 (after load)
    ];
    load_patch[1..5].copy_from_slice(&(load_inj as u32 - (load_dest as u32 + 5)).to_le_bytes());
    patch(load_dest, &load_patch);

    // check for .gm82 as well as .gm81 when dragging file onto game maker
    patch_call(0x6df7e2 as _, gm81_or_gm82_inj as _);
    // check for .gm82 as well as .gm81 in open file dialog
    patch_call(0x6e02ed as _, gm81_or_gm82_inj as _);
    // check for .gm82 as well as .gm81 in "rename if using an old file extension" code
    patch_call(0x6e0574 as _, gm81_or_gm82_inj as _);
    // replace now-unused .gm81 with .gm82
    patch(0x6dfbec as _, &[b'2']);
    // save new .gm82 projects to subfolder when using "save as" dialog
    patch_call(0x6e06b3 as _, make_new_folder as _);

    // fix stupid null pointer error
    patch(0x68ef02 as _, &[0xe9]);
    patch_call(0x68ef02 as _, fix_tile_null_pointer as _);

    // fix memory leak in image editor
    patch_call(0x643bd0 as _, free_image_editor_bitmap as _);

    // copy origin when New
    patch_call(0x6ee2f8 as _, copy_origin_on_new as _);

    // fix grid snap
    patch_call(0x64612b as _, floor_st0 as _);
    patch_call(0x646164 as _, floor_st0 as _);
    patch_call(0x64639e as _, floor_st0 as _);
    patch_call(0x6463d7 as _, floor_st0 as _);

    // fix access violation when closing object/timeline window while mousing over action
    patch_call(0x6c6f6f as _, dont_show_action_tooltip_if_event_is_null as _);
    patch_call(0x6f9043 as _, dont_show_action_tooltip_if_event_is_null as _);

    // add scrolling to path form
    patch_call(0x71fdcb as _, path_form_mouse_wheel_inj as _);

    // changing room in path form counts as a change
    patch_call(0x7211ff as _, path_room_change_forces_room_editor_save as _);

    // use zlib-ng for compression
    patch(0x52f34c as _, &[0xe9]);
    patch_call(0x52f34c as _, deflate_inj as _);
    patch(0x52f2e4 as _, &[0xe9]);
    patch_call(0x52f2e4 as _, inflate_inj as _);
    // build fast when making test build
    patch_call(0x6ce8a2 as _, build_fast as _);
    patch_call(0x6ce8cb as _, reset_compression as _);
    // build small when making release
    patch_call(0x6ce775 as _, build_small as _);
    patch_call(0x6ce78f as _, reset_compression as _);

    // compiler injections
    compiler::inject();
    // reset extra data, unwatch project folder, and add a blank object when loading a new project
    patch_call(0x70598c as _, stuff_to_do_on_project_init as _);

    // read text as ANSI on pre-8.1
    patch(0x70537b as _, &[0xe8]);
    patch_call(0x70537b as _, setup_unicode_parse_inj as _);
    // reset above
    patch_call(0x705acc as _, teardown_unicode_parse_inj as _);

    // .gm82 file associations
    patch_call(0x6ddacd as _, gm82_file_association_inj as _);

    // middle click in code editor shows resource
    // remove first check
    patch(0x6b7182 as _, &[0x90, 0x90, 0x90, 0x90, 0x90, 0x90]);
    // inject second check
    patch_call(0x6b721b as _, code_editor_middle_click as _);

    // code hint: faster extension function search and add script hints
    patch(0x71364e as _, &[0xae, 0x22]);
    #[rustfmt::skip]
    patch(0x6bb12e as _, &[
        // prior line: mov eax, [ebp-4]
        0x8b, 0x55, 0xf8, // mov edx, [ebp-8]
        0xe8, 0xde, 0x84, 0x05, 0x00, // call extension_get_helpline_from_function_name
        0x8b, 0x55, 0xf8, // mov edx, [ebp-8]
        0x8b, 0x02, // mov eax, [edx]
        0x85, 0xc0, // test eax, eax
        0x75, 0x10, // jnz to function end
        0x8b, 0x4d, 0xfc, // mov ecx, [ebp-4]
        0xe8, 0, 0, 0, 0, // call code_editor_script_hint
        0xeb, 0x06, // jmp to function end
    ]);
    patch_call(0x6bb142 as _, code_editor_script_hint as _);

    // script args in code completion
    patch_call(0x6baa91 as _, completion_script_args_inj as _);
    patch(0x6baa98 as _, &[0xa8]); // get previous script name result

    // fix triggers in code completion
    patch(0x6baa1c as _, &[0x98, 0x23]); // get trigger const instead of name
    patch(0x6baa2e as _, &[
        0x6a, 0x00, // push 0
        0x6a, 0x04, // push 4
        0x8d, 0x54, 0x24, 0x4, // lea edx, [esp+4]
        0x90, 0x90, // nops
    ]);
    patch(0x6baa41 as _, &[0xb0]);

    // show number on code actions
    patch_call(0x7002fe as _, write_number_on_actions as _);

    // default room editor settings
    patch(0x657852 as _, &[0xe8, 0, 0, 0, 0, 0x90, 0x90]);
    patch_call(0x657852 as _, room_size as _);

    // nop out room view size stuff
    patch(0x657904 as _, &[0x90; 14]);
    patch(0x65791c as _, &[0x90; 14]);

    // fix instance references in creation code when renaming room
    #[rustfmt::skip]
    patch(0x692fbb as _, &[
        0xe8, 0, 0, 0, 0, // call rename_room_inj
        0x74, 0x2a, // jz to end of function
        0x90, // nop
    ]);
    patch_call(0x692fbb as _, rename_room_inj as _);

    // replace ids in new room when duplicating
    #[rustfmt::skip]
    patch(0x692e72 as _, &[
        0x8b, 0x14, 0x98, // mov edx, [eax+ebx*4]
        0x8b, 0x04, 0xb0, // mov eax, [eax+esi*4]
        0x56, // push esi (new id)
        0x53, // push ebx (old id)
        0x50, // push eax (room ptr)
        0xe8, 0x14, 0x4b, 0xfc, 0xff, // call CRoom.Assign
        0xe8, 0x00, 0x00, 0x00, 0x00, // call freshen_room_ids
        0x90, 0x90, 0x90, // nop padding
    ]);
    patch_call(0x692e80 as _, duplicate_room as _);

    // show instance id in old room editor
    patch_call(0x68fbc9 as _, show_instance_id_inj as _);

    // funky room editor shit
    patch_call(0x69319c as _, room_form_inj as _);
    // disable news (replace function with a ret)
    patch(0x62c224 as _, &[0xc3]);
    // don't open gm82room when creating/duplicating a new room
    patch_call(0x6e2f86 as _, dont_make_room_form_inj as _);
    patch_call(0x6e2f5c as _, dont_make_room_form_inj as _);
    patch_call(0x6e169e as _, dont_make_room_form_inj as _);

    // configs for default room editor settings
    // force progress bar (replace check with nops)
    patch(0x6ca266 as _, &[0x90, 0x90]);
    // replace ShowProgress with DefRoomW
    patch(0x717cbc as _, b"\x08\0\0\0D\0e\0f\0R\0o\0o\0m\0W\0\0\0");
    patch(0x719350 as _, b"\x08\0\0\0D\0e\0f\0R\0o\0o\0m\0W\0\0\0");
    // replace NoWebsite with DefRoomH
    patch(0x7189e8 as _, b"\x08\0\0\0D\0e\0f\0R\0o\0o\0m\0H\0\0\0");
    patch(0x719e0c as _, b"\x08\0\0\0D\0e\0f\0R\0o\0o\0m\0H\0\0\0");
    // replace NewsBrowser with DefRoomS
    patch(0x718a24 as _, b"\x08\0\0\0D\0e\0f\0R\0o\0o\0m\0S\0\0\0");
    patch(0x719e48 as _, b"\x08\0\0\0D\0e\0f\0R\0o\0o\0m\0S\0\0\0");
    // read ShowProgress from reg as int
    patch(0x7170f7 as _, &[0x33, 0xd2]);
    patch(0x7170fe as _, &[0xe8, 0xcd, 0xfc, 0xff, 0xff, 0xa3]);
    patch(0x717104 as _, &(&DEFAULT_ROOM_WIDTH as *const u32 as usize as u32).to_le_bytes());
    // read NoWebsite from reg as int
    patch(0x71799d as _, &[0xe8, 0x2e, 0xf4, 0xff, 0xff, 0xa3]);
    patch(0x7179a3 as _, &(&DEFAULT_ROOM_HEIGHT as *const u32 as usize as u32).to_le_bytes());
    // read NewsBrowser from reg as int
    patch(0x7179bf as _, &[0xe8, 0x0c, 0xf4, 0xff, 0xff, 0xe8]);
    patch_call(0x7179c4 as _, fix_broken_room_size as _);
    // write ShowProgress to reg as int
    patch(0x718bac as _, &[0x8b, 0x15, 0x2c, 0xa8, 0x79, 0x00, 0x90]);
    patch(0x718bb9 as _, &[0x43, 0xe0]);
    patch(0x718bae as _, &(&DEFAULT_ROOM_WIDTH as *const u32 as usize as u32).to_le_bytes());
    // write NoWebsite to reg as int
    patch(0x71908e as _, &[0x8b, 0x15, 0x81, 0xa9, 0x79, 0x00, 0x90]);
    patch(0x71909b as _, &[0x61, 0xdb]);
    patch(0x719090 as _, &(&DEFAULT_ROOM_HEIGHT as *const u32 as usize as u32).to_le_bytes());
    // write NewsBrowser to reg as int
    patch(0x7190b0 as _, &[0x8b, 0x15, 0x83, 0xa9, 0x79, 0x00, 0x90]);
    patch(0x7190bd as _, &[0x3f, 0xdb]);
    patch(0x7190b2 as _, &(&DEFAULT_ROOM_SPEED as *const u32 as usize as u32).to_le_bytes());
    // write ShowProgress to form as ValueEdit
    patch(0x71a272 as _, &[0x8b, 0x15, 0x2c, 0xa8, 0x79, 0x00, 0xe8, 0xe3, 0x64, 0xe1, 0xff, 0x90, 0x90, 0x90, 0x90]);
    patch(0x71a274 as _, &(&DEFAULT_ROOM_WIDTH as *const u32 as usize as u32).to_le_bytes());
    // write NoWebsite to form as ValueEdit
    patch(0x71a4ec as _, &[0x8b, 0x15, 0x81, 0xa9, 0x79, 0x00, 0xe8, 0x69, 0x62, 0xe1, 0xff, 0x90, 0x90, 0x90, 0x90]);
    patch(0x71a4ee as _, &(&DEFAULT_ROOM_HEIGHT as *const u32 as usize as u32).to_le_bytes());
    // write NewsBrowser to form as ValueEdit
    patch(0x71a51d as _, &[0x8b, 0x15, 0x83, 0xa9, 0x79, 0x00, 0xe8, 0x38, 0x62, 0xe1, 0xff, 0x90, 0x90, 0x90, 0x90]);
    patch(0x71a51f as _, &(&DEFAULT_ROOM_SPEED as *const u32 as usize as u32).to_le_bytes());
    // read ShowProgress from form as ValueEdit
    patch(0x71a777 as _, &[0x8b, 0x80, 0xa0, 0x02, 0x00, 0x00, 0xa3, 0x2c, 0xa8, 0x79, 0x00, 0x90, 0x90]);
    patch(0x71a77e as _, &(&DEFAULT_ROOM_WIDTH as *const u32 as usize as u32).to_le_bytes());
    // read NoWebsite from form as ValueEdit
    patch(0x71a93f as _, &[0x8b, 0x80, 0xa0, 0x02, 0x00, 0x00, 0xa3, 0x81, 0xa9, 0x79, 0x00, 0x90, 0x90]);
    patch(0x71a946 as _, &(&DEFAULT_ROOM_HEIGHT as *const u32 as usize as u32).to_le_bytes());
    // read NewsBrowser from form as ValueEdit
    patch(0x71a96b as _, &[0x8b, 0x80, 0xa0, 0x02, 0x00, 0x00, 0xa3, 0x83, 0xa9, 0x79, 0x00, 0x90, 0x90]);
    patch(0x71a972 as _, &(&DEFAULT_ROOM_SPEED as *const u32 as usize as u32).to_le_bytes());

    // check for other processes before setting MakerRunning to false
    patch(0x71af15 as _, &[0xb9]);
    patch_call(0x71af15 as _, check_gm_processes as _);

    // error box only shows once and has custom message
    patch_call(0x51fe33 as _, patch_error_box as _);

    // regenerate temp directory if it doesn't exist
    patch_call(0x5342e8 as _, regen_temp_folder_when_making_file as _);
    patch_call(0x6ce82b as _, get_temp_folder_but_also_regen_it as _);

    // add three newest resources to popup menus
    patch_call(0x71c6a0 as _, get_treenode_count_and_preserve_resource_type as _);
    patch(0x71c6dd as _, &[0xe9]);
    patch_call(0x71c6dd as _, add_three_newest_inj as _);

    // update timestamps when setting name
    unsafe fn patch_timestamps(dest: *mut u8) {
        patch(dest, &[0x8b, 0xc3, 0xe8, 0xe0, 0x00, 0x00, 0x00]);
    }
    patch(0x62cbe9 as _, &[0x8b, 0xc3, 0xe8, 0x3c, 0x01, 0x00, 0x00]); // objects
    patch_timestamps(0x6f59e1 as _); // sprites
    patch_timestamps(0x652381 as _); // sounds
    patch_timestamps(0x692fe5 as _); // rooms
    patch_timestamps(0x64def9 as _); // backgrounds
    patch_timestamps(0x655c01 as _); // scripts
    patch_timestamps(0x722901 as _); // paths
    patch_timestamps(0x6fcd19 as _); // fonts
    patch_timestamps(0x6fa6c9 as _); // timelines

    // fix objects/timelines updating the wrong timestamp
    patch_call(0x6c73ef as _, properly_update_object_timestamp_drag_drop as _);
    patch_call(0x6f94c3 as _, properly_update_timeline_timestamp_drag_drop as _);
    patch_call(0x6c7512 as _, properly_update_object_timestamp_right_click as _);
    patch_call(0x6f95e6 as _, properly_update_timeline_timestamp_right_click as _);

    // update timestamp properly in mask form
    unsafe fn patch_timestamp_mask(dest: *mut u8) {
        patch(dest, &[0xe8, 0, 0, 0, 0, 0x90, 0x90, 0x90]);
        patch_call(dest, update_sprite_mask_timestamp as _);
    }
    patch_timestamp_mask(0x6f3208 as _);
    patch_timestamp_mask(0x6f33fa as _);
    patch_timestamp_mask(0x6f34e8 as _);
    patch_timestamp_mask(0x6f3555 as _);

    // check for time going backwards
    patch(0x4199fb as _, &[0xe9]);
    patch_call(0x4199fb as _, reset_if_time_went_backwards as _);

    patch_call(0x6cd943 as _, save_exe::save_assets_inj::<asset::Sprite> as usize);
    patch_call(0x6cd95e as _, save_exe::save_assets_inj::<asset::Background> as usize);
    patch_call(0x06cd979 as _, save_exe::save_assets_inj::<asset::Path> as usize);
    patch_call(0x6cd994 as _, save_exe::save_assets_inj::<asset::Script> as usize);
    patch_call(0x6cd9af as _, save_exe::save_assets_inj::<asset::Font> as usize);
    patch_call(0x6cd9f8 as _, save_exe::save_assets_inj::<asset::Room> as usize);
}
