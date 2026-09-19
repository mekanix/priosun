use anyhow::{bail, Result};
use libc::{c_char, c_int, c_uint, c_ulong, c_void};
use std::ffi::CString;
use std::io;
use std::mem::size_of;

const IFNAMSIZ: usize = 16;
const BRDGADD: c_ulong = 0;
const BRDGDEL: c_ulong = 1;
const IOC_IN: c_ulong = 0x8000_0000;
const IOC_OUT: c_ulong = 0x4000_0000;

const fn ioc(direction: c_ulong, number: c_ulong, size: usize) -> c_ulong {
    direction | ((size as c_ulong) << 16) | (('i' as c_ulong) << 8) | number
}

const SIOCSIFFLAGS: c_ulong = ioc(IOC_IN, 16, size_of::<libc::ifreq>());
const SIOCGIFFLAGS: c_ulong = ioc(IOC_IN | IOC_OUT, 17, size_of::<libc::ifreq>());
const SIOCSIFNAME: c_ulong = ioc(IOC_IN, 40, size_of::<libc::ifreq>());
const SIOCIFDESTROY: c_ulong = ioc(IOC_IN, 121, size_of::<libc::ifreq>());
const SIOCIFCREATE2: c_ulong = ioc(IOC_IN | IOC_OUT, 124, size_of::<libc::ifreq>());
const SIOCAIFGROUP: c_ulong = ioc(IOC_IN, 135, size_of::<Ifgroupreq>());
const SIOCSDRVSPEC: c_ulong = ioc(IOC_IN, 123, size_of::<libc::ifdrv>());
const SIOCSIFVNET: c_ulong = ioc(IOC_IN | IOC_OUT, 90, size_of::<libc::ifreq>());

#[repr(C)]
union IfgroupUnion {
    group: [c_char; IFNAMSIZ],
    groups: *mut c_void,
}

#[repr(C)]
struct Ifgroupreq {
    name: [c_char; IFNAMSIZ],
    len: c_uint,
    data: IfgroupUnion,
}

#[repr(C)]
struct Ifbreq {
    member: [c_char; IFNAMSIZ],
    ifs_flags: u32,
    stp_flags: u32,
    path_cost: u32,
    port_no: u8,
    priority: u8,
    protocol: u8,
    role: u8,
    state: u8,
    addr_count: u32,
    addr_max: u32,
    addr_exceeded: u32,
    pvid: u16,
    vlan_protocol: u16,
    pad: [u8; 28],
}

fn interface_name(destination: &mut [c_char; IFNAMSIZ], name: &str) -> Result<()> {
    let name = CString::new(name)?;
    let bytes = name.as_bytes_with_nul();
    if bytes.len() > destination.len() {
        bail!("interface name is too long");
    }
    for (destination, source) in destination.iter_mut().zip(bytes) {
        *destination = *source as c_char;
    }
    Ok(())
}

fn read_interface_name(name: &[c_char; IFNAMSIZ]) -> Result<String> {
    let length = name
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(name.len());
    Ok(String::from_utf8(
        name[..length].iter().map(|byte| *byte as u8).collect(),
    )?)
}

fn control_socket() -> Result<c_int> {
    let fd = unsafe { libc::socket(libc::AF_LOCAL, libc::SOCK_DGRAM, 0) };
    if fd < 0 {
        Err(io::Error::last_os_error().into())
    } else {
        Ok(fd)
    }
}

fn ioctl(fd: c_int, request: c_ulong, argument: *mut c_void, operation: &str) -> Result<()> {
    if unsafe { libc::ioctl(fd, request, argument) } < 0 {
        bail!("{operation}: {}", io::Error::last_os_error());
    }
    Ok(())
}

pub fn create(kind: &str) -> Result<String> {
    let fd = control_socket()?;
    let mut request = unsafe { std::mem::zeroed::<libc::ifreq>() };
    interface_name(&mut request.ifr_name, kind)?;
    let result = ioctl(
        fd,
        SIOCIFCREATE2,
        &mut request as *mut _ as *mut c_void,
        "create interface",
    );
    unsafe { libc::close(fd) };
    result?;
    read_interface_name(&request.ifr_name)
}

pub fn destroy(name: &str) -> Result<()> {
    let fd = control_socket()?;
    let mut request = unsafe { std::mem::zeroed::<libc::ifreq>() };
    interface_name(&mut request.ifr_name, name)?;
    let result = ioctl(
        fd,
        SIOCIFDESTROY,
        &mut request as *mut _ as *mut c_void,
        "destroy interface",
    );
    unsafe { libc::close(fd) };
    result
}

