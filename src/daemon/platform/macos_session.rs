//! The macOS graphical login session that contains the core.

#[repr(C)]
#[derive(Default)]
struct AuditInfo {
    user: u32,
    mask: [u32; 2],
    terminal: [u32; 6],
    session: u32,
    flags: u64,
}

// SAFETY: 签名与 `<bsm/audit.h>` 的 `getaudit_addr(struct auditinfo_addr *,
// int)` 一致；`AuditInfo` 按该结构体的字段顺序与类型 #[repr(C)] 声明。
unsafe extern "C" {
    fn getaudit_addr(info: *mut AuditInfo, size: libc::c_int) -> libc::c_int;
}

pub fn graphic_session() -> std::io::Result<Option<u32>> {
    let mut info = AuditInfo::default();
    // SAFETY: `info` 是本地可写变量，指针有效且大小与传入的 length 一致，
    // 调用期间不会被移动；`getaudit_addr` 只写这块内存。
    if unsafe { getaudit_addr(&mut info, std::mem::size_of::<AuditInfo>() as libc::c_int) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok((info.flags & 0x10 != 0).then_some(info.session))
}
