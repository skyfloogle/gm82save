use super::{patch, patch_call, InstanceExtra, TileExtra, EXTRA_DATA};
use std::arch::asm;

#[naked]
unsafe extern "C" fn save_82_if_exe() {
    // only saves settings version 825 when saving an exe with the creation code flag set
    asm! {
        "mov edx, 825",
        "mov ecx, 800",
        "test bl, bl", // if exe
        "cmovz edx, ecx",
        "bt word ptr [0x77f54e], 15", // if force cpu
        "cmovnc edx, ecx",
        "ret",
        options(noreturn),
    }
}

#[naked]
unsafe extern "C" fn save_bool_if_exe() {
    asm! {
        "push esi",
        "mov esi, 0x52f240", // WriteBoolean
        "mov ecx, 0x52f12c", // WriteInteger
        "test bl, bl", // if exe
        "cmovnz ecx, esi",
        "call ecx",
        "pop esi",
        "ret",
        options(noreturn),
    }
}

#[naked]
unsafe extern "C" fn save_creation_code_flag() {
    asm! {
        "mov ecx, 0x52f12c", // WriteInteger (for uninitialized args)
        "call ecx",
        "test bl, bl", // if exe
        "jz 1f",
        "bt word ptr [0x77f54e], 15", // if force cpu
        "jnc 1f",

        "mov eax, esi", // gmk stream
        "xor edx, edx", // 0 (webgl)
        "mov ecx, 0x52f12c", // WriteInteger
        "call ecx",
        "mov eax, esi", // gmk stream
        "mov dl, 1", // true (creation code)
        "mov ecx, 0x52f240", // WriteBoolean
        "call ecx",

        "1: ret",
        options(noreturn),
    }
}

#[naked]
unsafe extern "C" fn save_room_version_inj() {
    asm! {
        "mov cl, byte ptr [esp]",
        "call {}",
        "mov edx, eax",
        "mov eax, 0x658372",
        "jmp eax",
        sym save_room_version,
        options(noreturn),
    }
}

unsafe extern "fastcall" fn save_room_version(exe: bool) -> u32 {
    if exe && EXTRA_DATA.is_some() { 811 } else { 541 }
}

#[naked]
unsafe extern "C" fn save_instance_extra_inj() {
    asm! {
        "mov ecx, ebx", // file
        "mov eax, dword ptr [edi + 0x2f4]", // instance list
        "mov edx, dword ptr [eax + ebp*0x8 + 0xc]", // instance id
        "xor eax, eax",
        "mov al, byte ptr [esp]", // are we exe?
        "push eax",
        "call {}",
        "inc esi",
        "mov eax, 0x658600", // jnz of loop
        "dec dword ptr [esp + 0x4]",
        "jmp eax",
        sym save_instance_extra,
        options(noreturn),
    }
}

#[naked]
unsafe extern "C" fn save_tile_extra_inj() {
    asm! {
        "mov ecx, ebx", // file
        "mov eax, dword ptr [edi + 0x2fc]", // tile list
        "mov edx, dword ptr [eax + ebp*0x8 + 0x20]", // tile id
        "xor eax, eax",
        "mov al, byte ptr [esp]", // are we exe?
        "push eax",
        "call {}",
        "inc esi",
        "mov eax, 0x6586dd", // jnz of loop
        "dec dword ptr [esp + 0x4]",
        "jmp eax",
        sym save_tile_extra,
        options(noreturn),
    }
}

unsafe fn save_real(file: usize, real: &f64) {
    asm! {
        "push dword ptr [{real} + 0x4]",
        "push dword ptr [{real}]",
        "call {call}",
        call = in(reg) 0x52f140,
        real = in(reg) real,
        inlateout("eax") file => _,
        lateout("edx") _,
        lateout("ecx") _,
    }
}

unsafe extern "fastcall" fn save_instance_extra(file: usize, id: usize, exe: bool) {
    if exe {
        if let Some(data) = EXTRA_DATA.as_ref().map(|(insts, _)| insts.get(&id).unwrap_or(&InstanceExtra::DEFAULT)) {
            save_real(file, &data.xscale);
            save_real(file, &data.yscale);
            let _: u32 = delphi_call!(0x52f12c, file, data.blend);
            save_real(file, &data.angle);
        }
    }
}

unsafe extern "fastcall" fn save_tile_extra(file: usize, id: usize, exe: bool) {
    if exe {
        if let Some(data) = EXTRA_DATA.as_ref().map(|(_, tiles)| tiles.get(&id).unwrap_or(&TileExtra::DEFAULT)) {
            save_real(file, &data.xscale);
            save_real(file, &data.yscale);
            let _: u32 = delphi_call!(0x52f12c, file, data.blend);
        }
    }
}

pub unsafe fn inject() {
    // save creation code flag (reusing the software vertex processing flag)
    // write 825 instead of 800 for settings version if saving exe
    patch(0x70997c as _, &[0xe8]);
    patch_call(0x70997c as _, save_82_if_exe as _);
    // call WriteBoolean instead of WriteInteger if saving exe
    patch_call(0x709a4f as _, save_bool_if_exe as _);
    // save extra info if saving exe
    patch_call(0x709c99 as _, save_creation_code_flag as _);

    // save extra data on instances and tiles
    // write 811 instead of 541 for room version if saving exe
    patch(0x65836d as _, &[0xe9]);
    patch_call(0x65836d as _, save_room_version_inj as _);
    // instance stuff
    patch(0x6585fb as _, &[0xe9]);
    patch_call(0x6585fb as _, save_instance_extra_inj as _);
    // tile stuff
    patch(0x6586d8 as _, &[0xe9]);
    patch_call(0x6586d8 as _, save_tile_extra_inj as _);
}