pub fn set_up(name: &str) -> Result<()> {
    let fd = control_socket()?;
    let mut request = unsafe { std::mem::zeroed::<libc::ifreq>() };
    interface_name(&mut request.ifr_name, name)?;
    let result = (|| {
        ioctl(
            fd,
            SIOCGIFFLAGS,
            &mut request as *mut _ as *mut c_void,
            "get interface flags",
        )?;
        unsafe {
            request.ifr_ifru.ifru_flags[0] |= libc::IFF_UP as i16;
        }
        ioctl(
            fd,
            SIOCSIFFLAGS,
            &mut request as *mut _ as *mut c_void,
            "set interface flags",
        )
    })();
    unsafe { libc::close(fd) };
    result
}

pub fn group_add(name: &str, group: &str) -> Result<()> {
    let fd = control_socket()?;
    let mut request = unsafe { std::mem::zeroed::<Ifgroupreq>() };
    interface_name(&mut request.name, name)?;
    interface_name(unsafe { &mut request.data.group }, group)?;
    let result = ioctl(
        fd,
        SIOCAIFGROUP,
        &mut request as *mut _ as *mut c_void,
        "add interface group",
    );
    unsafe { libc::close(fd) };
    result
}

fn bridge_member(bridge: &str, member: &str, request: c_ulong) -> Result<()> {
    let fd = control_socket()?;
    let mut member_request = unsafe { std::mem::zeroed::<Ifbreq>() };
    interface_name(&mut member_request.member, member)?;
    let mut driver_request = unsafe { std::mem::zeroed::<libc::ifdrv>() };
    interface_name(&mut driver_request.ifd_name, bridge)?;
    driver_request.ifd_cmd = request;
    driver_request.ifd_len = size_of::<Ifbreq>();
    driver_request.ifd_data = &mut member_request as *mut _ as *mut c_void;
    let result = ioctl(
        fd,
        SIOCSDRVSPEC,
        &mut driver_request as *mut _ as *mut c_void,
        "change bridge membership",
    );
    unsafe { libc::close(fd) };
    result
}

pub fn bridge_add(bridge: &str, member: &str) -> Result<()> {
    bridge_member(bridge, member, BRDGADD)
}

pub fn bridge_delete(bridge: &str, member: &str) -> Result<()> {
    bridge_member(bridge, member, BRDGDEL)
}

pub fn move_to_vnet(name: &str, jid: i32) -> Result<()> {
    let fd = control_socket()?;
    let mut request = unsafe { std::mem::zeroed::<libc::ifreq>() };
    interface_name(&mut request.ifr_name, name)?;
    request.ifr_ifru.ifru_jid = jid;
    let result = ioctl(
        fd,
        SIOCSIFVNET,
        &mut request as *mut _ as *mut c_void,
        "move interface to jail",
    );
    unsafe { libc::close(fd) };
    result
}

fn rename(name: &str, new_name: &str) -> Result<()> {
    let fd = control_socket()?;
    let mut request = unsafe { std::mem::zeroed::<libc::ifreq>() };
    let mut replacement = [0_i8; IFNAMSIZ];
    interface_name(&mut request.ifr_name, name)?;
    interface_name(&mut replacement, new_name)?;
    request.ifr_ifru.ifru_data = replacement.as_mut_ptr().cast();
    let result = ioctl(
        fd,
        SIOCSIFNAME,
        &mut request as *mut _ as *mut c_void,
        "rename interface",
    );
    unsafe { libc::close(fd) };
    result
}

pub fn rename_in_jail(jid: i32, name: &str, new_name: &str) -> Result<()> {
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        bail!("fork: {}", io::Error::last_os_error());
    }
    if pid == 0 {
        if unsafe { libc::jail_attach(jid) } < 0 {
            unsafe { libc::_exit(libc::EIO) };
        }
        match rename(name, new_name) {
            Ok(()) => unsafe { libc::_exit(0) },
            Err(_) => unsafe { libc::_exit(libc::EIO) },
        }
    }
    let mut status = 0;
    if unsafe { libc::waitpid(pid, &mut status, 0) } < 0 {
        bail!("wait for interface rename: {}", io::Error::last_os_error());
    }
    if libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0 {
        Ok(())
    } else {
        bail!("rename interface in jail failed")
    }
}
